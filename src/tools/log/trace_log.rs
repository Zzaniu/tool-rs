use chrono::Local;
use std::env;
use std::io::stdout;
use tracing::Level;
use tracing_appender::rolling::Rotation;
use tracing_subscriber;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*; // 必须引入以启用 Registry 的 Layer 组合与初始化扩展

#[derive(Clone, Copy)]
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
pub fn init() {
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
        .with_target(false)
        .with_timer(LocalTimer);

    // 1. 获取全局和分层的过滤级别
    let global_level = LevelFilter::from_level(get_log_level(
        env::var("LOG_LEVEL").unwrap_or_default().to_lowercase(),
    ));
    let stdout_level = LevelFilter::from_level(get_log_level(
        env::var("STDOUT_LOG_LEVEL")
            .unwrap_or_default()
            .to_lowercase(),
    ));
    let file_level = LevelFilter::from_level(get_log_level(
        env::var("FILE_LOG_LEVEL")
            .unwrap_or_default()
            .to_lowercase(),
    ));

    // 2. 默认创建并配置 stdout_layer (输出带有 ANSI 颜色)
    let (stdout_non_blocking, stdout_guard) = tracing_appender::non_blocking(stdout());
    let stdout_layer = tracing_subscriber::fmt::layer()
        .event_format(format.clone().pretty())
        .with_writer(stdout_non_blocking)
        .with_ansi(true)
        .with_filter(stdout_level);

    Box::leak(Box::new(stdout_guard));

    // 3. 根据 flag 决定是否创建 file_layer (无 ANSI 颜色)
    let file_layer = if log_to_file_flag {
        let file_appender = tracing_appender::rolling::daily(
            env::var("LOG_DIR").unwrap_or_default(),
            env::var("LOG_FILE").unwrap_or("rs_log.log".to_owned()),
        );
        let (file_non_blocking, file_guard) = tracing_appender::non_blocking(file_appender);
        Box::leak(Box::new(file_guard));

        Some(
            tracing_subscriber::fmt::layer()
                .event_format(format)
                .with_writer(file_non_blocking)
                .with_ansi(false)
                .with_filter(file_level),
        )
    } else {
        None
    };

    // 4. 将 Layer 组合并注册至全局
    tracing_subscriber::registry()
        .with(global_level)
        .with(stdout_layer)
        .with(file_layer)
        .init();
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

pub fn init_with_config(log_config: LogConfig) {
    let format = tracing_subscriber::fmt::format()
        .with_level(true)
        .with_source_location(true)
        .with_target(false)
        .with_timer(LocalTimer);

    // 1. 获取过滤级别
    let log_level = LevelFilter::from_level(get_log_level(log_config.log_level.to_lowercase()));

    // 2. 默认创建并配置 stdout_layer (有 ANSI 颜色)
    let (stdout_non_blocking, stdout_guard) = tracing_appender::non_blocking(stdout());
    let stdout_layer = tracing_subscriber::fmt::layer()
        .event_format(format.clone().pretty())
        .with_writer(stdout_non_blocking)
        .with_ansi(true)
        .with_filter(log_level);

    Box::leak(Box::new(stdout_guard));

    // 3. 根据配置判断是否添加 file_layer (无 ANSI 颜色)
    let file_layer = if log_config.log_to_file_flag {
        let file_appender = tracing_appender::rolling::RollingFileAppender::new(
            log_config.rotation,
            log_config.log_dir,
            log_config.log_file,
        );
        let (file_non_blocking, file_guard) = tracing_appender::non_blocking(file_appender);
        Box::leak(Box::new(file_guard));

        Some(
            tracing_subscriber::fmt::layer()
                .event_format(format)
                .with_writer(file_non_blocking)
                .with_ansi(false)
                .with_filter(log_level),
        )
    } else {
        None
    };

    // 4. 将 Layer 组合并注册至全局
    tracing_subscriber::registry()
        .with(log_level)
        .with(stdout_layer)
        .with(file_layer)
        .init();
}
