//! Running external tools (ffmpeg / ffprobe) safely.
//!
//! Arguments are always passed as a list to [`tokio::process::Command`] –
//! never through a shell – so file names cannot inject commands.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use super::error::{Error, Result, tail_lines};
use crate::{tr, trf};

/// Paths to the external tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPaths {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// Result of the tool detection at startup.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolInfo {
    pub ffmpeg_version: Option<String>,
    pub ffprobe_version: Option<String>,
    pub ffmpeg_error: Option<String>,
    pub ffprobe_error: Option<String>,
}

impl ToolInfo {
    pub fn ready(&self) -> bool {
        self.ffmpeg_version.is_some() && self.ffprobe_version.is_some()
    }
}

/// Captured output of a finished process.
#[derive(Debug)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Run a tool to completion, capturing stdout/stderr, with timeout.
/// The child is killed if the future is dropped or the timeout expires.
pub async fn run_capture<I, S>(
    tool: &'static str,
    program: &Path,
    args: I,
    timeout: Duration,
    context: &str,
) -> Result<Captured>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|source| Error::ToolSpawn {
        tool,
        path: program.to_path_buf(),
        source,
    })?;
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| Error::io(program, e))?,
        Err(_) => {
            return Err(Error::ToolFailed {
                tool,
                code: None,
                context: context.to_string(),
                stderr: trf!("Timed out after {} s", timeout.as_secs()),
            });
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(Error::ToolFailed {
            tool,
            code: output.status.code(),
            context: context.to_string(),
            stderr: tail_lines(&stderr, 12),
        });
    }
    Ok(Captured {
        stdout: output.stdout,
        stderr,
    })
}

async fn version_of(tool: &'static str, path: &Path) -> std::result::Result<String, String> {
    match run_capture(
        tool,
        path,
        ["-hide_banner", "-version"],
        Duration::from_secs(10),
        tr!("version query"),
    )
    .await
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            Ok(text
                .lines()
                .next()
                .unwrap_or(tr!("unknown"))
                .trim()
                .to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Detect ffmpeg and ffprobe and their versions.
pub async fn detect(paths: &ToolPaths) -> ToolInfo {
    let (ffmpeg, ffprobe) = tokio::join!(
        version_of("ffmpeg", &paths.ffmpeg),
        version_of("ffprobe", &paths.ffprobe)
    );
    let mut info = ToolInfo::default();
    match ffmpeg {
        Ok(v) => info.ffmpeg_version = Some(v),
        Err(e) => info.ffmpeg_error = Some(e),
    }
    match ffprobe {
        Ok(v) => info.ffprobe_version = Some(v),
        Err(e) => info.ffprobe_error = Some(e),
    }
    info
}
