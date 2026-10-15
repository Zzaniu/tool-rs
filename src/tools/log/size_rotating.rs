//! 按文件大小旋转的日志 writer。
//!
//! 提供 [`SizeRotatingWriter`]，实现 [`std::io::Write`]：当写入会导致当前文件
//! 超过 `max_bytes` 时，先把当前文件改名归档（文件名追加本地时间戳），再新建
//! 同名文件继续写入。
//!
//! 归档文件**不做压缩，仅重命名**；可通过 `max_files` 限制保留的日志文件总数。
//!
//! # 降级语义
//!
//! 单次写入本身超过 `max_bytes` 时允许文件超限，以避免生成空归档。
//! 初始化必须完整扫描归档目录以恢复编号；打开文件或扫描失败会返回错误，由调用方处理。
//! 运行时旋转或清理失败会尽量继续记录，大小或数量限制可能暂时超出；后续成功旋转时
//! 会再次尝试清理。文件重新打开或写入失败仍可能导致当条日志丢失。

use chrono::{Local, NaiveDateTime, SubsecRound};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::normalize_log_dir;

/// 归档文件名使用的本地时间戳格式。
///
/// 不含 Windows 非法字符（冒号）；同一毫秒内的归档按数字序号排序。
const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H-%M-%S%.3f";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ArchiveKey {
    timestamp: NaiveDateTime,
    index: u64,
}

impl ArchiveKey {
    fn next(self) -> io::Result<Self> {
        Ok(Self {
            index: self
                .index
                .checked_add(1)
                .ok_or_else(|| io::Error::other("log archive sequence overflow"))?,
            ..self
        })
    }
}

/// 只接受本 writer 生成的完整时间戳和可选序号，其他名称不参与恢复或清理。
fn parse_archive_key(name: &str, prefix: &str) -> Option<ArchiveKey> {
    let suffix = name.strip_prefix(prefix)?;
    let (timestamp, index) = match suffix.rsplit_once('_') {
        Some((timestamp, index)) => {
            let parsed = index.parse::<u64>().ok()?;
            if parsed == 0 || parsed.to_string() != index {
                return None;
            }
            (timestamp, parsed)
        }
        None => (suffix, 0),
    };
    let parsed = NaiveDateTime::parse_from_str(timestamp, TIMESTAMP_FORMAT).ok()?;
    // chrono 的解析允许部分宽松格式，往返校验确保与实际归档名称完全一致。
    if parsed.format(TIMESTAMP_FORMAT).to_string() != timestamp {
        return None;
    }
    Some(ArchiveKey {
        timestamp: parsed,
        index,
    })
}

/// 按大小旋转的日志 writer。
///
/// 典型用法是把它交给 `tracing_appender::non_blocking`，再挂到
/// `tracing_subscriber` 的 `fmt::layer` 上。由于 `non_blocking` 会把写入串行化
/// 到单个 worker 线程，本类型的方法只会被单线程调用，因此内部无需加锁。
pub struct SizeRotatingWriter {
    dir: PathBuf,
    file_name: String,
    max_bytes: u64,
    max_files: Option<usize>,
    file: Option<File>,
    current_size: u64,
    last_archive: Option<ArchiveKey>,
}

impl SizeRotatingWriter {
    /// 创建 writer。
    ///
    /// - `log_dir`: 日志目录，不存在时会尝试创建。
    /// - `log_file`: 当前日志文件名（如 `rs_log.log`）。
    /// - `max_bytes`: 单个文件大小上限（字节），超过即旋转。
    /// - `max_files`: 日志文件总数量上限（**含正在写的当前文件**）。
    ///   `None` 或 `0` 表示不限制。
    pub fn new(
        log_dir: impl AsRef<Path>,
        log_file: impl Into<String>,
        max_bytes: u64,
        max_files: Option<usize>,
    ) -> io::Result<Self> {
        let dir = normalize_log_dir(log_dir);
        let file_name = log_file.into();
        // 大小为 0 无意义，至少留 1 字节以避免每次写入都旋转。
        let max_bytes = max_bytes.max(1);

        let file = create_writer(&dir.join(&file_name))?;
        let current_size = file.metadata().map(|meta| meta.len()).unwrap_or(0);

        let mut writer = Self {
            dir,
            file_name,
            max_bytes,
            max_files,
            file: Some(file),
            current_size,
            last_archive: None,
        };
        // 重启先恢复编号再清理旧归档；当前文件仍追加写，不主动切割。
        // 启动只扫描一次，恢复与清理共用结果；扫描不完整时返回错误，避免编号回退。
        let archived = writer.archive_entries()?;
        writer.last_archive = archived.iter().map(|(key, _)| *key).max();
        writer.prune_from(archived);
        Ok(writer)
    }

