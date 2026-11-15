//! Fault injection: every file operation of the transaction is made to fail
//! (or the "process" to crash) once, and the result must always be either
//! the complete new state or the untouched old state.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// The operation with this index fails, everything else works.
    Fail(io::ErrorKind),
    /// From this operation on nothing is executed any more (power loss);
    /// the interrupted operation leaves half a file behind.
    Crash,
}

struct FaultyFs {
    real: RealFs,
    ops: AtomicUsize,
    at: usize,
    fault: Fault,
    /// File system without hard links (FAT, some SMB shares).
    no_links: bool,
    /// Work directory and target on different file systems.
    cross_device: bool,
    /// Another process creates this file right before the first link.
    intrude: Mutex<Option<PathBuf>>,
}

impl FaultyFs {
    fn new(fault: Fault, at: usize) -> Self {
        Self {
            real: RealFs,
            ops: AtomicUsize::new(0),
            at,
            fault,
            no_links: false,
            cross_device: false,
            intrude: Mutex::new(None),
        }
    }

    /// Counts a mutating operation; `Err` = do not execute it.
    fn step(&self) -> io::Result<()> {
        let n = self.ops.fetch_add(1, Ordering::SeqCst);
        match self.fault {
            Fault::Fail(kind) if n == self.at => Err(kind.into()),
            Fault::Crash if n >= self.at => Err(io::Error::other("crashed")),
            _ => Ok(()),
        }
    }

    fn crashes_now(&self) -> bool {
        self.fault == Fault::Crash && self.ops.load(Ordering::SeqCst) == self.at + 1
    }

    /// The fault was reached during the run.
    fn hit(&self) -> bool {
        self.ops.load(Ordering::SeqCst) > self.at
    }
}

impl FsOps for FaultyFs {
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.step()?;
        self.real.create_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.step()?;
        let work_to_target = from.components().any(|c| c.as_os_str() == "work")
            != to.components().any(|c| c.as_os_str() == "work");
        if self.cross_device && work_to_target {
            return Err(io::ErrorKind::CrossesDevices.into());
        }
        self.real.rename(from, to)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.step()?;
        if let Some(p) = self.intrude.lock().unwrap().take() {
            std::fs::write(p, b"FOREIGN").unwrap();
        }
        if self.no_links {
            return Err(io::ErrorKind::Unsupported.into());
        }
        self.real.hard_link(from, to)
    }

    fn copy_synced(&self, from: &Path, to: &Path) -> io::Result<()> {
        if let Err(e) = self.step() {
            if self.crashes_now() {
                let data = std::fs::read(from)?;
                std::fs::write(to, &data[..data.len() / 2])?;
            }
            return Err(e);
        }
        std::fs::copy(from, to).map(|_| ()) // no fsync: speed only
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.step()?;
        self.real.remove_file(path)
    }

    fn write_synced(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if let Err(e) = self.step() {
            if self.crashes_now() {
                std::fs::write(path, &data[..data.len() / 2])?;
            }
            return Err(e);
        }
        std::fs::write(path, data)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.step()?;
        let _ = dir;
        Ok(())
    }

    fn len(&self, path: &Path) -> io::Result<u64> {
        self.real.len(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.real.exists(path)
    }

    fn same_file(&self, a: &Path, b: &Path) -> bool {
        self.real.same_file(a, b)
    }
}

type Snapshot = BTreeMap<String, Vec<u8>>;

struct Scene {
    _dir: tempfile::TempDir,
    work: PathBuf,
    lib: PathBuf,
    journal: PathBuf,
    files: Vec<PlannedFile>,
    sources: Snapshot,
}

const NEW: [(&str, &str, &[u8]); 3] = [
    ("out.mp4", "Film.mp4", b"NEW main output with some bytes"),
    ("sidecar-0.srt", "Film.ger.srt", b"NEW subtitles"),
    (
        "sidecar-1.sup",
        "Film.ger.forced.sup",
        b"NEW forced bitmap subtitles",
    ),
];

fn scene(existing: bool, replace: bool) -> Scene {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work/job-1");
    let lib = dir.path().join("lib/Film");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    let mut files = Vec::new();
    let mut sources = Snapshot::new();
    for (i, (src, dst, data)) in NEW.iter().enumerate() {
        std::fs::write(work.join(src), data).unwrap();
        sources.insert((*src).to_string(), data.to_vec());
        if existing {
            std::fs::write(lib.join(dst), format!("OLD {dst}")).unwrap();
        }
        files.push(PlannedFile {
            role: if i == 0 {
                FileRole::Main
            } else {
                FileRole::Sidecar
            },
            source: work.join(src),
            target: lib.join(dst),
            replace,
        });
    }
    std::fs::write(lib.join("Other.mp4"), b"unrelated").unwrap();
    Scene {
        journal: journal_path(&dir.path().join("work"), 1),
        _dir: dir,
        work,
        lib,
        files,
        sources,
    }
}

