//! FFmpeg process manager: spawn, progress, logging, priority, pause, cancel.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

use super::os;
use super::progress::{ProgressParser, ProgressUpdate};
use crate::core::error::{Error, Result};

const STDERR_KEEP_LINES: usize = 400;

/// Shared handle to control a running job's process(es).
#[derive(Debug, Clone)]
pub struct ProcessHandle {
    cancel_tx: Arc<watch::Sender<bool>>,
    pid: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
}

impl Default for ProcessHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessHandle {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self {
            cancel_tx: Arc::new(tx),
            pid: Arc::new(AtomicU32::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        // A stopped process must be continued to terminate cleanly.
        self.resume();
        self.cancel_tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancel_tx.borrow()
    }

    /// Resolves once the job is cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    /// Suspend the current process (Unix only). Returns whether it worked.
    pub fn pause(&self) -> bool {
        let pid = self.pid.load(Ordering::Relaxed);
        let ok = pid != 0 && os::suspend(pid);
        if ok {
            self.paused.store(true, Ordering::Relaxed);
        }
        ok
    }

    pub fn resume(&self) -> bool {
        let pid = self.pid.load(Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
        pid != 0 && os::resume(pid)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.cancel_tx.subscribe()
    }
}

/// Options for one ffmpeg run.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub ffmpeg: PathBuf,
    pub args: Vec<OsString>,
    /// Full stderr is appended to this file.
    pub log_file: Option<PathBuf>,
    pub nice: i32,
    pub context: String,
}

/// Result of a successful run.
#[derive(Debug, Default)]
pub struct RunOutput {
    /// Last stderr lines (warnings).
    pub stderr_tail: String,
}

async fn append_log(log: &Option<PathBuf>, text: &str) {
    if let Some(path) = log {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Ok(mut f) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
        {
            let _ = f.write_all(text.as_bytes()).await;
        }
    }
}

fn command_line_for_log(program: &Path, args: &[OsString]) -> String {
    let mut s = program.display().to_string();
    for a in args {
        s.push(' ');
        let a = a.to_string_lossy();
        if a.contains([' ', '\'', '"', ';', '$', '`']) {
            s.push('\'');
            s.push_str(&a.replace('\'', "'\\''"));
            s.push('\'');
        } else {
            s.push_str(&a);
        }
    }
    s
}

/// Run ffmpeg, reporting progress. Cancellation kills the process and
/// returns [`Error::Cancelled`].
pub async fn run_ffmpeg<F>(
    opts: RunOptions,
    handle: &ProcessHandle,
    mut on_progress: F,
) -> Result<RunOutput>
where
    F: FnMut(ProgressUpdate),
{
    if handle.is_cancelled() {
        return Err(Error::Cancelled);
    }
    append_log(
        &opts.log_file,
        &format!("\n$ {}\n", command_line_for_log(&opts.ffmpeg, &opts.args)),
    )
    .await;

    let mut cmd = Command::new(&opts.ffmpeg);
    cmd.args(&opts.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    if opts.nice > 0 {
        // BELOW_NORMAL_PRIORITY_CLASS
        cmd.creation_flags(0x0000_4000);
    }
    let mut child = cmd.spawn().map_err(|source| Error::ToolSpawn {
        tool: "ffmpeg",
        path: opts.ffmpeg.clone(),
        source,
    })?;
    let pid = child.id().unwrap_or(0);
    handle.pid.store(pid, Ordering::Relaxed);
    if opts.nice > 0 && pid != 0 && !os::set_nice(pid, opts.nice) && cfg!(unix) {
        tracing::debug!("could not set nice level for pid {pid}");
    }

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let log_file = opts.log_file.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut keep: VecDeque<String> = VecDeque::new();
        let mut buffer = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            buffer.push_str(&line);
            buffer.push('\n');
            if buffer.len() > 8192 {
                append_log(&log_file, &buffer).await;
                buffer.clear();
            }
            if keep.len() == STDERR_KEEP_LINES {
                keep.pop_front();
            }
            keep.push_back(line);
        }
        append_log(&log_file, &buffer).await;
        keep.into_iter().collect::<Vec<_>>().join("\n")
    });

    let mut cancel = handle.subscribe();
    let mut parser = ProgressParser::new();
    let mut lines = BufReader::new(stdout).lines();
    let mut cancelled = false;
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if let Some(update) = parser.push_line(&line) {
                        on_progress(update);
                    }
                }
                _ => break,
            },
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    cancelled = true;
                    break;
                }
            }
        }
    }

    // stdout closed: wait for exit, still honouring cancellation.
    if !cancelled {
        loop {
            tokio::select! {
                _ = child.wait() => break,
                changed = cancel.changed() => {
                    if changed.is_err() {
                        let _ = child.wait().await;
                        break;
                    }
                    if *cancel.borrow() {
                        cancelled = true;
                        break;
                    }
                }
            }
        }
    }
    if cancelled {
        let _ = child.start_kill();
    }
    let status = child.wait().await.map_err(|e| Error::io(&opts.ffmpeg, e))?;
    handle.pid.store(0, Ordering::Relaxed);
    let stderr_tail = stderr_task.await.unwrap_or_default();

    if cancelled || handle.is_cancelled() {
        append_log(&opts.log_file, "[framewise] cancelled by user\n").await;
        return Err(Error::Cancelled);
    }
    if !status.success() {
        return Err(Error::ToolFailed {
            tool: "ffmpeg",
            code: status.code(),
            context: opts.context,
            stderr: crate::core::error::tail_lines(&stderr_tail, 15),
        });
    }
    Ok(RunOutput { stderr_tail })
}
