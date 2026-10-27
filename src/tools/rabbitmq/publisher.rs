//! 基于 lapin 发布确认机制的并发消息发布器。
//!
//! 一个 [`Publisher`] 独占一个启用发布确认的 channel，允许同时有多个发布调用
//! 处于「已发送、未确认」状态，并发调用上限由 [`PublisherConfig::max_inflight`]
//! 控制。每条消息的结果由 lapin 的 `PublisherConfirm` 单独兑现，因此调用方可以
//! 并发调用 [`Publisher::publish`]，按各自结果处理，不需要通道池。
//!
//! `max_inflight` 是**调用级**闸门，不是 broker 侧未确认窗口的上限：超时或取消会
//! 立即归还配额，而 lapin 仍保留那条待确认消息（见 [`Publisher::wait_for_confirms`]）。
//!
//! 发布器不自动重连、不自动重发。调用方可通过 [`PublishError::is_outcome_unknown`]
//! 判断失败时消息结果是否已确定，再结合业务幂等决定是否重试。

use lapin::message::BasicReturnMessage;
use lapin::options::{BasicPublishOptions, ConfirmSelectOptions};
use lapin::{
    BasicProperties, Channel, ChannelState, Confirmation, Connection, ConnectionState,
    ConnectionStatus, ErrorKind,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, timeout, timeout_at};

/// 默认的并发在途上限。
pub const DEFAULT_MAX_INFLIGHT: usize = 16;

/// 默认的「发布 + 等待确认」超时时间。
pub const DEFAULT_PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);

/// 发布成功的判断方式。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PublishSuccessMode {
    /// 服务端确认收到消息即成功，不要求消息路由到队列。
    ///
    /// 这是默认模式；没有匹配队列的消息可能被服务端丢弃，成功不代表消费者已处理。
    #[default]
    BrokerAccepted,
    /// 服务端确认收到，且消息没有被退回，才算至少路由到一个队列。
    ///
    /// 该模式会启用 `mandatory`，由 lapin 把退回信息并入确认结果。
    /// 路由成功仍不代表消费者已处理消息。
    RoutedToQueue,
}

/// 发布器配置。
///
/// 使用 [`PublisherConfig::default`] 或构建方法创建，以便后续版本兼容地增加配置项。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PublisherConfig {
    /// 同时持有发布配额的调用上限，必须大于 0。
    ///
    /// 该值决定并发度：上限为 1 时行为等价于串行发布。它只限制并发调用数，
    /// 不限制 broker 侧未确认消息数：发布超时或取消后配额立即归还，但 lapin
    /// 保留的待确认条目仍会在 broker 侧占用未确认窗口。
    pub max_inflight: usize,
    /// 单条消息从发起发布到拿到确认的最长时间，不包含等待并发配额的时间。
    pub publish_timeout: Duration,
    /// 等待并发配额的最长时间，`None` 表示一直等待。
    ///
    /// 该时间与 [`Self::publish_timeout`] 分开计算，超时说明消息尚未发出。
    pub acquire_timeout: Option<Duration>,
    /// 发布成功的判断方式。
    ///
    /// [`PublishSuccessMode::RoutedToQueue`] 要求 `max_inflight` 为 1，否则配置校验失败。
    pub success_mode: PublishSuccessMode,
}

impl Default for PublisherConfig {
    /// 在途上限 16 条，发布超时 5 秒，无限等待配额，仅要求服务端确认。
    fn default() -> Self {
        Self {
            max_inflight: DEFAULT_MAX_INFLIGHT,
            publish_timeout: DEFAULT_PUBLISH_TIMEOUT,
            acquire_timeout: None,
            success_mode: PublishSuccessMode::default(),
        }
    }
}

impl PublisherConfig {
    /// 创建默认配置。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置并发在途上限。
    pub fn with_max_inflight(mut self, max_inflight: usize) -> Self {
        self.max_inflight = max_inflight;
        self
    }

    /// 设置单条消息的发布与确认总超时。
    pub fn with_publish_timeout(mut self, publish_timeout: Duration) -> Self {
        self.publish_timeout = publish_timeout;
        self
    }

    /// 设置等待并发配额的超时，`None` 表示一直等待。
    pub fn with_acquire_timeout(mut self, acquire_timeout: Option<Duration>) -> Self {
        self.acquire_timeout = acquire_timeout;
        self
    }

