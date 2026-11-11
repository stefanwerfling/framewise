//! Scanner: discovery → ffprobe analysis (bounded parallelism) → duplicates.
//!
//! The scanner only reads files; it never modifies anything in the source.

pub mod discovery;
pub mod ranking;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::media::model::MediaFile;
use crate::media::probe::{self, FileFacts};
pub use discovery::{DiscoveryOptions, ScanIssue};
pub use ranking::DuplicateGroup;

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub discovery: DiscoveryOptions,
    pub ffprobe: PathBuf,
    pub parallelism: usize,
    pub detect_duplicates: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScanProgress {
    Discovering,
    Discovered {
        total: usize,
    },
    Probed {
        done: usize,
        total: usize,
        current: PathBuf,
    },
    Duplicates,
}

#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub files: Vec<MediaFile>,
    pub issues: Vec<ScanIssue>,
    /// Files taken unchanged from the cache.
    pub reused: usize,
    pub duplicates: Vec<DuplicateGroup>,
    pub cancelled: bool,
}

/// Cached results keyed by absolute path; reused when size and mtime match.
pub type ScanCache = HashMap<PathBuf, MediaFile>;

pub async fn run_scan<F>(
    request: ScanRequest,
    cache: ScanCache,
    cancel: Arc<AtomicBool>,
    progress: F,
) -> ScanOutcome
where
    F: Fn(ScanProgress) + Send + Sync + 'static,
{
    let progress = Arc::new(progress);
    progress(ScanProgress::Discovering);

    let discovery = {
        let options = request.discovery.clone();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || discovery::discover(&options, &cancel))
            .await
            .unwrap_or_default()
    };
    let total = discovery.files.len();
    progress(ScanProgress::Discovered { total });

    let mut outcome = ScanOutcome {
        issues: discovery.issues,
        ..ScanOutcome::default()
    };

    let mut to_probe: Vec<FileFacts> = Vec::new();
    for facts in discovery.files {
        match cache.get(&facts.absolute_path) {
            Some(cached)
                if cached.size_bytes == facts.size_bytes
                    && cached.modified_unix == facts.modified_unix =>
            {
                let mut m = cached.clone();
                m.relative_path = facts.relative_path;
                outcome.files.push(m);
                outcome.reused += 1;
            }
            _ => to_probe.push(facts),
        }
    }

    let semaphore = Arc::new(Semaphore::new(request.parallelism.max(1)));
    let ffprobe = Arc::new(request.ffprobe.clone());
    let mut set = JoinSet::new();
    for facts in to_probe {
        let semaphore = semaphore.clone();
        let ffprobe = ffprobe.clone();
        let cancel = cancel.clone();
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok()?;
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let result = probe::analyze_file(&ffprobe, &facts).await;
            Some((facts.absolute_path, result))
        });
    }

    let mut done = outcome.reused;
    while let Some(joined) = set.join_next().await {
        let Ok(Some((path, result))) = joined else {
            continue;
        };
        done += 1;
        match result {
            Ok(media) => outcome.files.push(media),
            Err(err) => {
                tracing::warn!("probe failed for {}: {err}", path.display());
                outcome.issues.push(ScanIssue {
                    path: path.clone(),
                    message: err.to_string(),
                });
            }
        }
        progress(ScanProgress::Probed {
            done,
            total,
            current: path,
        });
    }
    outcome.cancelled = cancel.load(Ordering::Relaxed);
    outcome
        .files
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

    if request.detect_duplicates && !outcome.cancelled {
        progress(ScanProgress::Duplicates);
        let files = outcome.files.clone();
        outcome.duplicates = tokio::task::spawn_blocking(move || ranking::find_duplicates(&files))
            .await
            .unwrap_or_default();
    }
    outcome
}
