//! 基于 RabbitMQ 发布确认机制的串行消息发布器。
//!
//! 每个发布器独占一个启用发布确认的通道，将消息加入客户端发送队列后，
//! 继续等待服务端的 ACK、NACK 或关闭通知。默认只要求服务端确认接收，
//! 也可以配置为要求消息至少路由到一个队列；两种模式都不等待消费者处理。
//!
//! 发布器不自动重连或重发。调用方可通过 [`RabbitMqPublishError::is_outcome_unknown`]
//! 判断失败时消息结果是否已确定，并通过 [`RabbitMqPublisher::is_usable`]
//! 判断是否需要重新创建发布器。

use amqprs::channel::{BasicPublishArguments, Channel, ConfirmSelectArguments};
use amqprs::connection::Connection;
use amqprs::{Ack, BasicProperties, Cancel, CloseChannel, Nack, Return, callbacks};
use async_trait::async_trait;
use log::{debug, warn};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Instant, sleep_until, timeout_at};

/// 默认的入队与确认总超时时间，不包含等待其他发布调用结束的时间。
const DEFAULT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);

/// 发布成功的判断方式。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PublishSuccessMode {
    /// RabbitMQ 返回 Publisher Confirm ACK 即成功，不要求消息路由到队列。
    ///
    /// 这是默认模式；没有匹配队列的消息仍可能被丢弃，成功不代表消费者已处理。
    #[default]
    BrokerAccepted,
    /// RabbitMQ 返回 ACK 且消息没有被 RETURN，才算至少路由到一个队列。
    ///
    /// 发布时会启用 `mandatory`，并依赖不可路由消息先收到 RETURN、再收到确认的顺序。
    /// 路由成功仍不代表消费者已接收或处理消息。
    RoutedToQueue,
}

/// V2 publisher 配置。
///
/// 使用默认配置或构建方法创建，以便后续版本兼容地增加配置项。
/// 默认超时时间为 3 秒，默认成功模式为 [`PublishSuccessMode::BrokerAccepted`]。
///
///     use std::time::Duration;
///     use tool::mq::rabbitmq::v2::{PublishSuccessMode, RabbitMqPublisherConfig};
///
///     let config = RabbitMqPublisherConfig::new()
///         .with_confirm_timeout(Duration::from_secs(10))
///         .with_success_mode(PublishSuccessMode::RoutedToQueue);
///     assert_eq!(config.success_mode, PublishSuccessMode::RoutedToQueue);
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RabbitMqPublisherConfig {
    /// 客户端发送队列入队和等待 Publisher Confirm 共用的最长时间。
    ///
    /// 默认 3 秒，从取得串行发布锁并通过可用性检查后开始计时。
    /// 不包含等待同一个 publisher 的其他调用结束的时间。
    pub confirm_timeout: Duration,
    /// 发布成功的判断方式。
    ///
    /// 默认仅要求服务端返回 ACK；需要确认路由结果时使用 [`PublishSuccessMode::RoutedToQueue`]。
    pub success_mode: PublishSuccessMode,
}

impl Default for RabbitMqPublisherConfig {
    /// 使用 3 秒总超时，并以服务端 ACK 作为成功条件。
    fn default() -> Self {
        Self {
            confirm_timeout: DEFAULT_CONFIRM_TIMEOUT,
            success_mode: PublishSuccessMode::default(),
        }
    }
}

impl RabbitMqPublisherConfig {
    /// 创建默认配置：总超时 3 秒，收到服务端 ACK 即认为发布成功。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置客户端发送队列入队和等待 Publisher Confirm 的总超时时间。
    ///
    /// 此方法只保存配置；创建发布器或发布消息时会检查计时器能否表示该时长。
    pub fn with_confirm_timeout(mut self, confirm_timeout: Duration) -> Self {
        self.confirm_timeout = confirm_timeout;
        self
    }

    /// 设置发布成功的判断方式。
    ///
    /// 路由模式会统一启用 `mandatory`，覆盖每次发布参数中的同名设置。
    pub fn with_success_mode(mut self, success_mode: PublishSuccessMode) -> Self {
        self.success_mode = success_mode;
        self
    }

    /// 根据当前时刻计算绝对截止时间，拒绝会导致计时器溢出的配置。
    fn deadline(&self) -> Result<Instant, RabbitMqPublishError> {
        Instant::now()
            .checked_add(self.confirm_timeout)
            .ok_or(RabbitMqPublishError::InvalidConfirmTimeout)
    }
}

/// 专用通道回调产生的事件，按回调发送顺序交给当前发布调用处理。
#[derive(Debug)]
enum PublishEvent {
    /// 服务端确认接收一条或一批消息。
    Ack {
        /// 服务端确认帧中的消息编号。
        tag: u64,
        /// 是否同时确认该编号及之前尚未确认的消息。
        multiple: bool,
    },
    /// 服务端拒绝一条或一批消息。
    Nack {
        /// 服务端拒绝帧中的消息编号。
        tag: u64,
        /// 是否同时拒绝该编号及之前尚未确认的消息。
        multiple: bool,
    },
    /// 消息被服务端退回；RETURN 不携带用于匹配消息的确认编号。
    Returned {
        /// 服务端提供的退回原因，包含响应码、交换器和路由键等信息。
        detail: String,
    },
    /// 服务端关闭通道，携带与关闭原因缓存共享的原始关闭帧。
    Closed(Arc<CloseChannel>),
}

