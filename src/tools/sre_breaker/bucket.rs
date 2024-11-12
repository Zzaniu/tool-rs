#[derive(Debug, Default)]
pub(super) struct Bucket {
    /// 桶中成功的数量
    pub(super) sum: u64,
    /// 桶中总数量
    pub(super) total: u64,
}

impl Bucket {
    /// Add a value to the bucket
    pub(super) fn add(&mut self, val: u64) {
        self.sum += val;
        self.total += 1;
    }

    /// 重置桶
    pub(super) fn reset(&mut self) {
        self.sum = 0;
        self.total = 0;
    }
}
