mod size_rotating;
pub mod trace_log;
pub use tracing;
pub use tracing_appender;
pub use tracing_subscriber;

use std::path::{Path, PathBuf};

/// 把空目录归一化为当前目录，保证写入与归档清理使用同一个有效路径。
///
/// 时间模式（`RollingFileAppender`）与大小模式（`SizeRotatingWriter`）都使用它，
/// 避免“空字符串能写文件、却无法 `read_dir`”导致清理失效。
pub(crate) fn normalize_log_dir(log_dir: impl AsRef<Path>) -> PathBuf {
    let log_dir = log_dir.as_ref();
    if log_dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        log_dir.to_path_buf()
    }
}