/// V2 publisher 的专用 channel 回调。
struct PublisherCallback {
    /// 发送确认、退回和关闭事件；无界通道使回调无需等待发布调用消费。
    events: mpsc::UnboundedSender<PublishEvent>,
    /// 与发布器共享的停用标志，通道关闭时立即置为 true。
    unusable: Arc<AtomicBool>,
    /// 只记录首次关闭原因；内层 Arc 使缓存、事件和错误共享同一份关闭帧。
    close_details: Arc<OnceLock<Arc<CloseChannel>>>,
}

impl PublisherCallback {
    /// 保存关闭原因并停用发布器，再发送事件唤醒正在等待确认的调用。
    fn on_close(&self, close: CloseChannel) {
        // CloseChannel 没有实现 Clone，通过 Arc 共享原始帧，避免复制响应字段。
        let close = Arc::new(close);
        // 先保存原因，再公开不可用状态，保证早退路径也能读取 broker 的关闭详情。
        let _ = self.close_details.set(Arc::clone(&close));
        self.unusable.store(true, Ordering::Release);
        let _ = self.events.send(PublishEvent::Closed(close));
    }
}

#[async_trait]
impl callbacks::ChannelCallback for PublisherCallback {
    /// 处理服务端的通道关闭请求，返回成功以允许 amqprs 应答关闭请求。
    async fn close(
        &mut self,
        _channel: &Channel,
        close: CloseChannel,
    ) -> Result<(), amqprs::error::Error> {
        self.on_close(close);
        Ok(())
    }

    /// 此专用通道不创建消费者，无需处理消费者取消通知。
    async fn cancel(
        &mut self,
        _channel: &Channel,
        _cancel: Cancel,
    ) -> Result<(), amqprs::error::Error> {
        Ok(())
    }

    /// 此封装不维护额外的通道流控状态，统一以 active=true 应答流控请求。
    async fn flow(
        &mut self,
        _channel: &Channel,
        _active: bool,
    ) -> Result<bool, amqprs::error::Error> {
        Ok(true)
    }

    /// 转发 ACK 的确认编号和批量标记，由当前发布调用判断是否匹配。
    async fn publish_ack(&mut self, _channel: &Channel, ack: Ack) {
        let _ = self.events.send(PublishEvent::Ack {
            tag: ack.delivery_tag(),
            multiple: ack.mutiple(),
        });
    }

    /// 转发 NACK 的确认编号和批量标记，保留服务端明确拒绝消息的结论。
    async fn publish_nack(&mut self, _channel: &Channel, nack: Nack) {
        let _ = self.events.send(PublishEvent::Nack {
            tag: nack.delivery_tag(),
            multiple: nack.multiple(),
        });
    }

    /// 只保留退回原因，消息属性和正文不参与成功判定。
    async fn publish_return(
        &mut self,
        _channel: &Channel,
        ret: Return,
        _properties: BasicProperties,
        _content: Vec<u8>,
    ) {
        let _ = self.events.send(PublishEvent::Returned {
            detail: ret.to_string(),
        });
    }
}

/// 发布过程的析构保护：异步任务未安全收尾就被丢弃时，保守停用发布器。
///
/// 取消等待可能留下迟到的确认事件，继续复用会使消息与确认编号失去对应关系。
struct InFlightGuard {
    /// 与发布器及回调共享的停用标志。
    unusable: Arc<AtomicBool>,
    /// 是否仍需在析构时停用发布器，正常收尾后解除。
    armed: bool,
}

impl InFlightGuard {
    /// 创建立即生效的保护器，覆盖入队和等待确认两个阶段。
    fn new(unusable: Arc<AtomicBool>) -> Self {
        Self {
            unusable,
            armed: true,
        }
    }

    /// 完成结果处理，或确认入队超时且未发送时，解除析构停用保护。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InFlightGuard {
    /// 未解除保护说明发布流程提前结束，后续调用不得继续复用当前发布器。
    fn drop(&mut self) {
        if self.armed {
            self.unusable.store(true, Ordering::Release);
        }
    }
}

