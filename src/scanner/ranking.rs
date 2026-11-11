//! Ranking helpers: largest files per folder and globally, duplicate detection.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::media::model::MediaFile;

/// Indices of the `n` largest files in each folder.
pub fn top_n_per_folder(files: &[MediaFile], n: usize) -> Vec<usize> {
    let mut by_folder: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        by_folder.entry(f.folder()).or_default().push(i);
    }
    let mut result = Vec::new();
    for mut idx in by_folder.into_values() {
        idx.sort_by_key(|&i| std::cmp::Reverse(files[i].size_bytes));
        result.extend(idx.into_iter().take(n));
    }
    result.sort_unstable();
    result
}

/// Indices of the `n` largest files overall, largest first.
pub fn top_n_global(files: &[MediaFile], n: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..files.len()).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(files[i].size_bytes));
    idx.truncate(n);
    idx
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DuplicateKind {
    /// Same size and identical sampled content.
    IdenticalContent,
    /// Same title (file stem) and duration – e.g. MKV and MP4 version.
    SameTitle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub kind: DuplicateKind,
    pub paths: Vec<PathBuf>,
}

const SAMPLE: u64 = 4 * 1024 * 1024;

/// Hash of size + first and last 4 MiB. Cheap, good enough to confirm
/// candidates that already share the exact size.
pub fn partial_hash(path: &Path) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut hasher = DefaultHasher::new();
    len.hash(&mut hasher);
    let mut buf = vec![0u8; SAMPLE.min(len) as usize];
    file.read_exact(&mut buf)?;
    buf.hash(&mut hasher);
    if len > SAMPLE {
        let start = len.saturating_sub(SAMPLE).max(SAMPLE);
        file.seek(SeekFrom::Start(start))?;
        let mut tail = Vec::new();
        file.take(SAMPLE).read_to_end(&mut tail)?;
        tail.hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn normalized_stem(f: &MediaFile) -> String {
    f.relative_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Find duplicate groups. Reads file samples only for same-size candidates.
pub fn find_duplicates(files: &[MediaFile]) -> Vec<DuplicateGroup> {
    let mut groups = Vec::new();

    let mut by_size: HashMap<u64, Vec<&MediaFile>> = HashMap::new();
    for f in files.iter().filter(|f| f.size_bytes > 0) {
        by_size.entry(f.size_bytes).or_default().push(f);
    }
    for candidates in by_size.into_values().filter(|c| c.len() > 1) {
        let mut by_hash: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        for f in candidates {
            if let Ok(h) = partial_hash(&f.absolute_path) {
                by_hash.entry(h).or_default().push(f.absolute_path.clone());
            }
        }
        groups.extend(
            by_hash
                .into_values()
                .filter(|p| p.len() > 1)
                .map(|mut paths| {
                    paths.sort();
                    DuplicateGroup {
                        kind: DuplicateKind::IdenticalContent,
                        paths,
                    }
                }),
        );
    }

    let mut by_stem: HashMap<String, Vec<&MediaFile>> = HashMap::new();
    for f in files {
        by_stem.entry(normalized_stem(f)).or_default().push(f);
    }
    for candidates in by_stem.into_values().filter(|c| c.len() > 1) {
        // cluster by duration (±2 s)
        let mut used = vec![false; candidates.len()];
        for i in 0..candidates.len() {
            if used[i] {
                continue;
            }
            let mut cluster = vec![candidates[i].absolute_path.clone()];
            for j in i + 1..candidates.len() {
                let close = match (candidates[i].duration_s(), candidates[j].duration_s()) {
                    (Some(a), Some(b)) => (a - b).abs() <= 2.0,
                    _ => false,
                };
                if !used[j] && close && candidates[i].size_bytes != candidates[j].size_bytes {
                    used[j] = true;
                    cluster.push(candidates[j].absolute_path.clone());
                }
            }
            if cluster.len() > 1 {
                cluster.sort();
                groups.push(DuplicateGroup {
                    kind: DuplicateKind::SameTitle,
                    paths: cluster,
                });
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::fixtures::{media, video};

    fn file(rel: &str, size: u64, dur: f64) -> MediaFile {
        let mut m = media(
            "mkv",
            size,
            dur,
            vec![video("h264", 1920, 1080)],
            vec![],
            vec![],
        );
        m.relative_path = rel.into();
        m.absolute_path = PathBuf::from("/nonexistent").join(rel);
        m
    }

    #[test]
    fn top_n() {
        let files = vec![
            file("A/1.mkv", 10, 1.0),
            file("A/2.mkv", 30, 1.0),
            file("A/3.mkv", 20, 1.0),
            file("B/4.mkv", 5, 1.0),
        ];
        assert_eq!(top_n_per_folder(&files, 2), vec![1, 2, 3]);
        assert_eq!(top_n_global(&files, 2), vec![1, 2]);
    }

    #[test]
    fn identical_content_and_same_title() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mkv");
        let b = dir.path().join("b.mkv");
        let c = dir.path().join("c.mkv");
        std::fs::write(&a, vec![1u8; 1000]).unwrap();
        std::fs::write(&b, vec![1u8; 1000]).unwrap();
        std::fs::write(&c, vec![2u8; 1000]).unwrap();
        let mk = |p: &Path, rel: &str| {
            let mut m = file(rel, 1000, 100.0);
            m.absolute_path = p.to_path_buf();
            m
        };
        let mut files = vec![mk(&a, "x/a.mkv"), mk(&b, "y/b.mkv"), mk(&c, "z/c.mkv")];
        let mut same = file("Movie.mp4", 500, 5400.5);
        same.relative_path = "D/Movie.mp4".into();
        let mut same2 = file("Movie.mkv", 900, 5400.0);
        same2.relative_path = "D/Movie.mkv".into();
        files.push(same);
        files.push(same2);
        let groups = find_duplicates(&files);
        assert_eq!(groups.len(), 2);
        assert!(
            groups
                .iter()
                .any(|g| g.kind == DuplicateKind::IdenticalContent
                    && g.paths == vec![a.clone(), b.clone()])
        );
        assert!(
            groups
                .iter()
                .any(|g| g.kind == DuplicateKind::SameTitle && g.paths.len() == 2)
        );
    }
}
