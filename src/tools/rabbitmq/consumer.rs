//! 消费者订阅封装。
//!
//! 只做两件事：按配置设置 prefetch，然后创建 lapin 的 [`Consumer`]。消费循环、
//! ack/nack 与错误处理都由调用方负责——本模块不隐藏后台任务，也不替调用方
//! 决定重投递策略。

use crate::rabbitmq::lapin;
use lapin::options::{BasicConsumeOptions, BasicQosOptions};
use lapin::types::FieldTable;
use lapin::{Channel, Consumer};

/// 订阅配置。
///
/// 使用 [`ConsumerConfig::new`] 创建，再按需设置其他字段。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ConsumerConfig {
    /// 队列名，队列必须已存在（本模块不声明队列）。
    pub queue: String,
    /// 消费者标签；`None` 或空串表示由服务端生成。
    pub consumer_tag: Option<String>,
    /// 是否关闭手动确认。为 true 时消息在投递后立即从队列移除。
    pub no_ack: bool,
    /// 单个消费者的预取上限；`None` 表示不设置（使用服务端或通道既有值）。
    pub prefetch: Option<u16>,
}

impl ConsumerConfig {
    /// 创建订阅配置：不关闭手动确认，不设置预取。
    pub fn new(queue: impl Into<String>) -> Self {
        Self {
            queue: queue.into(),
            consumer_tag: None,
            no_ack: false,
            prefetch: None,
        }
    }

    /// 设置消费者标签。
    pub fn with_consumer_tag(mut self, consumer_tag: impl Into<String>) -> Self {
        self.consumer_tag = Some(consumer_tag.into());
        self
    }

    /// 设置是否关闭手动确认。
    pub fn with_no_ack(mut self, no_ack: bool) -> Self {
        self.no_ack = no_ack;
        self
    }

    /// 设置预取上限。
    pub fn with_prefetch(mut self, prefetch: u16) -> Self {
        self.prefetch = Some(prefetch);
        self
    }
}

/// 在通道上创建消费者。
///
/// 配置了 [`ConsumerConfig::prefetch`] 时先调用 `basic.qos`（仅作用于当前通道，
/// `global = false`），再订阅队列。
///
/// 返回的 [`Consumer`] 是一个 `Stream`，每项为 `lapin::Result<Delivery>`；调用方
/// 用 `Delivery::ack` / `Delivery::nack` 显式确认。连接断开时该 `Stream` 会产出
/// 错误而不是永久挂起。
pub async fn subscribe(channel: &Channel, config: &ConsumerConfig) -> lapin::Result<Consumer> {
    if let Some(prefetch) = config.prefetch {
        channel
            .basic_qos(prefetch, BasicQosOptions { global: false })
            .await?;
    }
    let consumer_tag = config.consumer_tag.clone().unwrap_or_default();
    channel
        .basic_consume(
            config.queue.clone().into(),
            consumer_tag.into(),
            BasicConsumeOptions {
                no_ack: config.no_ack,
                ..BasicConsumeOptions::default()
            },
            FieldTable::default(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认配置保持手动确认且不覆盖服务端预取设置。
    #[test]
    fn default_config_keeps_manual_ack() {
        let config = ConsumerConfig::new("queue");
        assert_eq!(config.queue, "queue");
        assert!(config.consumer_tag.is_none());
        assert!(!config.no_ack);
        assert!(config.prefetch.is_none());
    }

    /// 构建方法只覆盖指定字段。
    #[test]
    fn builder_methods_override_single_field() {
        let config = ConsumerConfig::new("queue")
            .with_consumer_tag("tag")
            .with_no_ack(true)
            .with_prefetch(32);

        assert_eq!(config.consumer_tag.as_deref(), Some("tag"));
        assert!(config.no_ack);
        assert_eq!(config.prefetch, Some(32));
    }
}
