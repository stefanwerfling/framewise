//! Recursive file discovery.
//!
//! * case-insensitive extension matching,
//! * symlink loops are detected by walkdir and reported, not followed,
//! * a physical file reached twice (e.g. via a symlinked folder) is listed once,
//! * excluded directories (target, work dir) are never entered,
//! * access errors are collected and reported instead of aborting the scan.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use walkdir::WalkDir;

use crate::core::paths::is_hidden;
use crate::media::probe::FileFacts;

#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    pub root: PathBuf,
    /// Lower-case extensions without dot.
    pub extensions: Vec<String>,
    pub follow_symlinks: bool,
    pub skip_hidden: bool,
    /// Canonical directories that must never be scanned.
    pub exclude: Vec<PathBuf>,
}

/// A problem encountered during the scan (file is skipped, scan continues).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScanIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub files: Vec<FileFacts>,
    pub issues: Vec<ScanIssue>,
}

pub fn has_media_extension(path: &Path, extensions: &[String]) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(&e)))
}

/// Walk `options.root` and collect candidate media files.
pub fn discover(options: &DiscoveryOptions, cancel: &AtomicBool) -> Discovery {
    let mut result = Discovery::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let exclude = &options.exclude;

    let walker = WalkDir::new(&options.root)
        .follow_links(options.follow_symlinks)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if options.skip_hidden && is_hidden(e.file_name()) {
                return false;
            }
            if e.file_type().is_dir()
                && let Ok(canonical) = e.path().canonicalize()
                && exclude.iter().any(|x| canonical.starts_with(x))
            {
                return false;
            }
            true
        });

    for entry in walker {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                let path = err
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| options.root.clone());
                let message = if let Some(ancestor) = err.loop_ancestor() {
                    format!("symlink loop to {}", ancestor.display())
                } else {
                    err.to_string()
                };
                result.issues.push(ScanIssue { path, message });
                continue;
            }
        };
        if !entry.file_type().is_file() || !has_media_extension(entry.path(), &options.extensions) {
            continue;
        }
        let path = entry.path().to_path_buf();
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !seen.insert(canonical) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                result.issues.push(ScanIssue {
                    path,
                    message: err.to_string(),
                });
                continue;
            }
        };
        let relative_path = path
            .strip_prefix(&options.root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| PathBuf::from(entry.file_name()));
        let modified_unix = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        result.files.push(FileFacts {
            absolute_path: path,
            relative_path,
            size_bytes: metadata.len(),
            modified_unix,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(root: &Path) -> DiscoveryOptions {
        DiscoveryOptions {
            root: root.to_path_buf(),
            extensions: vec!["mkv".into(), "mp4".into()],
            follow_symlinks: true,
            skip_hidden: true,
            exclude: vec![],
        }
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn finds_nested_files_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Action/Movie A.mkv"));
        touch(&root.join("Action/Movie B.MKV"));
        touch(&root.join("Drama/Movie C.Mp4"));
        touch(&root.join("Series/Season 01/Episode 01.mkv"));
        touch(&root.join("Series/Season 01/Episode 01.nfo"));
        touch(&root.join(".hidden/Secret.mkv"));
        touch(&root.join("Drama/.partial.mkv"));
        let d = discover(&opts(root), &AtomicBool::new(false));
        let mut rel: Vec<_> = d
            .files
            .iter()
            .map(|f| f.relative_path.to_string_lossy().into_owned())
            .collect();
        rel.sort();
        assert_eq!(
            rel,
            [
                "Action/Movie A.mkv",
                "Action/Movie B.MKV",
                "Drama/Movie C.Mp4",
                "Series/Season 01/Episode 01.mkv"
            ]
        );
        assert!(d.issues.is_empty());
        assert_eq!(d.files[0].size_bytes, 1);
    }

    #[test]
    fn excludes_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        touch(&root.join("a/1.mkv"));
        touch(&root.join("work/2.mkv"));
        let mut o = opts(&root);
        o.exclude = vec![root.join("work")];
        let d = discover(&o, &AtomicBool::new(false));
        assert_eq!(d.files.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn handles_symlink_loops_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("real/Film.mkv"));
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        std::os::unix::fs::symlink(root, root.join("real/loop")).unwrap();
        let d = discover(&opts(root), &AtomicBool::new(false));
        assert_eq!(d.files.len(), 1, "same physical file only once");
        assert!(d.issues.iter().any(|i| i.message.contains("loop")));
    }

    #[test]
    fn cancel_stops_early() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mkv"));
        let d = discover(&opts(dir.path()), &AtomicBool::new(true));
        assert!(d.files.is_empty());
    }
}
