//! Transactional placement of a job's result: the main output and all
//! sidecar files are published together or not at all.
//!
//! Phases (each recorded in a journal file *before* it changes anything):
//!
//! 1. **Staging** – every file is moved (same file system) or copied and
//!    synced (across file systems) next to its target as
//!    `.framewise-job<ID>-<name>.partial`. From here on every placement is a
//!    rename/link inside one directory and therefore atomic.
//! 2. **Publishing** – existing targets the user allowed to replace are
//!    renamed to `.framewise-job<ID>-<name>.bak` (never deleted yet), then each
//!    staged file is linked to its target without clobbering (hard link,
//!    or an existence check + rename on file systems without links).
//!    Afterwards every target must exist with the expected size.
//! 3. **Committed** – the commit point. Only now are backups and staged
//!    leftovers removed.
//!
//! Any error before the commit point rolls back: files this job published
//! are removed, backups are renamed back and staged files return to the work
//! directory (so a validated result can still be quarantined). A crash leaves
//! the journal behind; [`recover`] then completes the transaction if every
//! file is still available and otherwise rolls it back. The journal is only
//! deleted once the job state has been saved ([`release`]).
//!
//! All file operations go through [`FsOps`], so tests can inject an error or
//! a "crash" at every single step.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::encoding::quality::QualityResult;
use crate::encoding::validate::ValidationReport;

/// File system operations used by the transaction.
pub trait FsOps: Sync {
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Fails with `AlreadyExists` if `to` exists (never replaces).
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Copy and flush to disk.
    fn copy_synced(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Create/replace `path` with `data` and flush it to disk.
    fn write_synced(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Persist directory entries (rename/link) – best effort.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    fn len(&self, path: &Path) -> io::Result<u64>;
    fn exists(&self, path: &Path) -> bool;
    /// `a` and `b` are the same file (hard links of one inode).
    fn same_file(&self, a: &Path, b: &Path) -> bool;
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl FsOps for RealFs {
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }

    fn copy_synced(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::copy(from, to)?;
        std::fs::File::open(to)?.sync_all()
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn write_synced(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        f.write_all(data)?;
        f.sync_all()
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::fs::File::open(dir)?.sync_all()
        }
        #[cfg(not(unix))]
        {
            let _ = dir; // directory handles cannot be synced portably
            Ok(())
        }
    }

    fn len(&self, path: &Path) -> io::Result<u64> {
        std::fs::symlink_metadata(path).map(|m| m.len())
    }

    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    fn same_file(&self, a: &Path, b: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            match (std::fs::metadata(a), std::fs::metadata(b)) {
                (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            same_content(a, b).unwrap_or(false)
        }
    }
}

/// Fallback without inode numbers: equal size and equal head/tail bytes.
#[cfg(not(unix))]
fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    const PART: u64 = 1 << 20;
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let len = fa.metadata()?.len();
    if len != fb.metadata()?.len() {
        return Ok(false);
    }
    for start in [0, len.saturating_sub(PART)] {
        let n = PART.min(len) as usize;
        let (mut ba, mut bb) = (vec![0; n], vec![0; n]);
        fa.seek(SeekFrom::Start(start))?;
        fb.seek(SeekFrom::Start(start))?;
        fa.read_exact(&mut ba)?;
        fb.read_exact(&mut bb)?;
        if ba != bb {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileRole {
    Main,
    Sidecar,
}

/// One file to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub role: FileRole,
    /// Finished file in the job's work directory.
    pub source: PathBuf,
    pub target: PathBuf,
    /// An existing `target` may be replaced (only after the commit point is
    /// the old file deleted).
    pub replace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Staging,
    Publishing,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalFile {
    pub role: FileRole,
    pub source: PathBuf,
    pub staged: PathBuf,
    pub target: PathBuf,
    pub replace: bool,
    /// Where the replaced target is parked until the commit point.
    pub backup: Option<PathBuf>,
    pub size: u64,
}

/// What the job produced – kept so a transaction completed during recovery
/// can still be recorded as a finished job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobResult {
    pub result_size: Option<u64>,
    pub quality: Option<QualityResult>,
    pub validation: Option<ValidationReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    pub job_id: i64,
    pub phase: Phase,
    pub files: Vec<JournalFile>,
    pub result: JobResult,
}

impl Journal {
    pub fn target(&self) -> Option<&Path> {
        self.files
            .iter()
            .find(|f| f.role == FileRole::Main)
            .map(|f| f.target.as_path())
    }

    pub fn sidecars(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|f| f.role == FileRole::Sidecar)
            .map(|f| f.target.clone())
            .collect()
    }
}

/// Why a transaction did not complete.
#[derive(Debug)]
pub struct FinalizeError {
    pub error: io::Error,
    /// The path the failing operation was about.
    pub path: PathBuf,
    /// Everything was undone; the journal is gone. If `false`, the journal is
    /// kept and [`recover`] retries at the next start.
    pub rolled_back: bool,
}

impl FinalizeError {
    pub fn is_conflict(&self) -> bool {
        self.error.kind() == io::ErrorKind::AlreadyExists
    }
}

impl std::fmt::Display for FinalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}

/// Result of [`recover`].
#[derive(Debug)]
pub enum Recovery {
    /// All files are in place; the journal is kept until the job is saved.
    Completed(Journal),
    /// Nothing of the job is left in the target; journal removed.
    RolledBack { journal: Journal, reason: String },
    /// Neither completed nor undone – the journal stays for the next start.
    Incomplete { job_id: i64, error: String },
}

pub fn journal_dir(work_root: &Path) -> PathBuf {
    work_root.join("journal")
}

pub fn journal_path(work_root: &Path, job_id: i64) -> PathBuf {
    journal_dir(work_root).join(format!("job-{job_id}.json"))
}

/// Journals left behind by an interrupted finalization, sorted by job id.
pub fn pending(work_root: &Path) -> Vec<(i64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(journal_dir(work_root)) else {
        return Vec::new();
    };
    let mut found: Vec<(i64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name
                .strip_prefix("job-")?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            Some((id, e.path()))
        })
        .collect();
    found.sort();
    found
}

/// Last step once the finished job has been saved: remove leftovers of a
/// committed transaction and its journal. An uncommitted or unreadable
/// journal is kept for [`recover`].
pub fn release(fs: &dyn FsOps, journal_path: &Path) {
    match read_journal(journal_path) {
        Ok(j) if j.phase == Phase::Committed => {
            cleanup(fs, &j);
            if let Err(e) = fs.remove_file(journal_path) {
                tracing::warn!("could not remove journal {}: {e}", journal_path.display());
            }
        }
        Ok(j) => tracing::warn!(
            "journal {} not committed ({:?}) – kept",
            journal_path.display(),
            j.phase
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("journal {}: {e} – kept", journal_path.display()),
    }
}

fn hidden_sibling(target: &Path, job_id: i64, suffix: &str) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".framewise-job{job_id}-{name}.{suffix}"))
}