/// 发布或初始化失败；可通过 [`Self::is_outcome_unknown`] 区分结果是否已确定。
///
/// “结果未知”表示消息可能已经满足配置的成功条件，也可能没有，直接重试可能重复。
/// 两个通道关闭变体都保留原始 [`CloseChannel`]，可通过其 `reply_code()`、
/// `reply_text()`、`class_id()` 和 `method_id()` 读取服务端原因。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RabbitMqPublishError {
    /// 邮件发送封装序列化消息失败，本次消息尚未提交发布。
    ///
    /// [`RabbitMqPublisher::publish`] 接收已编码的正文，不产生此错误。
    #[error("序列化消息失败: {0}")]
    Serialization(#[from] serde_json::Error),
    /// 创建专用通道、注册回调或启用发布确认失败，尚未使用该发布器发送消息。
    #[error("RabbitMQ publisher 初始化失败: {0}")]
    Amqp(#[from] amqprs::error::Error),
    /// amqprs 未能将消息加入客户端发送队列，本次消息确定未发布。
    #[error("消息未能加入 RabbitMQ 客户端发送队列，可安全判断为未发布: {0}")]
    PublishFailed(#[source] amqprs::error::Error),
    /// 发布前发现发布器不可用，本次消息确定未发布，且没有缓存的服务端关闭帧。
    ///
    /// 可能由此前的超时、取消、连接关闭或通道关闭引起，需要重新创建发布器。
    #[error("publisher 已不可用，本次消息尚未发布，请重新创建 publisher")]
    PublisherUnavailable,
    /// 发布前 channel 已关闭，保留 broker 返回的原始关闭帧。
    ///
    /// 本次消息确定未发布。关闭帧与内部缓存共享，重建前可读取其中的响应码和原因。
    #[error("publisher 已不可用，channel 已被 RabbitMQ 关闭，本次消息尚未发布: {0}")]
    PublisherUnavailableChannelClosed(Arc<CloseChannel>),
    /// 截止时间内未能入队，本次消息确定未发布，不消耗确认编号。
    ///
    /// 此次超时本身不会停用发布器；能否继续使用仍取决于当前连接和通道状态。
    #[error("发布消息未能在超时前加入 RabbitMQ 客户端发送队列，可安全判断为未发布")]
    PublishTimeout,
    /// 超时时长超出计时器可表示范围，创建通道或发送当前消息前即被拒绝。
    #[error("confirm_timeout 超出计时器可表示范围，消息尚未发布")]
    InvalidConfirmTimeout,
    /// 收到匹配当前消息的 NACK，服务端已明确拒绝该消息。
    #[error("RabbitMQ 拒绝了消息，delivery_tag={delivery_tag}")]
    Nacked {
        /// 服务端 NACK 中的编号；批量拒绝时不一定等于当前消息的编号。
        delivery_tag: u64,
    },
    /// 路由模式下收到 RETURN，已确定消息没有路由到任何队列。
    ///
    /// 即使随后发生断线、关闭或确认超时，仍保留该已知失败结论。
    #[error("消息未路由到任何队列: {detail}")]
    Unroutable {
        /// 服务端 RETURN 帧的文本说明，包含退回原因和路由信息。
        detail: String,
    },
    /// 消息已入队，但截止时间内未得到最终确认，结果未知，发布器被停用。
    #[error("等待 RabbitMQ Publisher Confirm 超时，结果未知，重试可能导致重复")]
    ConfirmTimeout,
    /// 等待确认的本地回调事件通道已结束，结果未知，发布器被停用。
    #[error("Publisher Confirm 回调通道已关闭，结果未知，重试可能导致重复")]
    ConfirmEventsClosed,
    /// 等待确认期间检测到连接结束，结果未知，发布器被停用。
    #[error("RabbitMQ connection 已关闭，结果未知，重试可能导致重复")]
    ConnectionClosed,
    /// 等待确认期间 channel 关闭，保留 broker 返回的原始关闭帧。
    ///
    /// 本次消息已经入队，缺少最终确认，结果未知；原通道和发布器均不可继续使用。
    /// 已收到 RETURN 的路由失败会优先返回 [`Self::Unroutable`]。
    #[error("RabbitMQ channel 在确认消息前关闭，结果未知，重试可能导致重复: {0}")]
    ChannelClosed(Arc<CloseChannel>),
}

impl RabbitMqPublishError {
    /// 返回 true 表示无法确定消息是否满足所选的成功条件，重试可能重复。
    ///
    /// 返回 false 仅表示本次失败的结论已确定，不代表所有错误都可直接重试。
    /// 例如权限或路由配置错误应先修复；结果未知时则需要结合业务幂等机制决定是否重试。
    /// 仅显式列出已确定的失败，以便将来新增错误默认按结果未知处理。
    pub fn is_outcome_unknown(&self) -> bool {
        !matches!(
            self,
            Self::Serialization(_)
                | Self::Amqp(_)
                | Self::PublishFailed(_)
                | Self::PublisherUnavailable
                | Self::PublisherUnavailableChannelClosed(_)
                | Self::PublishTimeout
                | Self::InvalidConfirmTimeout
                | Self::Nacked { .. }
                | Self::Unroutable { .. }
        )
    }
}

/// 使用 Publisher Confirm 的串行 RabbitMQ publisher。
///
/// 自动创建并独占一个启用 confirm 的 channel，不替换 connection 已有的回调。
/// RETURN 不携带 delivery tag，因此不开放内部 channel，避免其他发布混入后无法关联。
/// 同一个 publisher 的并发调用会排队，等待当前消息的确认后才发布下一条消息。
/// 发布器持有传入连接的共享句柄，所有消息仍使用同一个底层连接。
///
/// 超时、断线或确认期间取消 future 后，应检查 [`Self::is_usable`] 并按需重新创建。
/// 本类型不自动重连或重发，结果未知时由调用方决定如何避免重复。
pub struct RabbitMqPublisher {
    /// 此发布器独占的通道，已启用发布确认，不向调用方开放以免混入其他消息。
    channel: Channel,
    /// 原连接的共享句柄，用于检查连接状态和订阅连接结束通知。
    connection: Connection,
    /// 回调事件接收端，仅由持有发布锁的调用消费。
    events: Mutex<mpsc::UnboundedReceiver<PublishEvent>>,
    /// 串行化每次入队与确认过程，确保无编号的 RETURN 可以对应到当前消息。
    publish_lock: Mutex<()>,
    /// 已成功入队的消息计数；每次递增后的值就是本次应等待的确认编号。
    next_delivery_tag: AtomicU64,
    /// 不可逆的停用标志，避免取消或超时后继续消费可能错配的迟到确认。
    unusable: Arc<AtomicBool>,
    /// 首次服务端关闭帧；即使关闭事件尚未消费，发布前检查也能返回具体原因。
    close_details: Arc<OnceLock<Arc<CloseChannel>>>,
    /// 创建时确定的超时时长与成功判定方式。
    config: RabbitMqPublisherConfig,
}

impl RabbitMqPublisher {
    /// 使用默认配置创建 publisher：收到 ACK 即认为 broker 已接手消息。
    ///
    /// 入队和等待确认共用 3 秒超时；初始化错误与 [`Self::new_with_config`] 相同。
    pub async fn new(connection: &Connection) -> Result<Self, RabbitMqPublishError> {
        Self::new_with_config(connection, RabbitMqPublisherConfig::default()).await
    }

    /// 使用指定配置创建 publisher。
    ///
    /// `connection` 应是已建立的连接。本方法在该连接上新建专用通道、注册通道回调，
    /// 然后等待发布确认模式启用成功，不更换连接原有的回调。
    ///
    /// # 错误
    ///
    /// 超时时长无法表示时返回 [`RabbitMqPublishError::InvalidConfirmTimeout`]；
    /// 通道初始化失败时返回 [`RabbitMqPublishError::Amqp`]，此时尚未发布消息。
    pub async fn new_with_config(
        connection: &Connection,
        config: RabbitMqPublisherConfig,
    ) -> Result<Self, RabbitMqPublishError> {
        // 在创建 channel 前拒绝无法表示的超时，避免无效配置留下额外资源。
        let _ = config.deadline()?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let channel = connection.open_channel(None).await?;
        let unusable = Arc::new(AtomicBool::new(false));
        let close_details = Arc::new(OnceLock::new());

        // 先安装回调，再启用确认模式，保证专用通道的异步通知有对应接收端。
        channel
            .register_callback(PublisherCallback {
                events: events_tx,
                unusable: Arc::clone(&unusable),
                close_details: Arc::clone(&close_details),
            })
            .await?;
        channel
            .confirm_select(ConfirmSelectArguments::default())
            .await?;

        Ok(Self {
            channel,
            connection: connection.clone(),
            events: Mutex::new(events_rx),
            publish_lock: Mutex::new(()),
            next_delivery_tag: AtomicU64::new(0),
            unusable,
            close_details,
            config,
        })
    }

    /// 发布一条消息并等待对应 Publisher Confirm。
    ///
    /// `arguments.mandatory` 由配置的成功模式统一设置：默认 false，路由模式为 true。
    /// 默认模式的 ACK 仅表示 broker 已接手，消息仍可能因无匹配队列而被丢弃。
    /// 路由模式依赖 RabbitMQ 对不可路由消息先发送 RETURN、再发送 confirm 的顺序。
    /// 两种模式均不等待消费者处理，也不会自动声明交换器、队列或绑定。
    /// 超时从取得串行发布锁后开始，包含客户端发送队列入队和等待 confirm。
    ///
    /// # 参数
    ///
    /// - `basic_properties`：消息属性，如内容类型、持久化标记和业务消息编号。
    /// - `content`：调用方已编码的消息正文，本方法不负责序列化。
    /// - `arguments`：交换器、路由键等发布参数，其中 `mandatory` 会被成功模式覆盖。
    ///
    /// # 返回值与错误
    ///
    /// 返回 `Ok(())` 表示消息满足创建时指定的成功条件，不表示消费者已处理消息。
    /// 入队失败、NACK 或路由失败等错误具有确定结论；确认丢失等错误结果未知，
    /// 可用 [`RabbitMqPublishError::is_outcome_unknown`] 判断，再决定如何重试。
    ///
    /// # 取消行为
    ///
    /// 等待串行锁时取消不会开始发布。进入入队或确认阶段后取消会保守停用发布器，
    /// 防止迟到确认与下一条消息错配；已入队的消息仍可能被服务端接收。
    pub async fn publish(
        &self,
        basic_properties: BasicProperties,
        content: Vec<u8>,
        mut arguments: BasicPublishArguments,
    ) -> Result<(), RabbitMqPublishError> {
        let _publish_guard = self.publish_lock.lock().await;
        if !self.is_usable() {
            return Err(unavailable_error(&self.close_details));
        }

        arguments.mandatory = matches!(self.config.success_mode, PublishSuccessMode::RoutedToQueue);
        let deadline = self.config.deadline()?;
        // 覆盖入队和确认阶段；调用方取消 future 时保守停用 publisher，避免迟到事件错配。
        // 内部可确定尚未入队的超时会解除保护，允许继续使用。
        let mut in_flight = InFlightGuard::new(Arc::clone(&self.unusable));
        if let Err(error) = enqueue_publish(
            self.channel
                .basic_publish(basic_properties, content, arguments),
            deadline,
        )
        .await
        {
            if matches!(error, RabbitMqPublishError::PublishTimeout) {
                // amqprs 2.1.5 只 await 一次 mpsc::send；取消等待保证消息尚未入队。
                // 没有遗留 confirm，也没有消耗 delivery tag，publisher 可继续使用。
                in_flight.disarm();
            }
            return Err(error);
        }

        // 仅成功入队后分配确认编号，与专用通道从 1 开始的服务端发布序号保持一致。
        let delivery_tag = self.next_delivery_tag.fetch_add(1, Ordering::Relaxed) + 1;
        let result = self.wait_for_confirm(delivery_tag, deadline).await;
        in_flight.disarm();
        result
    }

    /// 返回 publisher 是否仍可安全复用。
    ///
    /// 同时检查内部停用标志、通道状态和连接状态。此结果仅是当前快照，
    /// 返回 true 后仍可能断线或关闭，调用方仍需处理后续发布错误。
    pub fn is_usable(&self) -> bool {
        !self.unusable.load(Ordering::Acquire)
            && self.channel.is_open()
            && self.channel.is_connection_open()
    }

    /// 返回 publisher 的成功判断模式。
    ///
    /// 该模式在创建时确定，后续每次发布都使用相同的成功条件。
    pub fn success_mode(&self) -> PublishSuccessMode {
        self.config.success_mode
    }

    /// 等待指定编号的确认，同时观察连接结束通知，沿用入队阶段的截止时间。
    async fn wait_for_confirm(
        &self,
        expected_tag: u64,
        deadline: Instant,
    ) -> Result<(), RabbitMqPublishError> {
        let mut events = self.events.lock().await;
        let shutdown = wait_for_shutdown(self.connection.listen_network_io_failure(), || {
            self.connection.is_open()
        });
        wait_for_confirmation(
            &mut events,
            expected_tag,
            &self.config,
            &self.unusable,
            shutdown,
            deadline,
        )
        .await
    }
}

/// 为尚未发送的本次调用构造错误：优先附带首次通道关闭原因，没有原因时返回通用不可用错误。
fn unavailable_error(close_details: &OnceLock<Arc<CloseChannel>>) -> RabbitMqPublishError {
    match close_details.get() {
        Some(close) => RabbitMqPublishError::PublisherUnavailableChannelClosed(Arc::clone(close)),
        None => RabbitMqPublishError::PublisherUnavailable,
    }
}

/// 在绝对截止时间前将消息加入客户端发送队列，区分明确的入队错误和入队超时。
///
/// 此处成功只代表本地入队，服务端是否接收仍由后续发布确认决定。
async fn enqueue_publish(
    publish: impl Future<Output = Result<(), amqprs::error::Error>>,
    deadline: Instant,
) -> Result<(), RabbitMqPublishError> {
    match timeout_at(deadline, publish).await {
        Ok(result) => result.map_err(RabbitMqPublishError::PublishFailed),
        Err(_) => Err(RabbitMqPublishError::PublishTimeout),
    }
}

/// 等待连接结束，并兼容开始等待时连接就已关闭的情况。
///
/// amqprs 的监听 future 首次 poll 时才订阅通知，必须先订阅再检查连接标志，
/// 避免“检查时仍打开、订阅前已关闭”造成通知丢失。首次 poll 已完成时不再重复 poll。
/// 本层将网络故障和其他关闭统一视为连接结束，不向调用方传递底层的 bool。
///
/// amqprs 2.1.5 的关闭通知依赖读取任务退出；若写入任务先退出而读取仍阻塞，
/// 等待确认的调用可能先达到截止时间，并返回确认超时。
async fn wait_for_shutdown(shutdown: impl Future<Output = bool>, is_open: impl FnOnce() -> bool) {
    tokio::pin!(shutdown);
    let notified =
        std::future::poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx).is_ready())).await;
    if !notified && is_open() {
        let _ = shutdown.await;
    }
}