    fn base_path(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }

    /// 重新打开当前文件，并同步已写字节数。
    fn reopen_base(&mut self) -> io::Result<()> {
        let file = create_writer(&self.base_path())?;
        self.current_size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        self.file = Some(file);
        Ok(())
    }

    /// 分配递增的归档名称；清理旧文件或时钟回拨都不能让编号回退。
    fn next_rotated_path(&mut self, now: NaiveDateTime) -> io::Result<PathBuf> {
        let mut key = ArchiveKey {
            timestamp: now.trunc_subsecs(3),
            index: 0,
        };
        if let Some(last) = self.last_archive
            && key.timestamp <= last.timestamp
        {
            key = last.next()?;
        }

        loop {
            let timestamp = key.timestamp.format(TIMESTAMP_FORMAT);
            let name = if key.index == 0 {
                format!("{}.{}", self.file_name, timestamp)
            } else {
                format!("{}.{}_{}", self.file_name, timestamp, key.index)
            };
            let candidate = self.dir.join(name);
            if !candidate.try_exists()? {
                // 即使后续改名失败也保留已分配的编号，允许跳号但不复用。
                self.last_archive = Some(key);
                return Ok(candidate);
            }
            key = key.next()?;
        }
    }

    /// 旋转：关闭当前文件 → 改名归档 → 新建当前文件 → 清理超出的旧文件。
    ///
    /// 任何一步失败都会尽量恢复到"仍能继续写旧文件"的状态，并返回错误；调用方
    /// （`tracing-appender` worker）会按既有策略静默吞掉该错误，绝不 panic。
    fn rotate(&mut self) -> io::Result<()> {
        self.rotate_at(Local::now().naive_local())
    }

    fn rotate_at(&mut self, now: NaiveDateTime) -> io::Result<()> {
        // 路径分配可能失败，成功前保留当前句柄以便继续追加日志。
        let base = self.base_path();
        let rotated = self.next_rotated_path(now)?;

        // 关闭句柄：Windows 下持有句柄的文件无法被改名。
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
        self.file = None;

        if let Err(err) = fs::rename(&base, &rotated) {
            // 改名失败：重新打开当前文件继续写。
            let _ = self.reopen_base();
            return Err(err);
        }

        match create_writer(&base) {
            Ok(file) => {
                self.file = Some(file);
                self.current_size = 0;
            }
            Err(err) => {
                // 新建失败：回滚改名，再重新打开原文件继续写。
                let _ = fs::rename(&rotated, &base);
                let _ = self.reopen_base();
                return Err(err);
            }
        }

        self.prune();
        Ok(())
    }