fn write_journal(fs: &dyn FsOps, path: &Path, journal: &Journal) -> io::Result<()> {
    let data = serde_json::to_vec_pretty(journal).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs.write_synced(&tmp, &data)?;
    fs.rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        let _ = fs.sync_dir(dir);
    }
    Ok(())
}

fn read_journal(path: &Path) -> io::Result<Journal> {
    let data = std::fs::read(path)?;
    serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn sync_parent(fs: &dyn FsOps, path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fs.sync_dir(dir);
    }
}

/// The target holds this job's file. Only meaningful once staging is done:
/// either it is a link of the staged file, or the staged file was renamed
/// onto it (staged gone, size matches).
fn is_published(fs: &dyn FsOps, f: &JournalFile) -> bool {
    fs.exists(&f.target)
        && (fs.same_file(&f.staged, &f.target)
            || (!fs.exists(&f.staged) && fs.len(&f.target).ok() == Some(f.size)))
}

type StepResult = std::result::Result<(), (io::Error, PathBuf)>;

fn at(path: &Path) -> impl FnOnce(io::Error) -> (io::Error, PathBuf) + '_ {
    move |e| (e, path.to_path_buf())
}

/// Idempotent: a complete staged file is kept, a partial one is redone.
fn stage(fs: &dyn FsOps, f: &JournalFile) -> StepResult {
    if !fs.exists(&f.source) {
        return match fs.len(&f.staged) {
            Ok(n) if n == f.size => Ok(()),
            _ => Err((io::Error::from(io::ErrorKind::NotFound), f.source.clone())),
        };
    }
    if fs.exists(&f.staged) {
        fs.remove_file(&f.staged).map_err(at(&f.staged))?;
    }
    if let Some(dir) = f.staged.parent() {
        fs.create_dir_all(dir).map_err(at(dir))?;
    }
    match fs.rename(&f.source, &f.staged) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            fs.copy_synced(&f.source, &f.staged)
                .map_err(at(&f.staged))?;
        }
        Err(e) => return Err((e, f.staged.clone())),
    }
    if fs.len(&f.staged).map_err(at(&f.staged))? != f.size {
        return Err((
            io::Error::new(io::ErrorKind::InvalidData, "staged file is incomplete"),
            f.staged.clone(),
        ));
    }
    sync_parent(fs, &f.staged);
    Ok(())
}

