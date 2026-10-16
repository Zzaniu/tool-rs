use chrono::Local;
use std::env;
use std::io::{Write, stdout};
use tracing::Level;
use tracing_appender::rolling::Rotation;
use tracing_subscriber;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*; // 必须引入以启用 Registry 的 Layer 组合与初始化扩展

use super::normalize_log_dir;
use super::size_rotating::SizeRotatingWriter;

/// `LOG_ROTATION=size` 且未指定 `LOG_MAX_FILE_SIZE` 时默认的单文件大小上限（10 MiB）。
const DEFAULT_MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

#[derive(Clone, Copy)]
struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", Local::now().format("%FT%T%.3f"))
    }
}

/// 日志文件的旋转策略：**要么按时间、要么按大小**，二选一。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogRotation {
    /// 按时间周期旋转（沿用 `tracing-appender` 的行为，文件名带日期）。
    Time(Rotation),
    /// 单个文件超过 `max_bytes` 字节时旋转；归档文件仅改名、不压缩。
    Size { max_bytes: u64 },
}

/// 初始化日志, 默认级别都是 info
/// LOG_LEVEL 设置总的日志级别
/// FILE_LOG_LEVEL 设置写入文件日志级别
/// STDOUT_LOG_LEVEL 设置写入控制台日志级别
/// LOG_ROTATION 设置旋转策略: minutely/hourly/daily/weekly/never/size (默认 daily)
/// LOG_MAX_FILE_SIZE 当 LOG_ROTATION=size 时的单文件大小上限, 如 10MB/2GB/1048576 (默认 10MB)
/// LOG_MAX_FILES 日志文件最大数量(含当前文件), 0 或不设置表示不限制
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
        let writer = build_file_writer(
            rotation_from_env(),
            &env::var("LOG_DIR").unwrap_or_default(),
            &env::var("LOG_FILE").unwrap_or_else(|_| "rs_log.log".to_owned()),
            max_files_from_env(),
        );
        let (file_non_blocking, file_guard) = tracing_appender::non_blocking(writer);
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

/// 依据旋转策略构造底层文件 writer。
///
/// 初始化失败时 panic，与改造前 `RollingFileAppender::new` 的行为保持一致。
fn build_file_writer(
    rotation: LogRotation,
    log_dir: &str,
    log_file: &str,
    max_files: Option<usize>,
) -> Box<dyn Write + Send> {
    // 空目录也表示当前目录，确保写入和归档清理使用一致的有效路径。
    let log_dir = normalize_log_dir(log_dir);
    // 统一语义：`None` 与 `Some(0)` 都表示不限制。
    // 注意 tracing-appender 的 `max_log_files(0)` 会因 `max_files - 1` 下溢而 panic，
    // 因此这里必须先把 0 归一化为 None。
    let max_files = max_files.filter(|n| *n > 0);
    match rotation {
        LogRotation::Time(rotation) => {
            let mut builder = tracing_appender::rolling::RollingFileAppender::builder()
                .rotation(rotation)
                .filename_prefix(log_file);
            if let Some(max_files) = max_files {
                builder = builder.max_log_files(max_files);
            }
            let appender = builder
                .build(log_dir)
                .expect("initializing rolling file appender failed");
            Box::new(appender)
        }
        LogRotation::Size { max_bytes } => {
            let writer = SizeRotatingWriter::new(log_dir, log_file, max_bytes, max_files)
                .expect("initializing size rotating log writer failed");
            Box::new(writer)
        }
    }
}

/// 从环境变量解析旋转策略，非法值回退到按天旋转。
fn rotation_from_env() -> LogRotation {
    match env::var("LOG_ROTATION")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "minutely" => LogRotation::Time(Rotation::MINUTELY),
        "hourly" => LogRotation::Time(Rotation::HOURLY),
        "daily" | "" => LogRotation::Time(Rotation::DAILY),
        "weekly" => LogRotation::Time(Rotation::WEEKLY),
        "never" => LogRotation::Time(Rotation::NEVER),
        "size" => LogRotation::Size {
            max_bytes: env::var("LOG_MAX_FILE_SIZE")
                .ok()
                .and_then(|value| parse_size(&value))
                .unwrap_or(DEFAULT_MAX_FILE_SIZE),
        },
        other => {
            eprintln!("Unknown LOG_ROTATION value `{other}`, falling back to daily rotation");
            LogRotation::Time(Rotation::DAILY)
        }
    }
}