/// 单次发布的确认状态，将按序收到的事件归并为当前成功模式下的最终结果。
struct ConfirmTracker {
    /// 当前消息的确认编号，只处理单条匹配或批量范围覆盖该编号的 ACK/NACK。
    expected_tag: u64,
    /// 决定是否将 RETURN 计入失败的成功判定方式。
    success_mode: PublishSuccessMode,
    /// 路由模式下已收到的退回原因，在最终确认或异常结束时优先返回。
    returned: Option<String>,
}

impl ConfirmTracker {
    /// 处理一个事件；返回 None 表示尚需等待，Some 则表示本次发布可以结束。
    ///
    /// RETURN 在路由模式下先保存原因，仍继续消费对应确认，避免下一次发布错配。
    /// 不覆盖当前编号的 ACK/NACK 只记录日志，不结束等待。
    fn on_event(&mut self, event: PublishEvent) -> Option<Result<(), RabbitMqPublishError>> {
        let expected_tag = self.expected_tag;
        match event {
            PublishEvent::Ack { tag, multiple } if covers(tag, multiple, expected_tag) => {
                Some(match self.returned.take() {
                    Some(detail) => Err(RabbitMqPublishError::Unroutable { detail }),
                    None => Ok(()),
                })
            }
            PublishEvent::Nack { tag, multiple } if covers(tag, multiple, expected_tag) => {
                Some(self.finish(RabbitMqPublishError::Nacked { delivery_tag: tag }))
            }
            PublishEvent::Returned { detail } => {
                if self.success_mode == PublishSuccessMode::RoutedToQueue {
                    self.returned = Some(detail);
                } else {
                    debug!("忽略不影响 broker 接收语义的 RabbitMQ RETURN: {detail}");
                }
                None
            }
            PublishEvent::Closed(close) => {
                Some(self.finish(RabbitMqPublishError::ChannelClosed(close)))
            }
            PublishEvent::Ack { tag, multiple } => {
                debug!(
                    "忽略不匹配的 RabbitMQ publisher ACK: delivery_tag={tag}, multiple={multiple}, expected={expected_tag}"
                );
                None
            }
            PublishEvent::Nack { tag, multiple } => {
                warn!(
                    "忽略不匹配的 RabbitMQ publisher NACK: delivery_tag={tag}, multiple={multiple}, expected={expected_tag}"
                );
                None
            }
        }
    }