/// Idempotent: an already published file is left alone.
fn publish(fs: &dyn FsOps, f: &JournalFile) -> StepResult {
    if is_published(fs, f) {
        return Ok(());
    }
    if let Some(backup) = &f.backup
        && fs.exists(&f.target)
    {
        if fs.exists(backup) {
            // Old file already parked, but the target was recreated meanwhile.
            return Err((
                io::Error::from(io::ErrorKind::AlreadyExists),
                f.target.clone(),
            ));
        }
        fs.rename(&f.target, backup).map_err(at(&f.target))?;
        sync_parent(fs, backup);
    }
    match fs.hard_link(&f.staged, &f.target) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err((e, f.target.clone())),
        Err(_) => {
            // No hard links (FAT, some network shares): check, then rename.
            if fs.exists(&f.target) {
                return Err((
                    io::Error::from(io::ErrorKind::AlreadyExists),
                    f.target.clone(),
                ));
            }
            fs.rename(&f.staged, &f.target).map_err(at(&f.target))?;
        }
    }
    sync_parent(fs, &f.target);
    Ok(())
}

fn verify(fs: &dyn FsOps, files: &[JournalFile]) -> StepResult {
    for f in files {
        match fs.len(&f.target) {
            Ok(n) if n == f.size => {}
            Ok(n) => {
                return Err((
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("size {n} instead of {}", f.size),
                    ),
                    f.target.clone(),
                ));
            }
            Err(e) => return Err((e, f.target.clone())),
        }
    }
    Ok(())
}

/// Runs the remaining phases of `journal` up to and including the commit.
fn drive(fs: &dyn FsOps, path: &Path, journal: &mut Journal) -> StepResult {
    if journal.phase == Phase::Staging {
        for f in &journal.files {
            stage(fs, f)?;
        }
        let mut next = journal.clone();
        for f in &mut next.files {
            if f.replace && fs.exists(&f.target) {
                f.backup = Some(hidden_sibling(&f.target, next.job_id, "bak"));
            }
        }
        next.phase = Phase::Publishing;
        advance(fs, path, journal, next)?;
    }
    if journal.phase == Phase::Publishing {
        for f in &journal.files {
            publish(fs, f)?;
        }
        verify(fs, &journal.files)?;
        let mut next = journal.clone();
        next.phase = Phase::Committed;
        advance(fs, path, journal, next)?;
    }
    Ok(())
}

/// The in-memory state only moves on once the journal on disk says so –
/// a failed write must be rolled back from the phase that is recorded.
fn advance(fs: &dyn FsOps, path: &Path, journal: &mut Journal, next: Journal) -> StepResult {
    write_journal(fs, path, &next).map_err(at(path))?;
    *journal = next;
    Ok(())
}

/// Removes staged links and backups after the commit point (best effort:
/// leftovers are hidden files, never part of the result).
fn cleanup(fs: &dyn FsOps, journal: &Journal) {
    for f in &journal.files {
        for leftover in std::iter::once(&f.staged).chain(f.backup.as_ref()) {
            if fs.exists(leftover)
                && let Err(e) = fs.remove_file(leftover)
            {
                tracing::warn!("could not remove {}: {e}", leftover.display());
            }
        }
    }
}

