use std::env;
use std::io::stdout;

use chrono::Local;
use tracing::Level;
pub use tracing::{self, debug, error, info, trace, warn};
pub use tracing_appender::non_blocking::WorkerGuard;
pub use tracing_appender::rolling::Rotation;
pub use tracing_subscriber;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::writer::MakeWriterExt;

struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", Local::now().format("%FT%T%.3f"))
    }
}

/// 初始化日志, 默认级别都是 info
/// LOG_LEVEL 设置总的日志级别
/// FILE_LOG_LEVEL 设置写入文件日志级别
/// STDOUT_LOG_LEVEL 设置写入控制台日志级别
pub fn init() -> WorkerGuard {
    #[cfg(debug_assertions)]
    let log_to_file_flag = env::var("LOG_TO_FILE_FLAG")
        .map(|x| x.parse::<bool>().unwrap_or_default())
        .unwrap_or_default();
    #[cfg(not(debug_assertions))]
    let log_to_file_flag = env::var("LOG_TO_FILE_FLAG")
        .map(|x| x.parse::<bool>().unwrap_or(true))
        .unwrap_or(true);
    let format = tracing_subscriber::fmt::format()
        .with_level(true)
        .with_source_location(true)
        .with_target(true)
        .with_timer(LocalTimer);

    let builder = tracing_subscriber::fmt().with_max_level(get_log_level(
        env::var("LOG_LEVEL").unwrap_or_default().to_lowercase(),
    ));

    if log_to_file_flag {
        let file_appender = tracing_appender::rolling::daily(
            env::var("LOG_DIR").unwrap_or_default(),
            env::var("LOG_FILE").unwrap_or("rs_log.log".to_owned()),
        );
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        builder
            .event_format(format)
            .with_writer(
                non_blocking
                    .with_max_level(get_log_level(
                        env::var("FILE_LOG_LEVEL")
                            .unwrap_or_default()
                            .to_lowercase(),
                    ))
                    .and(
                        stdout.with_max_level(get_log_level(
                            env::var("STDOUT_LOG_LEVEL")
                                .unwrap_or_default()
                                .to_lowercase(),
                        )),
                    ),
            ) // 同时追加控制台输出
            .with_ansi(false) // 如果日志是写入文件，应将ansi的颜色输出功能关掉
            .init();
        return guard;
    }

    let (non_blocking, guard) = tracing_appender::non_blocking(stdout());
    builder
        .event_format(format.pretty())
        .with_writer(non_blocking)
        .with_ansi(true)
        .init();
    guard
}

fn get_log_level(log_level: impl AsRef<str>) -> Level {
    match log_level.as_ref() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    }
}

pub struct LogConfig<'a> {
    pub log_to_file_flag: bool,
    pub log_level: &'a str,
    pub log_dir: &'a str,
    pub log_file: &'a str,
    pub rotation: Rotation,
    // pub file_log_level: &'a str,
    // pub stdout_log_level: &'a str,
}

impl<'a> Default for LogConfig<'a> {
    fn default() -> Self {
        Self {
            #[cfg(debug_assertions)]
            log_to_file_flag: false,
            #[cfg(not(debug_assertions))]
            log_to_file_flag: true,
            log_level: "info",
            log_dir: "logs",
            log_file: "rs_log.log",
            rotation: Rotation::DAILY,
        }
    }
}

impl<'a> LogConfig<'a> {
    pub fn new<'b: 'a>(
        log_to_file_flag: bool,
        log_level: &'b str,
        log_dir: &'b str,
        log_file: &'b str,
        rotation: Rotation,
    ) -> Self {
        Self {
            log_to_file_flag,
            log_level,
            log_dir,
            log_file,
            rotation,
        }
    }

    pub fn log_file(mut self, log_file: &'a str) -> Self {
        self.log_file = log_file;
        self
    }

    pub fn log_dir(mut self, log_dir: &'a str) -> Self {
        self.log_dir = log_dir;
        self
    }

    pub fn log_level(mut self, log_level: &'a str) -> Self {
        self.log_level = log_level;
        self
    }

    pub fn log_to_file_flag(mut self, log_to_file_flag: bool) -> Self {
        self.log_to_file_flag = log_to_file_flag;
        self
    }

    pub fn rotation(mut self, rotation: Rotation) -> Self {
        self.rotation = rotation;
        self
    }
}

pub fn init_with_config(log_config: LogConfig) -> WorkerGuard {
    let format = tracing_subscriber::fmt::format()
        .with_level(true)
        .with_source_location(true)
        .with_target(true)
        .with_timer(LocalTimer);

    let builder = tracing_subscriber::fmt()
        .with_max_level(get_log_level(log_config.log_level.to_lowercase()));

    if log_config.log_to_file_flag {
        let file_appender = tracing_appender::rolling::RollingFileAppender::new(
            log_config.rotation,
            log_config.log_dir,
            log_config.log_file,
        );
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        builder
            .event_format(format)
            .with_writer(
                non_blocking
                    .with_max_level(get_log_level(log_config.log_level.to_lowercase()))
                    .and(stdout.with_max_level(get_log_level(log_config.log_level.to_lowercase()))),
            ) // 同时追加控制台输出
            .with_ansi(false) // 如果日志是写入文件，应将ansi的颜色输出功能关掉
            .init();
        return guard;
    }

    let (non_blocking, guard) = tracing_appender::non_blocking(stdout());
    builder
        .event_format(format.pretty())
        .with_writer(non_blocking)
        .with_ansi(true)
        .init();
    guard
}
