//! Logging: a daily log file (kept for a week) under the state folder, and optionally
//! stderr for headless runs (`--log debug`). `CRACK_LOG` sets the level filter
//! (e.g. `debug`, `crack=trace`); the default is `info`.

use std::path::PathBuf;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Where the log files go: `$XDG_STATE_HOME/crack` (`~/.local/state/crack`).
pub fn dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CRACK_DATA_DIR") {
        return PathBuf::from(dir).join("logs");
    }
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("crack")
}

/// Starts logging. Keep the guard alive until exit so the file gets flushed.
pub fn init(stderr_level: Option<&str>) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = || EnvFilter::try_from_env("CRACK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = std::fs::create_dir_all(dir());
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("crack")
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir())
        .ok();
    let (file_layer, guard) = match appender {
        Some(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_target(true)
                .with_filter(filter());
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };
    let stderr_layer = stderr_level.map(|level| {
        let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("debug"));
        let color = std::io::IsTerminal::is_terminal(&std::io::stderr());
        fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(color)
            .with_target(false)
            .with_filter(filter)
    });
    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
    guard
}
