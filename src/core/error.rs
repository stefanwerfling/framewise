//! Typed application errors with meaningful context.

use std::fmt;
use std::path::PathBuf;

use crate::{tr, trf};

/// Central error type of the domain layer.
///
/// Every variant carries enough context (paths, tool names, stderr excerpts)
/// to be shown to the user without further lookups. `Display` is localised.
#[derive(Debug)]
pub enum Error {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    ToolSpawn {
        tool: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    ToolFailed {
        tool: &'static str,
        code: Option<i32>,
        context: String,
        stderr: String,
    },
    ProbeParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidPaths(String),
    Config(String),
    Db(rusqlite::Error),
    Serde(serde_json::Error),
    Plan(String),
    Validation(String),
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Error::Io { path, source } => trf!("I/O error at '{}': {}", path.display(), source),
            Error::ToolSpawn { tool, path, source } => trf!(
                "Could not start {} ('{}'): {}",
                tool,
                path.display(),
                source
            ),
            Error::ToolFailed {
                tool,
                code,
                context,
                stderr,
            } => trf!(
                "{} failed (exit code {}) – {}: {}",
                tool,
                code.map_or_else(|| "–".to_string(), |c| c.to_string()),
                context,
                stderr
            ),
            Error::ProbeParse { path, source } => trf!(
                "Cannot read ffprobe output for '{}': {}",
                path.display(),
                source
            ),
            Error::InvalidPaths(m) => trf!("Invalid directory configuration: {}", m),
            Error::Config(m) => trf!("Configuration error: {}", m),
            Error::Db(e) => trf!("Database error: {}", e),
            Error::Serde(e) => trf!("Serialization error: {}", e),
            Error::Plan(m) => trf!("Cannot plan job: {}", m),
            Error::Validation(m) => {
                trf!("Validation failed: {}", m)
            }
            Error::Cancelled => tr!("Operation cancelled").to_string(),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } | Error::ToolSpawn { source, .. } => Some(source),
            Error::ProbeParse { source, .. } | Error::Serde(source) => Some(source),
            Error::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serde(e)
    }
}

impl Error {
    /// Helper to attach a path to an [`std::io::Error`].
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Shorten long stderr output for display while keeping the end (where
/// ffmpeg usually prints the actual error).
pub fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}