    /// 设置发布成功的判断方式。
    pub fn with_success_mode(mut self, success_mode: PublishSuccessMode) -> Self {
        self.success_mode = success_mode;
        self
    }

    /// 校验配置是否可用，拒绝零并发、越界并发、非法组合与无法表示的时长。
    fn validate(&self) -> Result<(), PublishError> {
        if self.max_inflight == 0 {
            return Err(PublishError::ConfigInvalid("max_inflight 必须大于 0"));
        }
        if self.max_inflight > Semaphore::MAX_PERMITS {
            return Err(PublishError::ConfigInvalid(
                "max_inflight 超出 Semaphore 允许的上限",
            ));
        }
        // lapin 把 RETURN 按 FIFO 配给完成的确认，多 in-flight 时无法把
        // 退回消息精确对应到具体某条消息，因此路由模式只允许单在途。
        if matches!(self.success_mode, PublishSuccessMode::RoutedToQueue) && self.max_inflight != 1
        {
            return Err(PublishError::ConfigInvalid(
                "RoutedToQueue 要求 max_inflight 为 1",
            ));
        }
        if Instant::now().checked_add(self.publish_timeout).is_none() {
            return Err(PublishError::ConfigInvalid(
                "publish_timeout 超出计时器可表示范围",
            ));
        }
        if let Some(acquire_timeout) = self.acquire_timeout
            && Instant::now().checked_add(acquire_timeout).is_none()
        {
            return Err(PublishError::ConfigInvalid(
                "acquire_timeout 超出计时器可表示范围",
            ));
        }
        Ok(())
    }
}

