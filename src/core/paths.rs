//! Source/target directory validation, target path mapping and naming
//! conflict handling.
//!
//! Safety rules enforced here:
//! * source and target must differ,
//! * the target must not be inside the source (no re-processing of our own output),
//! * existing files are never overwritten implicitly.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::{Error, Result};
use crate::{tr, trf};

/// What to do if the target file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConflictStrategy {
    /// Do not process the file (default, safest).
    #[default]
    Skip,
    /// Choose a new name like `Movie (1).mp4`.
    Rename,
    /// Replace an existing *result* after explicit confirmation. The old file
    /// is only replaced once the new one has been validated.
    ReplaceAfterConfirmation,
}

impl ConflictStrategy {
    pub const ALL: [ConflictStrategy; 3] = [
        ConflictStrategy::Skip,
        ConflictStrategy::Rename,
        ConflictStrategy::ReplaceAfterConfirmation,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ConflictStrategy::Skip => tr!("Skip"),
            ConflictStrategy::Rename => tr!("Choose a new file name"),
            ConflictStrategy::ReplaceAfterConfirmation => {
                tr!("Replace after confirmation")
            }
        }
    }
}

/// Canonical, validated source and target roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub source: PathBuf,
    pub target: PathBuf,
}

/// Resolve `path` as far as it exists (canonicalize the deepest existing
/// ancestor, append the rest). Rejects `..` in the non-existing part.
pub fn normalize_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| Error::io(path, e))?
            .join(path)
    };

    let mut existing = absolute.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent;
            }
            _ => break,
        }
    }
    let mut result = existing
        .canonicalize()
        .map_err(|e| Error::io(existing, e))?;
    for name in rest.into_iter().rev() {
        if Path::new(name)
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(Error::InvalidPaths(trf!(
                "Path contains invalid components: {}",
                path.display()
            )));
        }
        result.push(name);
    }
    Ok(result)
}

/// Validate a source/target pair.
pub fn validate_roots(source: &Path, target: &Path) -> Result<Roots> {
    if source.as_os_str().is_empty() || target.as_os_str().is_empty() {
        return Err(Error::InvalidPaths(
            tr!("Source and target directory are required").into(),
        ));
    }
    let source = source.canonicalize().map_err(|e| Error::io(source, e))?;
    if !source.is_dir() {
        return Err(Error::InvalidPaths(trf!(
            "Source is not a directory: {}",
            source.display()
        )));
    }
    let target = normalize_path(target)?;
    if target.exists() && !target.is_dir() {
        return Err(Error::InvalidPaths(trf!(
            "Target is not a directory: {}",
            target.display()
        )));
    }
    if target == source {
        return Err(Error::InvalidPaths(
            tr!("Source and target must not be identical").into(),
        ));
    }
    if target.starts_with(&source) {
        return Err(Error::InvalidPaths(trf!(
            "The target '{}' is inside the source '{}' – the output would be processed again",
            target.display(),
            source.display()
        )));
    }
    Ok(Roots { source, target })
}

/// Map a source-relative path to its target path: same folder structure and
/// file name, extension replaced by `.mp4`.
pub fn target_for_relative(target_root: &Path, relative: &Path, extension: &str) -> PathBuf {
    target_root.join(relative).with_extension(extension)
}

/// Build `Name (n).ext` for collision avoidance.
pub fn numbered_variant(path: &Path, n: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem} ({n}).{}", ext.to_string_lossy()),
        None => format!("{stem} ({n})"),
    };
    path.with_file_name(name)
}

/// Outcome of conflict resolution for one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Path is free.
    Use(PathBuf),
    /// Target exists, job is skipped.
    Skip { existing: PathBuf },
    /// Path existed and was replaced by a numbered variant.
    Renamed { original: PathBuf, renamed: PathBuf },
    /// Existing result will be replaced after validation (user confirmed).
    Replace(PathBuf),
}

impl ConflictResolution {
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConflictResolution::Use(p) | ConflictResolution::Replace(p) => Some(p),
            ConflictResolution::Renamed { renamed, .. } => Some(renamed),
            ConflictResolution::Skip { .. } => None,
        }
    }
}