    // 合法名称均预留编号，清理时再筛选确认是普通文件的条目。
    fn archive_entries(&self) -> io::Result<Vec<(ArchiveKey, fs::DirEntry)>> {
        let prefix = format!("{}.", self.file_name);
        let mut archived = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            // 枚举失败可能遗漏最高归档编号，必须传播错误。
            let entry = entry?;
            let name = entry.file_name();
            let Some(key) = name
                .to_str()
                .and_then(|name| parse_archive_key(name, &prefix))
            else {
                continue;
            };
            archived.push((key, entry));
        }
        Ok(archived)
    }

    /// 重新扫描目录后清理（旋转路径使用）。
    fn prune(&self) {
        if matches!(self.max_files, None | Some(0)) {
            return;
        }

        match self.archive_entries() {
            Ok(archived) => self.prune_from(archived),
            Err(err) => eprintln!("Couldn't read log directory: {err}"),
        }
    }

    /// 用已扫描到的条目清理旧归档，使日志文件总数不超过 `max_files`。
    ///
    /// 只删除能确认是普通文件的条目；名称合法但类型无法确认的条目不参与删除。
    fn prune_from(&self, mut archived: Vec<(ArchiveKey, fs::DirEntry)>) {
        let max_files = match self.max_files {
            Some(n) if n > 0 => n,
            _ => return, // 不限制
        };
        // 当前文件占 1 个名额，归档文件最多保留 max_files - 1 个。
        let keep = max_files.saturating_sub(1);

        archived.retain(|(_, entry)| entry.file_type().is_ok_and(|kind| kind.is_file()));

        if archived.len() <= keep {
            return;
        }
        archived.sort_unstable_by_key(|(key, _)| *key);
        let remove_count = archived.len() - keep;
        for (_, entry) in archived.into_iter().take(remove_count) {
            let path = entry.path();
            if let Err(err) = fs::remove_file(&path) {
                eprintln!("Failed to remove old log file {}: {err}", path.display());
            }
        }
    }
}

impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // 降级路径下句柄可能缺失，尝试补开。
        if self.file.is_none() {
            self.reopen_base()?;
        }

        // 已有内容且再写会超标才旋转，避免对单个超大写入产生空归档文件。
        if self.current_size > 0
            && self.current_size + buf.len() as u64 > self.max_bytes
            && let Err(err) = self.rotate()
        {
            eprintln!("Couldn't rotate log file: {err}");
        }

        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is unavailable"))?;
        let written = file.write(buf)?;
        self.current_size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// 以追加模式打开文件；目录不存在时先创建目录再重试（与 `tracing-appender` 一致）。