/// 发布失败原因；可通过 [`Self::is_outcome_unknown`] 区分结果是否已确定。
///
/// 「结果未知」表示消息可能已经满足配置的成功条件，也可能没有，直接重试可能重复。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PublishError {
    /// 消息体序列化失败，本次消息尚未提交发布。
    #[error("序列化消息失败: {0}")]
    Serialization(#[from] serde_json::Error),
    /// 配置不可用，尚未建立通道或发布消息。
    #[error("publisher 配置无效: {0}")]
    ConfigInvalid(&'static str),
    /// 创建通道或启用发布确认失败，本次发布器没有发出任何消息。
    #[error("publisher 初始化失败: {0}")]
    Setup(lapin::Error),
    /// 发布前发现连接不可用，本次消息确定未发布。
    #[error("连接当前状态为 {0:?}，本次消息尚未发布")]
    NotConnected(ConnectionState),
    /// 连接处于服务端资源告警引起的阻塞状态，本次消息确定未发布。
    ///
    /// 阻塞期间 broker 不再读取新消息，发布只能超时；直接失败可以让调用方尽快退让。
    #[error("连接被服务端阻塞（资源告警），本次消息尚未发布")]
    ConnectionBlocked,
    /// 发布前发现通道不可用，本次消息确定未发布。
    #[error("通道当前状态为 {0:?}，本次消息尚未发布")]
    ChannelUnavailable(ChannelState),
    /// 路由模式下消息被服务端退回，已确定没有路由到任何队列。
    #[error("消息未路由到任何队列: {reason}")]
    Unroutable {
        /// 服务端 RETURN 帧的原因、交换机与路由键。
        reason: String,
    },
    /// 服务端明确拒绝该消息。
    #[error("RabbitMQ 拒绝了消息: {reason}")]
    Nacked {
        /// 服务端 NACK 携带的退回收到的信息，通常为空。
        reason: String,
    },
    /// 在 `acquire_timeout` 内没有拿到并发配额，本次消息尚未发布。
    #[error("等待发布配额超时，本次消息尚未发布")]
    AcquireTimeout,
    /// 在 `publish_timeout` 内没有拿到入队许可，本次消息尚未提交。
    #[error("等待入队许可超时，本次消息尚未发布")]
    EnqueueTimeout,
    /// 在 `publish_timeout` 内未完成入队或未拿到确认，结果未知，重试可能导致重复。
    #[error("发布或等待确认超时，结果未知，重试可能导致重复")]
    PublishTimeout,
    /// 此前的发布在未取得确认的情况下结束，退回消息与确认的对应关系已不可信。
    ///
    /// 仅路由模式会出现。本次消息确定未发布；需要继续确认路由结果时，请重建发布器。
    #[error("此前有消息未取得确认，路由结果已不可信；本次消息尚未发布，请重建 publisher")]
    AttributionLost,
    /// 通道未处于发布确认模式，无法判断发布结果，重试可能导致重复。
    #[error("通道未启用发布确认，无法判断发布结果")]
    ConfirmNotEnabled,
    /// 连接在确认前断开，结果未知，重试可能导致重复。
    #[error("连接已断开，结果未知，重试可能导致重复: {0}")]
    ConnectionLost(lapin::Error),
    /// 通道级错误，结果未知，重试可能导致重复。
    #[error("通道级错误，结果未知，重试可能导致重复: {0}")]
    ChannelError(lapin::Error),
}

impl PublishError {
    /// 返回 true 表示无法确定消息是否满足所选的成功条件，重试可能重复。
    ///
    /// 返回 false 仅表示本次失败的结论已确定，不代表所有错误都可直接重试。
    /// 仅显式列出已确定的失败，以便将来新增错误默认按结果未知处理。
    pub fn is_outcome_unknown(&self) -> bool {
        !matches!(
            self,
            Self::Serialization(_)
                | Self::ConfigInvalid(_)
                | Self::Setup(_)
                | Self::NotConnected(_)
                | Self::ConnectionBlocked
                | Self::ChannelUnavailable(_)
                | Self::Unroutable { .. }
                | Self::Nacked { .. }
                | Self::AcquireTimeout
                | Self::EnqueueTimeout
                | Self::AttributionLost
        )
    }
}

/// 并发 RabbitMQ 发布器。
///
/// 同时发送的消息上限受 [`PublisherConfig::max_inflight`] 限制，超出时调用方等待配额。
/// 所有消息共用同一条连接，因此连接断开会影响该发布器的全部在途消息。
///
/// # 并发与归因
///
/// 帧入队阶段由内部互斥锁串行化：lapin 先分配客户端确认编号、再把帧写入发送队列，
/// 这两步之间没有原子保证，并发调用时编号顺序可能与线上顺序不一致。锁只覆盖入队，
/// 不覆盖等待确认，因此并发度不受影响。
///
/// `NACK` 与 `RETURN` 的精确归因仍只在 `max_inflight == 1` 时成立：lapin 把退回
/// 消息按先进先出配给确认。路由模式因此强制要求单在途。
///
/// 即使满足单在途，一旦某次发布在未取得确认的情况下结束（超时或取消），lapin 仍会
/// 保留那条待确认消息，之后收到的 `RETURN` 会被它先领走，导致路由结果错位。因此
/// 路由模式在发生这种情况后会把发布器标记为不可用，后续发布会直接返回
/// [`PublishError::AttributionLost`]，需要重建发布器。
///
/// # 取消行为
///
/// 在等待并发配额或入队许可阶段取消不会发出消息；在等待确认阶段取消时，lapin
/// 会把该消息登记到 `wait_for_confirms`，通道可继续用于普通发布，但路由模式会因
/// 归因不再可靠而停用；在 `basic_publish` 写帧过程中取消或超时，消息是否已写出
/// 无法确定，应按结果未知处理。
pub struct Publisher {
    /// 已启用发布确认的通道，所有消息共用。
    channel: Channel,
    /// 连接状态快照句柄，用于在发布前判断连接是否仍可用。
    connection_status: ConnectionStatus,
    /// 串行化帧入队，保证确认编号与线上顺序一致。
    publish_lock: Mutex<()>,
    /// 并发配额，数量等于 `max_inflight`。
    permits: Arc<Semaphore>,
    /// 路由模式下是否已经失去退回消息的归因能力。
    attribution_lost: AtomicBool,
    /// 创建时确定的配置。
    config: PublisherConfig,
}

impl Publisher {
    /// 使用指定配置在连接上创建一个发布器。
    ///
    /// 该函数会新建一个通道并启用发布确认，不会替换连接上已有的通道。
    ///
    /// # 错误
    ///
    /// 配置非法时返回 [`PublishError::ConfigInvalid`]；通道创建或确认模式启用失败时
    /// 返回 [`PublishError::Setup`]，此时没有任何消息被发出。
    pub async fn new(
        connection: &Connection,
        config: PublisherConfig,
    ) -> Result<Self, PublishError> {
        config.validate()?;
        let channel = connection
            .create_channel()
            .await
            .map_err(PublishError::Setup)?;
        channel
            .confirm_select(ConfirmSelectOptions::default())
            .await
            .map_err(PublishError::Setup)?;
        Ok(Self {
            channel,
            connection_status: connection.status().clone(),
            publish_lock: Mutex::new(()),
            permits: Arc::new(Semaphore::new(config.max_inflight)),
            attribution_lost: AtomicBool::new(false),
            config,
        })
    }

    /// 发布一条消息并等待其确认结果。
    ///
    /// `exchange` 与 `routing_key` 由调用方给出；`mandatory` 由成功模式统一设置。
    /// 本方法不声明交换机、队列或绑定，也不做重试。
    ///
    /// # 错误
    ///
    /// 等待配额、连接阻塞、消息未入队、消息被拒绝或未路由时返回结果已确定的错误；
    /// 发布或确认超时、连接断开等返回结果未知的错误，可用
    /// [`PublishError::is_outcome_unknown`] 判断后再决定是否重试。
    ///
    /// `publish_timeout` 覆盖「等待入队许可 + 入队 + 等待确认」，不包含等待并发配额。
    /// 路由模式下，若此前的发布未取得确认（超时或取消），后续发布直接返回
    /// [`PublishError::AttributionLost`]；此时应重建发布器。
    pub async fn publish(
        &self,
        exchange: &str,
        routing_key: &str,
        payload: &[u8],
        properties: BasicProperties,
    ) -> Result<(), PublishError> {
        // 路由模式一旦失去归因能力，继续发布只会得到不可信的路由结论。
        if matches!(self.config.success_mode, PublishSuccessMode::RoutedToQueue)
            && self.attribution_lost()
        {
            return Err(PublishError::AttributionLost);
        }
        let permit = self.acquire_permit().await?;
        // 资源告警期间 broker 不再读取新消息，提前失败可以给出“确定未发布”的结论。
        if self.connection_status.blocked() {
            return Err(PublishError::ConnectionBlocked);
        }
        // 截止时间覆盖入队与等确认两个阶段，与等待配额的时间分开计算。
        let deadline = Instant::now()
            .checked_add(self.config.publish_timeout)
            .ok_or(PublishError::ConfigInvalid(
                "publish_timeout 超出计时器可表示范围",
            ))?;
        let options = BasicPublishOptions {
            mandatory: matches!(self.config.success_mode, PublishSuccessMode::RoutedToQueue),
            immediate: false,
        };
        // 配额与归因标志交给同一个守卫：析构时先置位、后归还配额（语言保证
        // `Drop::drop` 早于字段析构），因此配额释放后新的持有者必然能看到失效标志。
        let mut guard = InflightGuard::new(
            permit,
            &self.attribution_lost,
            matches!(self.config.success_mode, PublishSuccessMode::RoutedToQueue),
        );
        // 等待配额期间前一次发布可能已超时并置位，必须在取得配额后复核一次。
        // 路由模式强制 `max_inflight == 1`，配额即串行化点，复核后不会再有并发
        // 发布抢先写入帧。
        if matches!(self.config.success_mode, PublishSuccessMode::RoutedToQueue)
            && self.attribution_lost()
        {
            return Err(PublishError::AttributionLost);
        }
        // lapin 在 basic_publish 内部先分配确认编号再入队，两步之间没有原子保证；
        // 这里只锁住入队阶段，等确认仍然并发。
        let confirm = {
            let _enqueue_guard = match timeout_at(deadline, self.publish_lock.lock()).await {
                Ok(guard) => guard,
                Err(_) => {
                    // 尚未提交任何东西，不算丢失归因。
                    guard.disarm();
                    return Err(PublishError::EnqueueTimeout);
                }
            };
            match timeout_at(
                deadline,
                self.channel.basic_publish(
                    exchange.into(),
                    routing_key.into(),
                    options,
                    payload,
                    properties,
                ),
            )
            .await
            {
                Ok(Ok(confirm)) => confirm,
                Ok(Err(error)) => {
                    // 只有发送前就发现连接不可用才能确定没有消耗确认编号。
                    if matches!(error.kind(), ErrorKind::InvalidConnectionState(_)) {
                        guard.disarm();
                    }
                    return Err(classify_publish_error(error));
                }
                // 帧可能已入队、也可能未入队，无法确定是否送达。
                Err(_) => return Err(PublishError::PublishTimeout),
            }
        };
        match timeout_at(deadline, confirm).await {
            // 超时后 confirm 会被丢弃，lapin 会把它登记进 wait_for_confirms，通道不被污染。
            Ok(Ok(confirmation)) => {
                // 已拿到确定结果，不存在遗留的待确认条目。
                guard.disarm();
                normalize_confirmation(confirmation, self.config.success_mode)
            }
            Ok(Err(error)) => {
                // lapin 在连接/通道错误时会清空所有待确认条目，归因仍然完整。
                guard.disarm();
                Err(classify_link_error(error))
            }
            // 确认未兑现：这条待确认条目仍留在 lapin 侧，之后的 RETURN 会被它领走。
            Err(_) => Err(PublishError::PublishTimeout),
        }
    }

    /// 返回发布器当前是否仍可安全复用。
    ///
    /// 同时检查连接状态、通道状态与路由归因能力，不包含服务端资源告警引起的临时
    /// 阻塞；阻塞状态请用 [`Self::is_blocked`] 单独判断。该结果是快照，返回 true
    /// 后仍可能断开，调用方仍需处理后续发布错误。
    pub fn is_usable(&self) -> bool {
        self.connection_status.connected()
            && self.channel.status().connected()
            && !self.attribution_lost()
    }

    /// 返回路由模式下的退回归因是否已经失效。
    ///
    /// 失效后仍可用该发布器发送普通消息，但不会再把 `RETURN` 当作路由结论；
    /// 需要精确路由判定时请重建发布器。
    pub fn attribution_lost(&self) -> bool {
        self.attribution_lost.load(Ordering::Acquire)
    }

    /// 返回连接是否被服务端标记为阻塞（资源告警）。
    ///
    /// 阻塞期间 broker 不再读取新消息，[`Self::publish`] 会直接返回
    /// [`PublishError::ConnectionBlocked`]。该状态是临时的，解除后无需重建发布器。
    pub fn is_blocked(&self) -> bool {
        self.connection_status.blocked()
    }

    /// 返回当前持有并发配额的发布调用数量。
    ///
    /// 只反映 [`PublisherConfig::max_inflight`] 这个调用级闸门的占用情况：等待配额
    /// 或被阻塞的调用不计入。发布超时或取消后配额会被归还，而 lapin 仍保留那条
    /// 待确认消息，因此 broker 侧未确认消息可能多于此值；需要等待这些遗留确认
    /// 落定时请用 [`Self::wait_for_confirms`]。
    pub fn active_publishes(&self) -> usize {
        // 已借出的配额即代表正在执行的发布调用，发布器创建后不会再扩容。
        self.config.max_inflight - self.permits.available_permits()
    }

    /// 返回配置中声明的并发在途上限。
    pub fn max_inflight(&self) -> usize {
        self.config.max_inflight
    }

    /// 返回底层通道，供调用方声明交换机、队列或开启消费者。
    ///
    /// 在该通道上直接发布会绕过并发配额与结果归一化，请仅用于其他管理操作。
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// 等待 lapin 记录的最后一个待确认消息完成，并返回期间被退回的消息。
    ///
    /// 语义由 lapin 决定：它等待「最后注册的确认编号」被服务端确认。当 broker
    /// 乱序确认（先确认后面的编号）时可能提前返回，此时 [`Self::active_publishes`]
    /// 仍大于 0。需要严格屏障时请使用 `max_inflight == 1`。
    ///
    /// 发布超时或取消后 lapin 仍保留那条待确认消息，它不占用并发配额，也不会
    /// 体现在 [`Self::active_publishes`] 中；需要对账时用它排空这些遗留确认。
    pub async fn wait_for_confirms(&self) -> lapin::Result<Vec<BasicReturnMessage>> {
        self.channel.wait_for_confirms().await
    }

    /// 取得一个并发配额；配置了 `acquire_timeout` 时按时返回错误。
    ///
    /// 返回 owned 配额，以便与归因标志打包进同一个析构守卫。
    async fn acquire_permit(&self) -> Result<OwnedSemaphorePermit, PublishError> {
        let acquire = Arc::clone(&self.permits).acquire_owned();
        match self.config.acquire_timeout {
            None => acquire.await.map_err(|_| PublishError::AcquireTimeout),
            Some(acquire_timeout) => timeout(acquire_timeout, acquire)
                .await
                .map_err(|_| PublishError::AcquireTimeout)?
                .map_err(|_| PublishError::AcquireTimeout),
        }
    }
}

/// 发布过程的析构保护：持有并发配额，未取得确认就被丢弃时标记路由归因已失效。
///
/// lapin 会把被丢弃的 `PublisherConfirm` 继续保留在待确认集合中，之后到达的
/// `RETURN` 会被这条遗留消息先领走，因此不能再把后续退回结果当作自己的路由结论。
///
/// 置位写在 [`Drop::drop`] 里，配额作为字段在 `drop` 之后才析构，这一顺序由语言
/// 保证，因此「先置位、后归还配额」不依赖局部变量声明顺序；下一个拿到同一配额的
/// 调用只要在取得配额后复核标志，就不会漏看这次失效。
struct InflightGuard<'a> {
    /// 与发布器共享的归因失效标志。
    lost: &'a AtomicBool,
    /// 仅路由模式需要记录；其他模式下不会写入标志。
    armed: bool,
    /// 并发配额；析构顺序保证它晚于标志置位被归还，只持有不读取。
    _permit: OwnedSemaphorePermit,
}

