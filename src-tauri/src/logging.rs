//! Persistent file logging with daily rotation and 7-day retention.
//!
//! Log files are written to `{app_data_dir}/CMTYStudio/logs/`.
//! A stderr layer is also included so `cargo run` output is unchanged.

use std::fs;
use std::path::PathBuf;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Maximum age of log files before cleanup (in days).
const LOG_RETENTION_DAYS: u64 = 7;

/// Return the log directory: `{data_dir}/CMTYStudio/logs/`.
fn log_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("CMTYStudio").join("logs"))
}

/// Initialise the global tracing subscriber with file + stderr output.
///
/// If the log directory cannot be resolved, falls back to stderr-only logging.
pub fn init() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if let Some(dir) = log_dir() {
        // Ensure the directory exists before handing it to the appender.
        let _ = fs::create_dir_all(&dir);

        let file_appender = rolling::daily(&dir, "cmtystudio.log");
        let file_layer = fmt::layer()
            .with_writer(file_appender)
            .with_ansi(false)
            .with_target(true);

        let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_target(true);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stderr_layer)
            .init();

        // Best-effort cleanup of old log files after subscriber is live.
        cleanup_old_logs(&dir);
    } else {
        // Fallback: stderr only.
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().with_writer(std::io::stderr))
            .init();
    }
}

/// Remove log files older than `LOG_RETENTION_DAYS`.
fn cleanup_old_logs(dir: &PathBuf) {
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(LOG_RETENTION_DAYS * 24 * 60 * 60);

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Only touch files that look like our log files.
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.starts_with("cmtystudio.log") => n,
            _ => continue,
        };
        // The current day's log has no date suffix — never delete it.
        if name == "cmtystudio.log" {
            continue;
        }
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}