/// Undo an uncommitted transaction. With `restore_sources` the files return
/// to the work directory (still running), otherwise they are deleted
/// (recovery after a crash).
fn rollback(fs: &dyn FsOps, journal: &Journal, restore_sources: bool) -> io::Result<()> {
    let mut first_error = None;
    let mut note = |r: io::Result<()>| {
        if let Err(e) = r {
            tracing::warn!("rollback step failed: {e}");
            first_error.get_or_insert(e);
        }
    };
    let keep = |f: &JournalFile| restore_sources && !fs.exists(&f.source);
    for f in journal.files.iter().rev() {
        if journal.phase == Phase::Publishing && is_published(fs, f) {
            if !fs.exists(&f.staged) && keep(f) {
                note(fs.rename(&f.target, &f.source));
            } else {
                note(fs.remove_file(&f.target));
            }
        }
        if let Some(backup) = &f.backup
            && fs.exists(backup)
            && !fs.exists(&f.target)
        {
            note(fs.rename(backup, &f.target));
        }
        if fs.exists(&f.staged) {
            if keep(f) {
                note(fs.rename(&f.staged, &f.source));
            } else {
                note(fs.remove_file(&f.staged));
            }
        }
        sync_parent(fs, &f.target);
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Publish all `files` as one transaction. On success the journal (phase
/// `Committed`) is returned and stays on disk until [`release`].
pub fn finalize(
    fs: &dyn FsOps,
    journal_path: &Path,
    job_id: i64,
    files: Vec<PlannedFile>,
    result: JobResult,
) -> std::result::Result<Journal, FinalizeError> {
    let fail = |error: io::Error, path: &Path, rolled_back| FinalizeError {
        error,
        path: path.to_path_buf(),
        rolled_back,
    };
    // Preflight: nothing has been touched yet.
    let mut entries = Vec::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        if files[..i].iter().any(|o| o.target == f.target) {
            let e = io::Error::new(io::ErrorKind::InvalidInput, "target listed twice");
            return Err(fail(e, &f.target, true));
        }
        if !f.replace && fs.exists(&f.target) {
            return Err(fail(io::ErrorKind::AlreadyExists.into(), &f.target, true));
        }
        let size = fs.len(&f.source).map_err(|e| fail(e, &f.source, true))?;
        entries.push(JournalFile {
            role: f.role,
            source: f.source.clone(),
            staged: hidden_sibling(&f.target, job_id, "partial"),
            target: f.target.clone(),
            replace: f.replace,
            backup: None,
            size,
        });
    }
    let mut journal = Journal {
        job_id,
        phase: Phase::Staging,
        files: entries,
        result,
    };
    if let Some(dir) = journal_path.parent() {
        fs.create_dir_all(dir).map_err(|e| fail(e, dir, true))?;
    }
    write_journal(fs, journal_path, &journal).map_err(|e| fail(e, journal_path, true))?;

    match drive(fs, journal_path, &mut journal) {
        Ok(()) => {
            cleanup(fs, &journal);
            Ok(journal)
        }
        Err((error, path)) => {
            let rolled_back = rollback(fs, &journal, true).is_ok();
            if rolled_back {
                let _ = fs.remove_file(journal_path);
            }
            Err(FinalizeError {
                error,
                path,
                rolled_back,
            })
        }
    }
}

/// Finish or undo the transaction recorded in `journal_path` (startup).
pub fn recover(fs: &dyn FsOps, journal_path: &Path, job_id: i64) -> Recovery {
    let mut journal = match read_journal(journal_path) {
        Ok(j) => j,
        Err(e) => {
            return Recovery::Incomplete {
                job_id,
                error: format!("journal {} unreadable: {e}", journal_path.display()),
            };
        }
    };
    match drive(fs, journal_path, &mut journal) {
        Ok(()) => {
            cleanup(fs, &journal);
            Recovery::Completed(journal)
        }
        Err((error, path)) => {
            let reason = format!("{}: {error}", path.display());
            match rollback(fs, &journal, false) {
                Ok(()) => {
                    let _ = fs.remove_file(journal_path);
                    Recovery::RolledBack { journal, reason }
                }
                Err(e) => Recovery::Incomplete {
                    job_id: journal.job_id,
                    error: format!("{reason}; rollback failed: {e}"),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests;