/// Resolve a naming conflict.
///
/// * `exists` – the path exists on disk,
/// * `reserved` – the path is already the target of another planned job.
///
/// A path reserved by another job is never "replaced" – that would make two
/// jobs write the same file; such conflicts are renamed or skipped.
pub fn resolve_conflict(
    desired: &Path,
    strategy: ConflictStrategy,
    exists: &dyn Fn(&Path) -> bool,
    reserved: &dyn Fn(&Path) -> bool,
) -> ConflictResolution {
    let taken = |p: &Path| exists(p) || reserved(p);
    if !taken(desired) {
        return ConflictResolution::Use(desired.to_path_buf());
    }
    let rename = || {
        let renamed = (1..10_000)
            .map(|n| numbered_variant(desired, n))
            .find(|p| !taken(p))
            .unwrap_or_else(|| numbered_variant(desired, 10_000));
        ConflictResolution::Renamed {
            original: desired.to_path_buf(),
            renamed,
        }
    };
    match strategy {
        ConflictStrategy::Skip => ConflictResolution::Skip {
            existing: desired.to_path_buf(),
        },
        ConflictStrategy::Rename => rename(),
        ConflictStrategy::ReplaceAfterConfirmation if !reserved(desired) => {
            ConflictResolution::Replace(desired.to_path_buf())
        }
        ConflictStrategy::ReplaceAfterConfirmation => rename(),
    }
}

/// `true` if `name` is a hidden file or directory (dot-file).
pub fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn maps_relative_structure() {
        let t = target_for_relative(
            Path::new("/media/optimized"),
            Path::new("Series/Season 01/Episode 01.mkv"),
            "mp4",
        );
        assert_eq!(
            t,
            Path::new("/media/optimized/Series/Season 01/Episode 01.mp4")
        );
        let t = target_for_relative(Path::new("/o"), Path::new("Movie.2020.1080p.MKV"), "mp4");
        assert_eq!(t, Path::new("/o/Movie.2020.1080p.mp4"));
    }

    #[test]
    fn rejects_identical_and_nested_roots() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        assert!(validate_roots(&src, &src).is_err());
        assert!(validate_roots(&src, &src.join("out")).is_err());
        assert!(validate_roots(&src, &src.join("a/b/../out")).is_err());
        let ok = validate_roots(&src, &dir.path().join("out/new")).unwrap();
        assert!(ok.target.ends_with("out/new"));
        // file as source
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_roots(&file, &dir.path().join("o")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_target_inside_source_via_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::os::unix::fs::symlink(&src, dir.path().join("link")).unwrap();
        assert!(validate_roots(&src, &dir.path().join("link/out")).is_err());
    }

    #[test]
    fn conflict_strategies() {
        let existing: HashSet<PathBuf> = [PathBuf::from("/o/M.mp4"), PathBuf::from("/o/M (1).mp4")]
            .into_iter()
            .collect();
        let exists = |p: &Path| existing.contains(p);
        let none = |_: &Path| false;
        let desired = Path::new("/o/M.mp4");

        assert_eq!(
            resolve_conflict(
                Path::new("/o/N.mp4"),
                ConflictStrategy::Skip,
                &exists,
                &none
            ),
            ConflictResolution::Use("/o/N.mp4".into())
        );
        assert!(matches!(
            resolve_conflict(desired, ConflictStrategy::Skip, &exists, &none),
            ConflictResolution::Skip { .. }
        ));
        assert_eq!(
            resolve_conflict(desired, ConflictStrategy::Rename, &exists, &none).path(),
            Some(Path::new("/o/M (2).mp4"))
        );
        assert_eq!(
            resolve_conflict(
                desired,
                ConflictStrategy::ReplaceAfterConfirmation,
                &exists,
                &none
            ),
            ConflictResolution::Replace(desired.into())
        );
        // reserved by another job: never replace
        let reserved = |p: &Path| p == Path::new("/o/X.mp4");
        assert_eq!(
            resolve_conflict(
                Path::new("/o/X.mp4"),
                ConflictStrategy::ReplaceAfterConfirmation,
                &none,
                &reserved
            )
            .path(),
            Some(Path::new("/o/X (1).mp4"))
        );
    }
}