    /// 以失败结束等待：优先保留已知路由失败，否则使用传入的确认失败或中断错误。
    fn finish(&mut self, fallback: RabbitMqPublishError) -> Result<(), RabbitMqPublishError> {
        // 已收到 RETURN 就已确定路由失败，不能被随后的断线或超时改成结果未知。
        Err(match self.returned.take() {
            Some(detail) => RabbitMqPublishError::Unroutable { detail },
            None => fallback,
        })
    }
}

/// 同时等待确认事件、连接结束和绝对截止时间，并维护发布器是否可复用。
///
/// 回调事件优先于同时就绪的关闭或超时，以免已排队的 ACK 或 RETURN 被覆盖。
/// 连接关闭、事件通道结束、服务端通道关闭或确认超时都会停用发布器；
/// 正常匹配的 ACK/NACK 则已消费该消息的确认，不因此停用发布器。
async fn wait_for_confirmation(
    events: &mut mpsc::UnboundedReceiver<PublishEvent>,
    expected_tag: u64,
    config: &RabbitMqPublisherConfig,
    unusable: &AtomicBool,
    shutdown: impl Future<Output = ()>,
    deadline: Instant,
) -> Result<(), RabbitMqPublishError> {
    let mut tracker = ConfirmTracker {
        expected_tag,
        success_mode: config.success_mode,
        returned: None,
    };
    tokio::pin!(shutdown);

    loop {
        let result = tokio::select! {
            // 已排队的 ACK/RETURN 优先于随后的断线和超时，保留已知结论。
            biased;
            event = events.recv() => match event {
                Some(event) => {
                    if matches!(&event, PublishEvent::Closed(_)) {
                        unusable.store(true, Ordering::Release);
                    }
                    tracker.on_event(event)
                }
                None => {
                    unusable.store(true, Ordering::Release);
                    Some(tracker.finish(RabbitMqPublishError::ConfirmEventsClosed))
                }
            },
            _ = &mut shutdown => {
                unusable.store(true, Ordering::Release);
                Some(tracker.finish(RabbitMqPublishError::ConnectionClosed))
            }
            _ = sleep_until(deadline) => {
                let result = tracker.finish(RabbitMqPublishError::ConfirmTimeout);
                // 即使 RETURN 已确定路由失败，未消费的 confirm 也会让后续 delivery_tag 关联失去同步。
                unusable.store(true, Ordering::Release);
                Some(result)
            }
        };
        if let Some(result) = result {
            return result;
        }
    }
}

