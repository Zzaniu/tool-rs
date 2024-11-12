use super::bucket::Bucket;

#[derive(Debug)]
pub(super) struct Window {
    /// 所有的桶
    buckets: Vec<Bucket>,
    /// 桶的数量
    size: usize,
}

impl Window {
    pub(super) fn new(size: usize) -> Self {
        let mut buckets = Vec::with_capacity(size);
        for _ in 0..size {
            buckets.push(Bucket::default());
        }
        Self { buckets, size }
    }

    /// 给哪个桶加一个值
    pub(super) fn add(&mut self, offset: usize, val: u64) {
        self.buckets[offset].add(val);
    }

    /// 重置某个桶
    pub(super) fn reset_bucket(&mut self, offset: usize) {
        self.buckets[offset % self.size].reset();
    }

    /// 从 offset 开始, 重置 count 个桶
    pub(super) fn reset_buckets(&mut self, offset: usize, count: usize) {
        for i in 0..count {
            self.reset_bucket(offset + i);
        }
    }

    /// 从 start 开始, 经过 end 个桶, 统计这些桶的总和
    pub(super) fn reduce(&self, start: usize, end: usize) -> (u64, u64) {
        let mut accept = 0u64;
        let mut total = 0u64;
        for offset in 0..end {
            let bucket = &self.buckets[(start + offset) % self.size];
            accept += bucket.sum;
            total += bucket.total;
        }
        (accept, total)
    }
}
