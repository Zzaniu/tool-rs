//! 基于 lapin 的 RabbitMQ 客户端封装。
//!
//! 与 `crate::mq::rabbitmq` 的关系：两者能力对齐，但底层客户端不同。
//! 本模块直接复用 lapin 的连接、通道与确认类型，并通过 [`mod@publisher`]
//! 提供「单连接、单通道、多消息在途」的并发发布能力。
//!
//! 与 amqprs 版本的主要差异：
//!
//! - 每条消息的确认结果由 lapin 的 `PublisherConfirm` 独立兑现，不需要自研
//!   确认状态机，也不需要通道池；
//! - 连接断开时 lapin 会主动拒绝所有待确认的 `PublisherConfirm`，等待中的
//!   发布调用不会挂起；
//! - `RETURN` 与确认结果由 lapin 合并到同一个 `Confirmation` 中，本模块据此判断
//!   消息是否真的路由到了队列；但 lapin 按 FIFO 配对，因此
//!   `PublishSuccessMode::RoutedToQueue` 要求 `max_inflight == 1`，且一旦某次发布
//!   在未取得确认的情况下结束（超时或取消），后续发布会返回 `AttributionLost`，
//!   需要重建发布器。
//!
//! 帧入队阶段由发布器内部加锁串行化，避免确认编号与线上顺序错位；等待确认阶段
//! 仍然并发。连接处于服务端资源告警的阻塞态时，发布直接返回失败而不是等超时。
//!
//! 本模块不做自动重试、不做业务幂等，也不声明 exchange 与 queue。

pub use lapin;

pub mod connect;
pub mod consumer;
pub mod mail;
pub mod publisher;