impl<'a> InflightGuard<'a> {
    /// 创建保护器；`enabled` 为 false 时析构不写入标志，但仍会归还配额。
    fn new(permit: OwnedSemaphorePermit, lost: &'a AtomicBool, enabled: bool) -> Self {
        Self {
            lost,
            armed: enabled,
            _permit: permit,
        }
    }

    /// 发布已得到确定结论，或根本没有提交，解除保护。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InflightGuard<'_> {
    /// 未解除保护说明确认没有兑现，归因不可再信；配额在字段析构时归还。
    fn drop(&mut self) {
        if self.armed {
            self.lost.store(true, Ordering::Release);
        }
    }
}

/// 把 lapin 的确认结果归一化为发布结果。
fn normalize_confirmation(
    confirmation: Confirmation,
    success_mode: PublishSuccessMode,
) -> Result<(), PublishError> {
    match confirmation {
        Confirmation::Ack(returned) => match (success_mode, returned) {
            // 路由模式下收到退回消息，说明没有任何队列接收该消息。
            (PublishSuccessMode::RoutedToQueue, Some(returned)) => Err(PublishError::Unroutable {
                reason: describe_returned(&returned),
            }),
            _ => Ok(()),
        },
        Confirmation::Nack(returned) => Err(PublishError::Nacked {
            reason: returned
                .as_ref()
                .map(describe_returned)
                .unwrap_or_else(|| "服务端未提供退回信息".to_owned()),
        }),
        Confirmation::NotRequested => Err(PublishError::ConfirmNotEnabled),
    }
}

