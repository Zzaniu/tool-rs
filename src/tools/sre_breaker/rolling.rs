use super::window::Window;
use parking_lot::RwLock;
use std::ops::Add;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) struct RollingPolicy {
    mutex: RwLock<Inner>,
    size: usize,
    bucket_duration: Duration,
}

#[derive(Debug)]
struct Inner {
    window: Window,
    offset: usize,
    last_append_time: Instant,
}

impl Default for RollingPolicy {
    fn default() -> Self {
        RollingPolicy::new(10, Duration::from_millis(100))
    }
}

impl RollingPolicy {
    pub fn new(size: usize, bucket_duration: Duration) -> RollingPolicy {
        RollingPolicy {
            size,
            bucket_duration,
            mutex: RwLock::new(Inner {
                window: Window::new(size),
                offset: 0,
                last_append_time: Instant::now(),
            }),
        }
    }

    /// 往桶中添加一个值, 并更新 offset
    pub fn add(&self, val: u64) {
        let mut guard = self.mutex.write();
        let raw_move_bucket_count = self.timespan(guard.last_append_time);
        let offset = guard.offset;
        if raw_move_bucket_count > 0 {
            let mut move_bucket_count = raw_move_bucket_count;
            // 划过的桶如果大于 size, 就设置为 size
            if move_bucket_count > self.size {
                move_bucket_count = self.size;
            }
            // 把划过的桶重置下
            guard
                .window
                .reset_buckets((offset + 1) % self.size, move_bucket_count);
            // 更新 offset
            guard.offset = (guard.offset + move_bucket_count) % self.size;
            // 更新 last_append_time
            guard.last_append_time = guard
                .last_append_time
                .add(self.bucket_duration * raw_move_bucket_count as u32);
        }
        // 对应 bucket 中增加一个值
        guard.window.add(offset, val);
    }

    /// 计算当前滑块滑过了几个 bucket
    pub fn timespan(&self, last_append_time: Instant) -> usize {
        // 这里没有四舍五入, 直接是向下取整的
        let span = (Instant::now().duration_since(last_append_time).as_millis()
            / self.bucket_duration.as_millis()) as i32;
        if span > -1 {
            return span as usize;
        }
        self.size
    }

    /// 统计滑过的桶之外的其他桶的总和
    pub fn reduce(&self) -> (u64, u64) {
        let guard = self.mutex.read();
        let move_bucket_count = self.timespan(guard.last_append_time);
        let stat_count = if move_bucket_count == 0 {
            // 把当前桶排除
            self.size as i64 - 1
        } else {
            // 把划过的桶排除
            self.size as i64 - move_bucket_count as i64
        };
        if stat_count > 0 {
            let offset = (guard.offset + move_bucket_count + 1) % self.size;
            return guard.window.reduce(offset, stat_count as usize);
        }
        (0, 0)
    }

    pub fn summary(&self) -> (u64, u64) {
        self.reduce()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn test_instant() {
        let start = Instant::now();
        thread::sleep(Duration::from_secs(1));
        let end = Instant::now();
        let x = end.duration_since(start);
        println!("{:?}", x);
    }

    #[test]
    fn test_rolling() {
        let x = 1.8 as i32;
        println!("x = {}", x);
        let r = RollingPolicy::new(10, Duration::from_millis(300)); // 总共 10 个桶, 一个桶 300 ms
        println!("r = {:?}", r);
    }
}