/// 判断单条或批量确认是否覆盖当前消息编号。
///
/// 单条确认必须编号相等；批量确认覆盖不大于 tag 的编号，tag 为 0 时覆盖所有待确认消息。
fn covers(tag: u64, multiple: bool, expected: u64) -> bool {
    if multiple {
        tag == 0 || expected <= tag
    } else {
        tag == expected
    }
}

/// 不依赖实际 RabbitMQ 服务的单元测试，覆盖成功模式、超时、关闭和取消行为。
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// 默认配置只要求服务端 ACK，不要求消息路由到队列。
    #[test]
    fn default_success_mode_only_requires_broker_ack() {
        assert_eq!(
            RabbitMqPublisherConfig::default().success_mode,
            PublishSuccessMode::BrokerAccepted
        );
        assert_eq!(
            RabbitMqPublisherConfig::new().success_mode,
            PublishSuccessMode::default()
        );
    }

    /// 调用方可以通过构建方法设置超时和成功模式。
    #[test]
    fn config_builders_allow_external_configuration() {
        let config = RabbitMqPublisherConfig::new()
            .with_confirm_timeout(Duration::from_secs(10))
            .with_success_mode(PublishSuccessMode::RoutedToQueue);

        assert_eq!(config.confirm_timeout, Duration::from_secs(10));
        assert_eq!(config.success_mode, PublishSuccessMode::RoutedToQueue);
    }

    /// 极大超时时长被明确拒绝，不因计算截止时间溢出而崩溃。
    #[test]
    fn oversized_timeout_is_rejected_without_panicking() {
        let config = RabbitMqPublisherConfig::new().with_confirm_timeout(Duration::MAX);
        let error = config.deadline().unwrap_err();

        assert!(matches!(error, RabbitMqPublishError::InvalidConfirmTimeout));
        assert!(!error.is_outcome_unknown());
        assert!(RabbitMqPublisherConfig::new().deadline().is_ok());
    }

    /// 关闭事件消费前即可读到完整原因，且缓存、事件及两个错误共享同一份关闭帧。
    /// 同时验证发布前关闭为确定未发布，确认期间关闭为结果未知。
    #[test]
    fn idle_close_preserves_broker_reason_before_consuming_event() {
        for reply_code in [403, 404, 406] {
            let (events_tx, mut events_rx) = mpsc::unbounded_channel();
            let unusable = Arc::new(AtomicBool::new(false));
            let close_details = Arc::new(OnceLock::new());
            let callback = PublisherCallback {
                events: events_tx,
                unusable: Arc::clone(&unusable),
                close_details: Arc::clone(&close_details),
            };
            // CloseChannel 没有公开构造器，用其 Deserialize 实现构造 broker 关闭帧。
            let close = serde_json::from_value(serde_json::json!({
                "reply_code": reply_code,
                "reply_text": [13, "broker reason"],
                "class_id": 60,
                "method_id": 40,
            }))
            .unwrap();

            callback.on_close(close);

            assert!(unusable.load(Ordering::Acquire));
            let error = unavailable_error(&close_details);
            assert!(!error.is_outcome_unknown());
            let RabbitMqPublishError::PublisherUnavailableChannelClosed(close) = error else {
                panic!("expected channel close details before publishing");
            };
            assert_eq!(close.reply_code(), reply_code);
            assert_eq!(close.reply_text(), "broker reason");
            assert_eq!(close.class_id(), 60);
            assert_eq!(close.method_id(), 40);
            assert!(Arc::ptr_eq(&close, close_details.get().unwrap()));

            let event = events_rx.try_recv().unwrap();
            assert!(
                matches!(&event, PublishEvent::Closed(event_close) if Arc::ptr_eq(event_close, &close))
            );
            let mut tracker = ConfirmTracker {
                expected_tag: 1,
                success_mode: PublishSuccessMode::BrokerAccepted,
                returned: None,
            };
            let error = tracker.on_event(event).unwrap().unwrap_err();
            assert!(error.is_outcome_unknown());
            assert!(matches!(
                error,
                RabbitMqPublishError::ChannelClosed(event_close) if Arc::ptr_eq(&event_close, &close)
            ));
        }
        assert!(matches!(
            unavailable_error(&OnceLock::new()),
            RabbitMqPublishError::PublisherUnavailable
        ));
    }

    /// 底层通知首次轮询即完成时直接结束，不重复轮询已经完成的异步任务。
    #[tokio::test]
    async fn shutdown_notifications_complete_without_repolling() {
        for network_failure in [true, false] {
            // async 块完成后再次 poll 会 panic，确保已完成通知不会被重复等待。
            wait_for_shutdown(async { network_failure }, || true).await;
        }
    }

    /// 先建立关闭通知订阅，再检查连接状态，覆盖订阅时序上的竞争情况。
    #[tokio::test]
    async fn shutdown_subscribes_before_checking_connection() {
        let subscribed = Cell::new(false);
        let shutdown = std::future::poll_fn(|_| {
            subscribed.set(true);
            Poll::<bool>::Pending
        });

        wait_for_shutdown(shutdown, || {
            assert!(subscribed.get());
            false
        })
        .await;
    }

    /// 连接仍打开时继续等待通知，通知到达后能够结束等待。
    #[tokio::test]
    async fn shutdown_waits_for_notification_on_open_connection() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let shutdown = wait_for_shutdown(async { shutdown_rx.await.unwrap() }, || true);
        tokio::pin!(shutdown);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx).is_pending())).await
        );

        shutdown_tx.send(true).unwrap();
        shutdown.await;
    }

    /// 队列满时入队超时不会发出消息，原发送队列仍可继续使用。
    #[tokio::test]
    async fn enqueue_timeout_does_not_send_message() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(1);
        outgoing_tx.send(1).await.unwrap();
        let result = enqueue_publish(
            async {
                outgoing_tx
                    .send(2)
                    .await
                    .map_err(|error| amqprs::error::Error::InternalChannelError(error.to_string()))
            },
            Instant::now(),
        )
        .await;

        let error = result.unwrap_err();
        assert!(matches!(error, RabbitMqPublishError::PublishTimeout));
        assert!(!error.is_outcome_unknown());
        assert_eq!(outgoing_rx.recv().await, Some(1));
        assert!(matches!(
            outgoing_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        outgoing_tx.send(3).await.unwrap();
        assert_eq!(outgoing_rx.recv().await, Some(3));
    }

    /// 入队操作明确返回错误时，能够确定本次消息尚未发布。
    #[tokio::test]
    async fn enqueue_failure_is_definitely_not_published() {
        let result = enqueue_publish(
            async {
                Err(amqprs::error::Error::InternalChannelError(
                    "outgoing queue closed".to_owned(),
                ))
            },
            Instant::now(),
        )
        .await;

        let error = result.unwrap_err();
        assert!(matches!(error, RabbitMqPublishError::PublishFailed(_)));
        assert!(!error.is_outcome_unknown());
    }

    /// 等待确认沿用入队阶段的截止时间，确认超时后停用发布器并标记结果未知。
    #[tokio::test]
    async fn confirmation_reuses_deadline_and_disables_timed_out_publisher() {
        let (_events_tx, mut events_rx) = mpsc::unbounded_channel();
        let unusable = AtomicBool::new(false);
        let deadline = Instant::now();
        enqueue_publish(async { Ok(()) }, deadline).await.unwrap();

        let result = wait_for_confirmation(
            &mut events_rx,
            1,
            &RabbitMqPublisherConfig::default(),
            &unusable,
            std::future::pending(),
            deadline,
        )
        .await;

        let error = result.unwrap_err();
        assert!(matches!(error, RabbitMqPublishError::ConfirmTimeout));
        assert!(error.is_outcome_unknown());
        assert!(unusable.load(Ordering::Acquire));
    }

    /// 已排队的匹配 ACK 优先于同时发生的断线或超时，保留成功结论。
    #[tokio::test]
    async fn queued_ack_takes_priority_over_shutdown_and_timeout() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let unusable = AtomicBool::new(false);
        events_tx
            .send(PublishEvent::Ack {
                tag: 1,
                multiple: false,
            })
            .unwrap();

        let result = wait_for_confirmation(
            &mut events_rx,
            1,
            &RabbitMqPublisherConfig::default(),
            &unusable,
            async {},
            Instant::now(),
        )
        .await;

        assert!(result.is_ok());
        assert!(!unusable.load(Ordering::Acquire));
    }

    /// 已收到 RETURN 时保留确定的路由失败；缺少最终确认仍会停用发布器。
    #[tokio::test]
    async fn returned_then_timeout_keeps_failure_known_and_disables_publisher() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let unusable = AtomicBool::new(false);
        events_tx
            .send(PublishEvent::Returned {
                detail: "no route".to_owned(),
            })
            .unwrap();
        let config =
            RabbitMqPublisherConfig::new().with_success_mode(PublishSuccessMode::RoutedToQueue);

        let result = wait_for_confirmation(
            &mut events_rx,
            1,
            &config,
            &unusable,
            std::future::pending(),
            Instant::now(),
        )
        .await;

        let error = result.unwrap_err();
        assert!(matches!(error, RabbitMqPublishError::Unroutable { .. }));
        assert!(!error.is_outcome_unknown());
        assert!(unusable.load(Ordering::Acquire));
    }

    /// 批量确认包含其上界，编号 0 覆盖全部，单条确认只匹配相同编号。
    #[test]
    fn multiple_ack_covers_expected_delivery_tag() {
        assert!(covers(0, true, 1));
        assert!(covers(3, true, 2));
        assert!(!covers(1, true, 2));
        assert!(covers(2, false, 2));
        assert!(!covers(3, false, 2));
    }

    /// 超时和断线属于结果未知，明确的入队失败与 NACK 属于结果已确定。
    #[test]
    fn only_missing_confirm_is_unknown() {
        assert!(RabbitMqPublishError::ConfirmTimeout.is_outcome_unknown());
        assert!(RabbitMqPublishError::ConnectionClosed.is_outcome_unknown());
        assert!(
            !RabbitMqPublishError::PublishFailed(amqprs::error::Error::InternalChannelError(
                "not queued".to_owned()
            ))
            .is_outcome_unknown()
        );
        assert!(!RabbitMqPublishError::Nacked { delivery_tag: 1 }.is_outcome_unknown());
    }

    /// 相同的 RETURN 加 ACK 序列，在接收模式下成功，在路由模式下返回失败。
    #[test]
    fn confirm_tracker_applies_the_selected_success_mode() {
        let mut broker_accepted = ConfirmTracker {
            expected_tag: 1,
            success_mode: PublishSuccessMode::BrokerAccepted,
            returned: None,
        };
        assert!(
            broker_accepted
                .on_event(PublishEvent::Returned {
                    detail: "no route".to_owned(),
                })
                .is_none()
        );
        assert!(matches!(
            broker_accepted.on_event(PublishEvent::Ack {
                tag: 1,
                multiple: false,
            }),
            Some(Ok(()))
        ));

        let mut routed = ConfirmTracker {
            expected_tag: 2,
            success_mode: PublishSuccessMode::RoutedToQueue,
            returned: None,
        };
        assert!(
            routed
                .on_event(PublishEvent::Returned {
                    detail: "no route".to_owned(),
                })
                .is_none()
        );
        assert!(matches!(
            routed.on_event(PublishEvent::Ack {
                tag: 2,
                multiple: false,
            }),
            Some(Err(RabbitMqPublishError::Unroutable { detail })) if detail == "no route"
        ));
    }

    /// 路由失败已确定后，不被后续确认超时替换成结果未知。
    #[test]
    fn known_return_survives_missing_confirm() {
        let mut tracker = ConfirmTracker {
            expected_tag: 1,
            success_mode: PublishSuccessMode::RoutedToQueue,
            returned: None,
        };
        tracker.on_event(PublishEvent::Returned {
            detail: "no route".to_owned(),
        });

        assert!(matches!(
            tracker.finish(RabbitMqPublishError::ConfirmTimeout),
            Err(RabbitMqPublishError::Unroutable { detail }) if detail == "no route"
        ));
    }

    /// 忽略其他编号的 NACK，匹配当前消息后仍保留已确定的路由失败。
    #[test]
    fn a_known_nack_is_not_replaced_by_an_unrelated_return() {
        let mut tracker = ConfirmTracker {
            expected_tag: 2,
            success_mode: PublishSuccessMode::RoutedToQueue,
            returned: None,
        };
        tracker.on_event(PublishEvent::Returned {
            detail: "no route".to_owned(),
        });

        assert!(
            tracker
                .on_event(PublishEvent::Nack {
                    tag: 1,
                    multiple: false,
                })
                .is_none()
        );
        assert!(matches!(
            tracker.on_event(PublishEvent::Nack {
                tag: 2,
                multiple: false,
            }),
            Some(Err(RabbitMqPublishError::Unroutable { .. }))
        ));
    }

    /// 发布保护器未解除就被销毁时停用发布器，防止取消后的迟到确认错配。
    #[test]
    fn cancelling_in_flight_publish_disables_publisher() {
        let unusable = Arc::new(AtomicBool::new(false));
        let guard = InFlightGuard::new(Arc::clone(&unusable));

        drop(guard);

        assert!(unusable.load(Ordering::Acquire));
    }

    /// 正常解除保护后销毁保护器，不会额外停用发布器。
    #[test]
    fn completed_publish_keeps_publisher_usable() {
        let unusable = Arc::new(AtomicBool::new(false));
        let mut guard = InFlightGuard::new(Arc::clone(&unusable));
        guard.disarm();

        drop(guard);

        assert!(!unusable.load(Ordering::Acquire));
    }
}