fn snapshot(dir: &Path) -> Snapshot {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

fn expected_after(before: &Snapshot) -> Snapshot {
    let mut after = before.clone();
    for (_, dst, data) in NEW {
        after.insert(dst.to_string(), data.to_vec());
    }
    after
}

fn release(path: &Path) {
    super::release(&RealFs, path);
    assert!(!path.exists());
}

/// Every operation fails once / crashes once, for all file system variants.
#[test]
fn every_fault_ends_complete_or_untouched() {
    for (existing, replace) in [(false, false), (true, true)] {
        for (no_links, cross_device) in [(false, false), (true, false), (false, true), (true, true)]
        {
            for fault in [Fault::Fail(io::ErrorKind::StorageFull), Fault::Crash] {
                let mut at = 0;
                loop {
                    let s = scene(existing, replace);
                    let before = snapshot(&s.lib);
                    let after = expected_after(&before);
                    let mut fs = FaultyFs::new(fault, at);
                    fs.no_links = no_links;
                    fs.cross_device = cross_device;
                    let ctx = format!(
                        "existing={existing} links={} xdev={cross_device} {fault:?} at {at}",
                        !no_links
                    );
                    let res = finalize(&fs, &s.journal, 1, s.files.clone(), JobResult::default());

                    match (&res, fault) {
                        (Ok(j), _) => {
                            assert_eq!(j.phase, Phase::Committed, "{ctx}");
                            release(&s.journal);
                            assert_eq!(snapshot(&s.lib), after, "{ctx}");
                        }
                        (Err(e), Fault::Fail(_)) => {
                            assert!(e.rolled_back, "{ctx}: {e}");
                            assert!(!s.journal.exists(), "{ctx}");
                            assert_eq!(snapshot(&s.lib), before, "{ctx}");
                            // The validated result is still available for quarantine.
                            assert_eq!(snapshot(&s.work), s.sources, "{ctx}");
                        }
                        (Err(_), _) => {
                            if s.journal.exists() {
                                // Restart: the transaction must be completed.
                                match recover(&RealFs, &s.journal, 1) {
                                    Recovery::Completed(j) => assert_eq!(j.phase, Phase::Committed),
                                    other => panic!("{ctx}: {other:?}"),
                                }
                                release(&s.journal);
                                assert_eq!(snapshot(&s.lib), after, "{ctx}");
                            } else {
                                assert_eq!(snapshot(&s.lib), before, "{ctx}");
                            }
                        }
                    }
                    if !fs.hit() {
                        assert!(res.is_ok(), "{ctx}");
                        break;
                    }
                    at += 1;
                    assert!(at < 200, "{ctx}: no end");
                }
            }
        }
    }
}

#[test]
fn existing_target_is_never_clobbered() {
    let s = scene(true, false);
    let before = snapshot(&s.lib);
    let err = finalize(
        &RealFs,
        &s.journal,
        1,
        s.files.clone(),
        JobResult::default(),
    )
    .unwrap_err();
    assert!(err.is_conflict());
    assert!(err.rolled_back);
    assert_eq!(snapshot(&s.lib), before);
    assert_eq!(snapshot(&s.work), s.sources);
    assert!(!s.journal.exists());
}

#[test]
fn file_created_during_finalization_is_kept() {
    for no_links in [false, true] {
        let s = scene(false, false);
        let before = snapshot(&s.lib);
        let mut fs = FaultyFs::new(Fault::None, usize::MAX);
        fs.no_links = no_links;
        let foreign = s.lib.join("Film.ger.srt");
        *fs.intrude.lock().unwrap() = Some(foreign.clone());
        let err = finalize(&fs, &s.journal, 1, s.files.clone(), JobResult::default()).unwrap_err();
        assert!(err.is_conflict(), "{err}");
        assert!(err.rolled_back);
        let mut expected = before.clone();
        expected.insert("Film.ger.srt".into(), b"FOREIGN".to_vec());
        assert_eq!(snapshot(&s.lib), expected, "links={}", !no_links);
        assert_eq!(snapshot(&s.work), s.sources);
    }
}

#[test]
fn replace_keeps_old_files_until_commit() {
    let s = scene(true, true);
    let after = expected_after(&snapshot(&s.lib));
    let j = finalize(
        &RealFs,
        &s.journal,
        1,
        s.files.clone(),
        JobResult::default(),
    )
    .unwrap();
    assert_eq!(j.target(), Some(s.lib.join("Film.mp4").as_path()));
    assert_eq!(j.sidecars().len(), 2);
    release(&s.journal);
    assert_eq!(snapshot(&s.lib), after);
    assert_eq!(
        std::fs::read(s.lib.join("Other.mp4")).unwrap(),
        b"unrelated"
    );
}

#[test]
fn unreadable_journal_is_kept_for_inspection() {
    let s = scene(false, false);
    std::fs::create_dir_all(s.journal.parent().unwrap()).unwrap();
    std::fs::write(&s.journal, b"{ broken").unwrap();
    assert!(matches!(
        recover(&RealFs, &s.journal, 1),
        Recovery::Incomplete { job_id: 1, .. }
    ));
    assert!(s.journal.exists());
    let root = s.journal.parent().unwrap().parent().unwrap();
    assert_eq!(pending(root), vec![(1, s.journal.clone())]);
}

#[test]
fn recovery_rolls_back_when_the_result_is_gone() {
    let s = scene(false, false);
    // Crash right after the journal was written (before staging finished).
    let fs = FaultyFs::new(Fault::Crash, 4);
    assert!(finalize(&fs, &s.journal, 1, s.files.clone(), JobResult::default()).is_err());
    assert!(s.journal.exists());
    // The work directory was cleaned up meanwhile.
    std::fs::remove_dir_all(&s.work).unwrap();
    let before = snapshot(&s.lib);
    match recover(&RealFs, &s.journal, 1) {
        Recovery::RolledBack { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(!s.journal.exists());
    let mut clean = before.clone();
    clean.retain(|k, _| !k.starts_with(".framewise"));
    assert_eq!(snapshot(&s.lib), clean);
}