fn create_writer(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .or_else(|_| {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            OpenOptions::new().append(true).create(true).open(path)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 返回目录下所有与该日志文件相关的文件（当前文件 + 归档文件），按名字排序。
    ///
    /// 注意：Windows 下目录项缓存的文件大小要等句柄关闭后才刷新，因此测试必须
    /// 先 drop writer 再调用本函数读取大小。
    fn read_files(dir: &Path, file_name: &str) -> Vec<(PathBuf, u64)> {
        let mut files: Vec<(PathBuf, u64)> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(file_name))
            .map(|entry| {
                let path = entry.path();
                let size = entry.metadata().unwrap().len();
                (path, size)
            })
            .collect();
        files.sort();
        files
    }

    fn read_log_contents(dir: &Path) -> Vec<String> {
        let mut contents: Vec<_> = read_files(dir, "app.log")
            .into_iter()
            .map(|(path, _)| fs::read_to_string(path).unwrap())
            .collect();
        contents.sort();
        contents
    }

    fn fixed_time() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-01-01T00-00-00.123", TIMESTAMP_FORMAT).unwrap()
    }

    #[test]
    fn rotates_when_exceeding_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 100, None).unwrap();
            for _ in 0..10 {
                writer.write_all(&[b'x'; 60]).unwrap();
            }
            writer.flush().unwrap();
        }

        let files = read_files(dir.path(), "app.log");
        // 1 个当前文件 + 9 个归档文件
        assert_eq!(files.len(), 10);
        // 旋转后数据无丢失
        assert_eq!(files.iter().map(|(_, size)| size).sum::<u64>(), 600);
        // 当前文件大小不超过上限
        let current = files
            .iter()
            .find(|(path, _)| path.file_name().unwrap().to_string_lossy() == "app.log")
            .unwrap();
        assert!(
            current.1 <= 100,
            "current file should not exceed max_bytes, got {}",
            current.1
        );
    }

    #[test]
    fn prunes_files_beyond_max_files() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 100, Some(2)).unwrap();
            for _ in 0..10 {
                writer.write_all(&[b'x'; 60]).unwrap();
            }
            writer.flush().unwrap();
        }

        // 最多保留 2 个文件（含当前文件）
        assert_eq!(read_files(dir.path(), "app.log").len(), 2);
    }

    #[test]
    fn startup_prunes_archives_and_keeps_appending_current_file() {
        for max_files in [None, Some(0), Some(1), Some(2)] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("app.log"), b"current").unwrap();
            for index in [1, 2, 10, 11] {
                let name = format!("app.log.2026-01-01T00-00-00.123_{index}");
                fs::write(dir.path().join(name), format!("archive {index}")).unwrap();
            }

            let mut writer =
                SizeRotatingWriter::new(dir.path(), "app.log", 1_000, max_files).unwrap();
            let expected_count = max_files.filter(|n| *n > 0).unwrap_or(5);
            assert_eq!(read_files(dir.path(), "app.log").len(), expected_count);
            assert_eq!(fs::read(dir.path().join("app.log")).unwrap(), b"current");
            if max_files == Some(2) {
                assert_eq!(read_log_contents(dir.path()), ["archive 11", "current"]);
            }

            writer.write_all(b" appended").unwrap();
            drop(writer);
            assert_eq!(
                fs::read(dir.path().join("app.log")).unwrap(),
                b"current appended"
            );
            assert_eq!(read_files(dir.path(), "app.log").len(), expected_count);
        }
    }

    #[test]
    fn startup_returns_scan_error_even_when_log_file_opens() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("app.log");
        let missing_dir = dir.path().join("missing");
        for max_files in [None, Some(0), Some(2)] {
            // 绝对日志路径让文件可正常打开，而归档目录仍不存在。
            let err = SizeRotatingWriter::new(
                &missing_dir,
                log_path.to_string_lossy().into_owned(),
                100,
                max_files,
            )
            .err()
            .expect("startup must return the archive directory scan error");
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
            assert!(log_path.is_file());
        }
    }

    #[test]
    fn archive_names_reserve_sequence_without_deleting_directories() {
        let dir = tempfile::tempdir().unwrap();
        let named_dir = dir.path().join("app.log.2026-01-01T00-00-00.123_20");
        fs::create_dir(&named_dir).unwrap();
        let older_archive = dir.path().join("app.log.2026-01-01T00-00-00.123_1");
        fs::write(&older_archive, b"older").unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 3, Some(2)).unwrap();
            // 同名目录不占归档名额，启动时应保留这唯一的旧归档。
            assert_eq!(fs::read(&older_archive).unwrap(), b"older");
            writer.write_all(b"old").unwrap();
            writer.rotate_at(fixed_time()).unwrap();
            writer.write_all(b"new").unwrap();
        }

        assert!(named_dir.is_dir());
        assert!(!older_archive.exists());
        assert_eq!(
            fs::read(dir.path().join("app.log.2026-01-01T00-00-00.123_21")).unwrap(),
            b"old"
        );
        assert_eq!(fs::read(dir.path().join("app.log")).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[test]
    fn pruning_preserves_non_archive_files() {
        let dir = tempfile::tempdir().unwrap();
        let backups = [
            "app.log.1.bak",
            "app.log.2000-01-01",
            "app.log.2000-01-01T00-00-00.000.bak",
            "app.log.2000-01-01T00-00-00.000_1.bak",
            "app.log.2000-01-01T00-00-00.000_0",
            "app.log.2000-01-01T00-00-00.000_01",
            "app.log.2000-01-01T00-00-00.000_+1",
            "app.log.2000-01-01T00-00-00.0000",
            "app.log.2000-02-30T00-00-00.000",
        ];
        for name in backups {
            fs::write(dir.path().join(name), b"backup").unwrap();
        }
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 3, Some(1)).unwrap();
            writer.write_all(b"old").unwrap();
            writer.write_all(b"new").unwrap();
        }

        for name in backups {
            assert_eq!(fs::read(dir.path().join(name)).unwrap(), b"backup");
        }
        assert_eq!(fs::read(dir.path().join("app.log")).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), backups.len() + 1);
    }

    #[test]
    fn prunes_using_numeric_archive_sequence() {
        let dir = tempfile::tempdir().unwrap();
        for index in [1, 2, 10] {
            let name = format!("app.log.2000-01-01T00-00-00.000_{index}");
            fs::write(dir.path().join(name), format!("archive {index}")).unwrap();
        }
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 3, Some(3)).unwrap();
            writer.write_all(b"old").unwrap();
            writer.write_all(b"new").unwrap();
        }

        assert_eq!(read_log_contents(dir.path()), ["archive 10", "new", "old"]);
    }

    #[test]
    fn same_millisecond_rotation_keeps_latest_contents() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 2, Some(2)).unwrap();
            writer.write_all(b"00").unwrap();
            for index in 1..20 {
                // 纳秒值不同，但归档名称中的毫秒值相同。
                let now = fixed_time() + chrono::TimeDelta::microseconds(index);
                writer.rotate_at(now).unwrap();
                writer.write_all(format!("{index:02}").as_bytes()).unwrap();
            }
        }

        assert_eq!(read_log_contents(dir.path()), ["18", "19"]);
    }

    #[test]
    fn restart_restores_latest_archive_sequence() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 2, Some(3)).unwrap();
            writer.write_all(b"00").unwrap();
            for index in 1..12 {
                writer.rotate_at(fixed_time()).unwrap();
                writer.write_all(format!("{index:02}").as_bytes()).unwrap();
            }
        }
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 2, Some(3)).unwrap();
            assert_eq!(read_log_contents(dir.path()), ["09", "10", "11"]);
            assert_eq!(fs::read(dir.path().join("app.log")).unwrap(), b"11");
            writer.rotate_at(fixed_time()).unwrap();
            writer.write_all(b"12").unwrap();
        }

        assert_eq!(read_log_contents(dir.path()), ["10", "11", "12"]);
    }

    #[test]
    fn clock_rollback_keeps_latest_contents() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 2, Some(2)).unwrap();
            writer.write_all(b"00").unwrap();
            writer.rotate_at(fixed_time()).unwrap();
            writer.write_all(b"01").unwrap();
            writer
                .rotate_at(fixed_time() - chrono::TimeDelta::hours(1))
                .unwrap();
            writer.write_all(b"02").unwrap();
        }

        assert_eq!(read_log_contents(dir.path()), ["01", "02"]);
    }

    #[test]
    fn keeps_all_files_when_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        {
            // max_files = Some(0) 视为不限制
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 100, Some(0)).unwrap();
            for _ in 0..5 {
                writer.write_all(&[b'x'; 60]).unwrap();
            }
            writer.flush().unwrap();
        }

        assert_eq!(read_files(dir.path(), "app.log").len(), 5);
    }

    #[test]
    fn does_not_rotate_empty_file_for_oversized_single_write() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 10, None).unwrap();
            writer.write_all(&[b'x'; 50]).unwrap();
            writer.flush().unwrap();
        }

        // 单个超大写入不应产生空的归档文件
        let files = read_files(dir.path(), "app.log");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, 50);
    }

    #[test]
    fn appends_to_existing_file_on_start() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 1_000, None).unwrap();
            writer.write_all(b"hello").unwrap();
            writer.flush().unwrap();
        }
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 1_000, None).unwrap();
            assert_eq!(read_files(dir.path(), "app.log").len(), 1);
            writer.write_all(b"world").unwrap();
            writer.flush().unwrap();
        }

        let content = fs::read_to_string(dir.path().join("app.log")).unwrap();
        assert_eq!(content, "helloworld");
        assert_eq!(read_files(dir.path(), "app.log").len(), 1);
    }

    #[test]
    fn full_file_rotates_only_on_first_write_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("app.log"), b"old").unwrap();
        {
            let mut writer = SizeRotatingWriter::new(dir.path(), "app.log", 3, None).unwrap();
            assert_eq!(read_log_contents(dir.path()), ["old"]);
            writer.write_all(b"new").unwrap();
        }

        assert_eq!(read_log_contents(dir.path()), ["new", "old"]);
    }
}