/// 从环境变量解析最大文件数；未设置、非法或 0 均表示不限制。
fn max_files_from_env() -> Option<usize> {
    match env::var("LOG_MAX_FILES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
    {
        Some(n) if n > 0 => Some(n),
        _ => None,
    }
}

/// 解析形如 `10MB`、`512KB`、`1GB`、`1024B` 或纯字节数的字符串（大小写不敏感）。
fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_uppercase();
    let (number, unit) = if let Some(n) = value.strip_suffix("KB") {
        (n, 1024)
    } else if let Some(n) = value.strip_suffix("MB") {
        (n, 1024 * 1024)
    } else if let Some(n) = value.strip_suffix("GB") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = value.strip_suffix('B') {
        (n, 1)
    } else {
        (value.as_str(), 1)
    };
    number.trim().parse::<u64>().ok()?.checked_mul(unit)
}

pub struct LogConfig<'a> {
    pub log_to_file_flag: bool,
    pub log_level: &'a str,
    pub log_dir: &'a str,
    pub log_file: &'a str,
    pub rotation: LogRotation,
    /// 日志文件最大数量（含当前文件）；`None` 表示不限制。
    pub max_files: Option<usize>,
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
            rotation: LogRotation::Time(Rotation::DAILY),
            max_files: None,
        }
    }
}

impl<'a> LogConfig<'a> {
    pub fn new<'b: 'a>(
        log_to_file_flag: bool,
        log_level: &'b str,
        log_dir: &'b str,
        log_file: &'b str,
        rotation: LogRotation,
        max_files: Option<usize>,
    ) -> Self {
        Self {
            log_to_file_flag,
            log_level,
            log_dir,
            log_file,
            rotation,
            max_files,
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

    pub fn rotation(mut self, rotation: LogRotation) -> Self {
        self.rotation = rotation;
        self
    }

    pub fn max_files(mut self, max_files: usize) -> Self {
        self.max_files = Some(max_files);
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
        let writer = build_file_writer(
            log_config.rotation,
            log_config.log_dir,
            log_config.log_file,
            log_config.max_files,
        );
        let (file_non_blocking, file_guard) = tracing_appender::non_blocking(writer);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_supports_units() {
        assert_eq!(parse_size("10MB"), Some(10 * 1024 * 1024));
        assert_eq!(parse_size("512kb"), Some(512 * 1024));
        assert_eq!(parse_size("1GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("1024B"), Some(1024));
        assert_eq!(parse_size("2048"), Some(2048));
        assert_eq!(parse_size(" 5 mb "), Some(5 * 1024 * 1024));
    }

    #[test]
    fn parse_size_rejects_invalid() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("10XB"), None);
    }

    #[test]
    fn time_rotation_with_zero_max_files_is_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().to_str().unwrap();

        // Some(0) 必须归一化为"不限制"；否则 tracing-appender 的
        // max_log_files(0) 会因 `max_files - 1` 下溢而 panic。
        let _writer =
            build_file_writer(LogRotation::Time(Rotation::DAILY), dir, "app.log", Some(0));
    }

    #[test]
    fn empty_log_dir_obeys_file_limit() {
        use std::fs;
        use std::process::Command;

        const CHILD_ENV: &str = "TOOL_RS_EMPTY_LOG_DIR_TEST_CHILD";
        if env::var_os(CHILD_ENV).is_none() {
            // 在子进程中设置工作目录，不影响并行执行的其他测试。
            let dir = tempfile::tempdir().unwrap();
            let output = Command::new(env::current_exe().unwrap())
                .args([
                    "tools::log::trace_log::tests::empty_log_dir_obeys_file_limit",
                    "--exact",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child test failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }

        let writers: [(&str, Box<dyn Write + Send>); 2] = [
            (
                "size.log",
                build_file_writer(LogRotation::Size { max_bytes: 3 }, "", "size.log", Some(2)),
            ),
            (
                "direct.log",
                Box::new(SizeRotatingWriter::new("", "direct.log", 3, Some(2)).unwrap()),
            ),
        ];
        for (file_name, mut writer) in writers {
            for _ in 0..5 {
                writer.write_all(b"log").unwrap();
            }
            drop(writer);
            let count = fs::read_dir(".")
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(file_name))
                .count();
            assert_eq!(count, 2, "file limit was ignored for {file_name}");
        }

        for day in 1..=3 {
            fs::write(format!("time.log.2000-01-{day:02}"), b"old").unwrap();
        }
        let mut writer =
            build_file_writer(LogRotation::Time(Rotation::DAILY), "", "time.log", Some(2));
        writer.write_all(b"new").unwrap();
        drop(writer);
        let count = fs::read_dir(".")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("time.log"))
            .count();
        assert_eq!(count, 2, "file limit was ignored for time rotation");
    }
}