/// 描述被退回的消息，包含服务端响应码、原因、交换机与路由键。
fn describe_returned(returned: &BasicReturnMessage) -> String {
    format!(
        "reply_code={}, reply_text={}, exchange={}, routing_key={}",
        returned.reply_code,
        returned.reply_text,
        returned.delivery.exchange,
        returned.delivery.routing_key
    )
}

/// 分类 `basic_publish` 返回的错误。
///
/// lapin 会在写帧前检查连接与通道状态，因此这两类错误可以确定消息未发布。
fn classify_publish_error(error: lapin::Error) -> PublishError {
    match error.kind() {
        ErrorKind::InvalidConnectionState(state) => PublishError::NotConnected(*state),
        ErrorKind::InvalidChannelState(state, _) => PublishError::ChannelUnavailable(*state),
        _ => classify_link_error(error),
    }
}

/// 分类确认阶段或链路错误：连接级故障归为断开，其余归为通道级错误。
fn classify_link_error(error: lapin::Error) -> PublishError {
    match error.kind() {
        ErrorKind::IOError(_)
        | ErrorKind::RuntimeShutdownError(_)
        | ErrorKind::MissingHeartbeatError => PublishError::ConnectionLost(error),
        _ => PublishError::ChannelError(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lapin::message::Delivery;

    /// 默认配置为 16 条在途、5 秒超时、不限配额等待、仅要求服务端确认。
    #[test]
    fn default_config_matches_documented_values() {
        let config = PublisherConfig::default();
        assert_eq!(config.max_inflight, DEFAULT_MAX_INFLIGHT);
        assert_eq!(config.max_inflight, 16);
        assert_eq!(config.publish_timeout, DEFAULT_PUBLISH_TIMEOUT);
        assert_eq!(config.publish_timeout, Duration::from_secs(5));
        assert!(config.acquire_timeout.is_none());
        assert_eq!(config.success_mode, PublishSuccessMode::BrokerAccepted);
        assert!(config.validate().is_ok());
    }

    /// 零并发上限会被拒绝，避免发布调用永久等待配额。
    #[test]
    fn zero_max_inflight_is_rejected() {
        let config = PublisherConfig::new().with_max_inflight(0);
        let error = config.validate().unwrap_err();
        assert!(matches!(error, PublishError::ConfigInvalid(_)));
        assert!(!error.is_outcome_unknown());
    }

    /// 超过 `Semaphore` 允许上限的并发数必须在建发布器前被拒绝，否则 `Semaphore::new` 会 panic。
    #[test]
    fn max_inflight_above_semaphore_limit_is_rejected() {
        let config = PublisherConfig::new().with_max_inflight(Semaphore::MAX_PERMITS + 1);
        let error = config.validate().unwrap_err();
        assert!(matches!(error, PublishError::ConfigInvalid(_)));
        assert!(!error.is_outcome_unknown());

        // 上限本身可用。
        assert!(
            PublisherConfig::new()
                .with_max_inflight(Semaphore::MAX_PERMITS)
                .validate()
                .is_ok()
        );
    }

    /// 路由模式依赖 RETURN 与确认一一对应，多 in-flight 时 lapin 无法精确归因。
    #[test]
    fn routed_mode_requires_single_inflight() {
        let error = PublisherConfig::new()
            .with_max_inflight(2)
            .with_success_mode(PublishSuccessMode::RoutedToQueue)
            .validate()
            .unwrap_err();
        assert!(matches!(error, PublishError::ConfigInvalid(_)));

        assert!(
            PublisherConfig::new()
                .with_max_inflight(1)
                .with_success_mode(PublishSuccessMode::RoutedToQueue)
                .validate()
                .is_ok()
        );
    }

    /// 无法表示的时长在创建发布器前就被拒绝，不会在发布时 panic。
    #[test]
    fn oversized_timeouts_are_rejected() {
        let config = PublisherConfig::new().with_publish_timeout(Duration::MAX);
        assert!(matches!(
            config.validate().unwrap_err(),
            PublishError::ConfigInvalid(_)
        ));

        let config = PublisherConfig::new().with_acquire_timeout(Some(Duration::MAX));
        assert!(matches!(
            config.validate().unwrap_err(),
            PublishError::ConfigInvalid(_)
        ));
    }

    /// 只有在途中断与确认阶段才可能出现结果未知，其余失败都有确定结论。
    #[test]
    fn outcome_unknown_classification_is_explicit() {
        let serialization =
            PublishError::Serialization(serde_json::from_str::<u8>("not-a-number").unwrap_err());
        let io_error: lapin::Error =
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "connection closed").into();

        let deterministic = [
            serialization,
            PublishError::ConfigInvalid("bad"),
            PublishError::Setup(io_error.clone()),
            PublishError::NotConnected(ConnectionState::Closed),
            PublishError::ChannelUnavailable(ChannelState::Closed),
            PublishError::Unroutable {
                reason: "NO_ROUTE".to_owned(),
            },
            PublishError::Nacked {
                reason: "no detail".to_owned(),
            },
            PublishError::AcquireTimeout,
            PublishError::ConnectionBlocked,
            PublishError::EnqueueTimeout,
            PublishError::AttributionLost,
        ];
        for error in &deterministic {
            assert!(!error.is_outcome_unknown(), "unexpected: {error}");
        }

        let unknown = [
            PublishError::PublishTimeout,
            PublishError::ConfirmNotEnabled,
            PublishError::ConnectionLost(io_error.clone()),
            PublishError::ChannelError(io_error),
        ];
        for error in &unknown {
            assert!(error.is_outcome_unknown(), "unexpected: {error}");
        }
    }

    /// 路由模式一旦未取得确认就结束发布，后续不能再依赖退回结果。
    #[test]
    fn abandoned_publish_poisons_routed_attribution() {
        let permits = Arc::new(Semaphore::new(2));
        let lost = AtomicBool::new(false);
        {
            let permit = Arc::clone(&permits).try_acquire_owned().unwrap();
            let mut guard = InflightGuard::new(permit, &lost, true);
            guard.disarm();
        }
        assert!(!lost.load(Ordering::Acquire));

        {
            // 未调用 disarm：模拟超时或取消导致确认未兑现。
            let permit = Arc::clone(&permits).try_acquire_owned().unwrap();
            let _guard = InflightGuard::new(permit, &lost, true);
        }
        assert!(lost.load(Ordering::Acquire));
    }

    /// 非路由模式不记录归因状态，超时或取消不会停用发布器。
    #[test]
    fn non_routed_mode_never_records_attribution_loss() {
        let permits = Arc::new(Semaphore::new(1));
        let lost = AtomicBool::new(false);
        {
            let permit = Arc::clone(&permits).try_acquire_owned().unwrap();
            let _guard = InflightGuard::new(permit, &lost, false);
        }
        assert!(!lost.load(Ordering::Acquire));
    }

    /// 守卫析构时先置位失效标志、再归还配额；下一个配额持有者据此才能看到失效。
    #[test]
    fn guard_marks_attribution_lost_before_releasing_permit() {
        let permits = Arc::new(Semaphore::new(1));
        let lost = AtomicBool::new(false);
        let permit = Arc::clone(&permits).try_acquire_owned().unwrap();
        {
            // 未调用 disarm：模拟确认超时或被取消。
            let _guard = InflightGuard::new(permit, &lost, true);
            assert_eq!(permits.available_permits(), 0);
        }
        assert!(lost.load(Ordering::Acquire));
        assert_eq!(permits.available_permits(), 1);
    }

    /// 构造一条被退回的消息，供归一化测试使用。
    fn returned_message() -> BasicReturnMessage {
        BasicReturnMessage {
            delivery: Delivery::mock(0, "exchange".into(), "route".into(), false, Vec::new()),
            reply_code: 312,
            reply_text: "NO_ROUTE".into(),
        }
    }

    /// 服务端 ACK 在两种成功模式下都算成功；被退回的消息只在路由模式下失败。
    #[test]
    fn ack_is_success_unless_routed_mode_requires_routing() {
        assert!(
            normalize_confirmation(Confirmation::Ack(None), PublishSuccessMode::BrokerAccepted)
                .is_ok()
        );
        assert!(
            normalize_confirmation(Confirmation::Ack(None), PublishSuccessMode::RoutedToQueue)
                .is_ok()
        );

        let error = normalize_confirmation(
            Confirmation::Ack(Some(returned_message())),
            PublishSuccessMode::RoutedToQueue,
        )
        .unwrap_err();
        assert!(matches!(error, PublishError::Unroutable { .. }));
        assert!(!error.is_outcome_unknown());
        assert!(error.to_string().contains("NO_ROUTE"));
        assert!(error.to_string().contains("route"));

        // 默认模式不要求路由，退回消息不影响成功判定。
        assert!(
            normalize_confirmation(
                Confirmation::Ack(Some(returned_message())),
                PublishSuccessMode::BrokerAccepted
            )
            .is_ok()
        );
    }

    /// NACK 表示服务端明确拒绝，属于已确定的失败。
    #[test]
    fn nack_is_deterministic_failure() {
        let error =
            normalize_confirmation(Confirmation::Nack(None), PublishSuccessMode::BrokerAccepted)
                .unwrap_err();
        assert!(matches!(error, PublishError::Nacked { .. }));
        assert!(!error.is_outcome_unknown());
    }

    /// 未启用确认模式的通道无法判断结果，按未知处理。
    #[test]
    fn missing_confirm_mode_is_treated_as_unknown() {
        let error = normalize_confirmation(
            Confirmation::NotRequested,
            PublishSuccessMode::BrokerAccepted,
        )
        .unwrap_err();
        assert!(matches!(error, PublishError::ConfirmNotEnabled));
        assert!(error.is_outcome_unknown());
    }

    /// 连接级错误归为断开，协议级错误归为通道错误，前者更利于调用方判断。
    #[test]
    fn link_errors_are_classified_by_kind() {
        let io_error: lapin::Error =
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed").into();
        assert!(matches!(
            classify_link_error(io_error),
            PublishError::ConnectionLost(_)
        ));

        let protocol_error: lapin::Error = ErrorKind::MissingHeartbeatError.into();
        assert!(matches!(
            classify_link_error(protocol_error),
            PublishError::ConnectionLost(_)
        ));
    }
}
