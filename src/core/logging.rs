//! Logging setup based on `tracing`.

use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

/// Initialise logging to stderr and to `<log_dir>/framewise.log`.
///
/// The filter can be overridden with `RUST_LOG`.
pub fn init(log_dir: &Path) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file = std::fs::create_dir_all(log_dir).ok().and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("framewise.log"))
            .ok()
    });

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let result = match file {
        Some(file) => builder
            .with_ansi(false)
            .with_writer(std::io::stderr.and(Mutex::new(file)))
            .try_init(),
        None => builder.try_init(),
    };
    if let Err(err) = result {
        eprintln!("Logging konnte nicht initialisiert werden: {err}");
    }
}
