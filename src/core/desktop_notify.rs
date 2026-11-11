//! Desktop notifications (best effort, never blocking).
//!
//! Linux/BSD: `notify-send` with an argument list – no shell, and `--` ends
//! option parsing so file names starting with `-` cannot become options.
//! Other platforms: no-op for now (macOS would need an AppleScript string,
//! which is deliberately not built from file names).

use std::time::Duration;

/// Fire-and-forget; must be called inside the Tokio runtime.
pub fn send(summary: String, body: String) {
    if !cfg!(all(unix, not(target_os = "macos"))) {
        return;
    }
    tokio::spawn(async move {
        let args = notify_send_args(&summary, &body);
        let result = crate::core::tools::run_capture(
            "notify-send",
            std::path::Path::new("notify-send"),
            &args,
            Duration::from_secs(10),
            "desktop notification",
        )
        .await;
        if let Err(e) = result {
            tracing::debug!("desktop notification not shown: {e}");
        }
    });
}

fn notify_send_args(summary: &str, body: &str) -> Vec<String> {
    vec![
        "--app-name=Framewise".into(),
        "--icon=video-x-generic".into(),
        "--".into(),
        summary.into(),
        body.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_end_before_user_text() {
        let args = notify_send_args("Done", "-rf; $(evil).mkv");
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[sep + 2], "-rf; $(evil).mkv");
        assert!(args[..sep].iter().all(|a| a.starts_with("--")));
    }
}
