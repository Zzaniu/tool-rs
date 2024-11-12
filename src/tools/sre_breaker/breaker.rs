use super::rolling::RollingPolicy;
use rand::{self, random};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("circuitbreaker: not allowed for circuit open")]
    CircuitOpenError,
    #[error(transparent)]
    UnexpectedError(#[from] anyhow::Error),
}

#[derive(Debug, derive_builder::Builder)]
/// google sre 弹性熔断器
pub struct SreBreaker {
    /// 系数, 熔断触发范围
    #[builder(default = "1f64 / 0.9f64")]
    k: f64,
    /// 滑动窗口
    #[builder(setter(skip), default = "RollingPolicy::default()")]
    policy: RollingPolicy,

    /// 请求数小于这个数字, 直接忽略
    #[builder(default = "5")]
    requests: u64,
}

impl Default for SreBreaker {
    fn default() -> Self {
        // 默认请求拒绝率小于 0.11 就不触发熔断
        SreBreaker::new(1f64 / 0.9f64, 5)
    }
}

impl SreBreaker {
    pub fn new(k: f64, requests: u64) -> SreBreaker {
        SreBreaker {
            k,
            requests,
            policy: RollingPolicy::default(),
        }
    }

    pub fn allow(&self) -> Result<(), Error> {
        // 获取一段时间内的所有请求数和接受的请求数
        let (accept, req_total) = self.policy.summary();
        // 接收的请求数乘上我们设置的系数, 用这个来代替总的接收数量
        let requests = self.k * accept as f64;
        // 如果请求数不足, 或者总的接收数量大于总请求数, 不会触发熔断
        if req_total < self.requests || (req_total as f64) < requests {
            return Ok(());
        }
        // dr 越大, 说明被拒绝的请求越多, 那么越容易触发熔断
        let dr = 0f64.max(((req_total as f64) - requests) / ((req_total + 1) as f64));
        // 在配置的接受范围之内, 不会触发熔断
        if dr < 0f64 {
            return Ok(());
        }
        if self.true_on_proba(dr) {
            return Err(Error::CircuitOpenError);
        }
        Ok(())
    }

    fn true_on_proba(&self, proba: f64) -> bool {
        // 随机生成一个 0 ~ 1 之间的小数,
        // 如果 proba 越大, 随机数小于它的概率就越大
        random::<f64>() < proba
    }

    pub fn mark_success(&self) {
        self.policy.add(1);
    }

    pub fn mark_failed(&self) {
        self.policy.add(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_rang() {
        let i = random::<f64>();
        println!("{}", i);
    }

    #[test]
    fn test_sre() {
        let mut breaker = SreBreaker::default();
        println!("breaker = {breaker:?}");
        for i in 0..1000 {
            match breaker.allow() {
                Ok(_) => {
                    println!("allow");
                }
                Err(err) => {
                    println!("err = {err}, breaker = {breaker:?}");
                    breaker.mark_failed();
                    continue;
                }
            }

            thread::sleep(Duration::from_millis(1));
            if random::<f64>() > 0.8f64 {
                breaker.mark_failed();
            } else {
                breaker.mark_success();
            }
        }
    }

    #[test]
    fn test_sre_builder() {
        let breaker = SreBreakerBuilder::default().build().unwrap();
        println!("breaker = {breaker:?}");
    }
}
