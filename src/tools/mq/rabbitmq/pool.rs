//! RabbitMQ publisher 池：下一迭代的实施方案与任务清单。
//!
//! 状态：仅记录设计和待办，尚未实现池化功能，也尚未引入 deadpool 依赖。
//! 以下接口名、配置和流程均为拟议方案；完成对应任务后再提供可调用接口。
//!
//! # 一、目标与使用边界
//!
//! - 保留直接使用 `v2::RabbitMqPublisher` 的方式；现有构造、发布和错误契约继续适用。
//! - 新增 `pool::RabbitMqPublisherPool`，让调用方按需要选择池化发布。
//! - 池化对象是完整的 `RabbitMqPublisher`，每个对象仍独占一个启用 confirm 的 channel。
//!   一个池共享一个 connection；每个 channel 内串行，不同 channel 可以同时等待确认。
//! - 使用 deadpool 的 managed 池承担容量限制、借用排队、对象归还和失效对象替换。
//!   不再手写一套池，不把现有 publisher 改成单通道多消息同时等待确认的实现。
//! - 保留 ACK、NACK、RETURN、取消保护和结果未知判断；池层不自动重发业务消息。
//! - `max_size = N` 限制池可借出的 publisher 数量。按一次发布借用一个对象使用时，
//!   最多 N 条消息同时等待确认；共享连接的带宽、流控和故障仍会影响所有 channel。
//! - 多 channel 不提供跨 channel 的全局消息顺序。依赖顺序的调用方应直接使用一个
//!   publisher，或持续持有同一个借用对象并按顺序发布。
//! - 不在本次池化任务中加入自动重连、连接池、业务重试、消息去重或单通道流水线。
//!
//! # 二、文件与对外路径
//!
//! 实施顺序：先确认依赖与初始化取消安全，再实现错误/配置、Manager、借用对象和池，
//! 最后补充示例与测试。下面每一节都作为下一迭代的独立任务检查。
//!
//! - [ ] `Cargo.toml`：增加可选依赖并接入 `mq` feature。
//! - [ ] `Cargo.lock`：只更新依赖解析实际需要的条目，检查新增依赖及最低 Rust 版本。
//! - [ ] 本文件：用实现替换已完成的任务说明，保留必要的中文模块与接口文档。
//! - [ ] `rabbitmq/mod.rs`：确认导出 `pub mod pool`；实施时检查现状，避免重复添加。
//! - [ ] `v2.rs`：优先复用现有实现；只有初始化保护、必要的内部校验或清理确实需要时
//!   才增加内部辅助方法，不暴露底层 channel，不重写确认处理逻辑。
//! - [ ] `mail.rs`：保留 `send_mail_v2(&RabbitMqPublisher, ...)` 的参数和返回值。
//!   文档说明先从池借用 publisher，再调用现有函数即可。
//!
//! 源码位于 `src/tools/mq/rabbitmq/pool.rs`。由于 crate 根部的 `tools` 模块是私有的，
//! 且通过 `pub use tools::*` 重导出，用户实际使用的公开路径应是
//! `tool::mq::rabbitmq::pool`，直接使用的路径仍是 `tool::mq::rabbitmq::v2`。
//!
//! # 三、依赖、版本与 Tokio 运行时
//!
//! - [ ] 实施时先核实可用且未撤回的 deadpool 版本、Manager 接口及 Rust 版本要求，
//!   再确定版本约束并生成锁文件。前文提过的 0.13.1 未得到有效核实，不直接照抄。
//! - [ ] 本方案的池行为已对照本机 deadpool 0.12.3 源码；改用其他版本时重新核对
//!   `managed/manager.rs`、`pool.rs`、`object.rs`、`errors.rs` 和 `builder.rs`。
//! - [ ] 使用 `deadpool`，不使用绑定其他 RabbitMQ 客户端的适配包。
//!   依赖配置应包含 `optional = true`、`default-features = false`，
//!   并仅启用 `features = ["managed", "rt_tokio_1"]`。
//! - [ ] 在现有 `mq` feature 中增加 `dep:deadpool`。直接使用与池化使用是接口选择，
//!   本轮不额外拆出一个新的 Cargo feature，也不修改现有默认 feature 集合。
//! - [ ] 池 builder 显式设置 `.runtime(deadpool::Runtime::Tokio1)`。
//!   启用 `rt_tokio_1` 只提供运行时支持，不等于已为这个池指定运行时。
//! - [ ] 文档明确：deadpool 的 Object 归还是同步操作，不因归还本身而需要 runtime；
//!   池超时需要指定 runtime，而 amqprs 的 I/O 和 channel 析构清理本身也依赖 Tokio。
//!   使用、销毁和清理 RabbitMQ 资源应在 Tokio 运行时仍存活时完成。
//! - [ ] 通过依赖树和锁文件差异统计新增 crate，不承诺固定“净新增两个”。
//!   例如已核对的 0.12.3 还依赖 num_cpus，最终增量取决于版本、平台和已有依赖。
//!
//! # 四、拟议配置与公开接口
//!
//! - [ ] 新增 `RabbitMqPublisherPoolConfig`，采用中文文档、构建方法和
//!   `#[non_exhaustive]`，允许将来兼容地扩展配置。
//! - [ ] `RabbitMqPublisherPoolConfig::new(max_size)` 要求调用方显式指定容量，
//!   不沿用 deadpool 按 CPU 数量推导的默认容量。拒绝 0 和依赖无法支持的极大容量，
//!   返回配置错误，避免无期限等待或底层 semaphore 构造时 panic。
//! - [ ] 配置提供 `wait_timeout`、`create_timeout`、`recycle_timeout`，
//!   类型均为 `Option<Duration>`；`None` 表示对应阶段不设置时间上限。
//!   初始建议 wait/create 取 `RabbitMqPublisherConfig::default().confirm_timeout`，
//!   recycle 默认为 None，因为本方案回收检查只有本地状态读取，不执行网络探测。
//! - [ ] 默认时间从现有配置读取，不复制硬编码秒数。配置创建后各超时独立，
//!   修改 publisher 的 confirm_timeout 不应悄悄改动显式配置过的池超时。
//! - [ ] 校验配置时保留零时长的合法语义；尤其 wait 为零应表示立即尝试借用。
//!   对极大时长做计时器范围校验，不能因构造截止时间溢出而 panic。
//! - [ ] publisher 配置独立传入 Manager，池内所有对象使用相同的成功模式和确认超时。
//!   初始化前验证配置；若要复用 v2 的校验，可提取 `pub(super)` 辅助方法，
//!   不为池层重复实现一套逐渐不一致的校验规则。
//!
//! 拟议的 `RabbitMqPublisherPool` 接口：
//!
//! - `new(&Connection, max_size) -> Result<Self, RabbitMqPoolError>`：
//!   同步创建默认配置的池句柄，先校验配置，按需创建 publisher，不预先握手。
//! - `new_with_config(&Connection, RabbitMqPublisherConfig, RabbitMqPublisherPoolConfig)`：
//!   同步构建指定配置的池，同样返回 Result；成功仅表示本地构建成功，不表示 broker 可达。
//! - `get().await -> Result<PooledRabbitMqPublisher, RabbitMqPoolError>`：
//!   借出一个可用 publisher；首次获取或失效替换时才创建专用 channel。
//! - `publish(properties, content, arguments).await -> Result<(), RabbitMqPoolError>`：
//!   便利方法，内部完成借用、一次发布和归还，消息参数与现有 publisher 保持一致。
//! - `close()`：关闭池并唤醒等待借用的调用，具体关闭边界见生命周期任务。
//! - 为池句柄实现 Clone；克隆应共享同一个池、容量和 connection，不新建一组 channel。
//!   状态指标接口按实际需要添加，不把扩展监控作为基础池化的前置条件。
//!
//! # 五、错误映射与消息结果
//!
//! - [ ] 新增 `RabbitMqPoolError`，使用 thiserror、中文说明和 `#[non_exhaustive]`。
//!   不向 `RabbitMqPublishError` 塞入池获取错误，保持直接使用的错误契约。
//! - [ ] 至少区分以下错误来源，并保留 source 链：
//!   `InvalidConfig` 表示池/发布配置无效；
//!   `Build` 包装 deadpool 的 BuildError；
//!   `Acquire` 包装 `deadpool::managed::PoolError<RabbitMqPublishError>`；
//!   `Publish` 包装原始 RabbitMqPublishError。
//!   InvalidConfig 若包装原错误，同样保留 source，不统一压成 String。
//! - [ ] Manager 的关联类型使用 `Type = RabbitMqPublisher`、
//!   `Error = RabbitMqPublishError`。create 的初始化失败进入 Acquire::Backend，
//!   实际发布失败进入 Publish；即使底层类型相同，也必须按所处阶段显式映射。
//! - [ ] 在池错误上提供 `is_outcome_unknown()`：
//!   InvalidConfig、Build、Acquire 返回 false；Publish 委托给原错误的同名方法。
//!   对今后新增的未明确分类变体，沿用保守原则按结果未知处理。
//! - [ ] Acquire 阶段的所有失败对“本次调用”都属于确定未发布，包括 Wait/Create
//!   超时、池关闭、初始化失败、运行时配置错误。前提是 create/recycle 不发布业务消息，
//!   本方案不安装会发送业务消息的 hooks。
//! - [ ] 不根据 Acquire 包装的内层错误名称推断结果，也不把它转换成确认超时。
//!   不应把历史消息结果未知，误认为这次尚未取得对象的调用也已发送。
//! - [ ] 保留池超时的阶段信息；核对选定版本是否向调用方暴露 Recycle 超时。
//!   已核对的 0.12.3 会在 recycle 失败或超时后丢弃对象并继续获取，不能写成
//!   “每次 recycle 失败都会直接返回 Acquire 错误”。
//! - [ ] false 只表示本次消息结果确定，不保证立即重试能成功；权限、交换器配置、
//!   容量不足和连接故障仍需调用方按原因处理。
//!
//! # 六、Manager 的实现方式
//!
//! - [ ] 定义私有 Manager，保存 connection 的共享句柄、publisher 配置，
//!   以及下一节需要的初始化并发保护；Connection::clone 不建立新的 TCP 连接。
//! - [ ] create 开头检查 `connection.is_open()`；已关闭时直接返回发布前不可用错误，
//!   避免无效初始化。状态检查只是快照，仍需处理随后断线和握手失败。
//! - [ ] 经初始化取消保护后调用 `RabbitMqPublisher::new_with_config`。
//!   复用其“打开 channel、注册专用回调、启用 confirm”顺序，不自行覆盖 connection
//!   回调，不使用业务正在共享的普通 channel，也不每次 publish 都重新开 channel。
//! - [ ] recycle 只调用 `publisher.is_usable()`：为 true 则返回 Ok，
//!   否则返回 `RecycleError::message(...)`，由 deadpool 淘汰并按需创建替代对象。
//! - [ ] recycle 是借出旧对象之前的检查，不是 Object 析构归还时的异步检查。
//!   不可用对象暂时归还池中没有关系，但下次不得不经检查就借给业务发送。
//! - [ ] 不把逻辑停用标志重置为 false，不清空旧事件后强行复用旧 channel，
//!   不重置仍在使用的 channel 的 delivery tag。
//! - [ ] NACK 或路由失败是否停用 publisher，继续由现有实现决定。
//!   池层不能仅因一次 publish 返回 Err 就认定对象永久损坏；以 is_usable 为准。
//! - [ ] Manager 不持有池句柄或已借出对象，避免循环引用；无需为了普通归还实现
//!   自定义异步 Drop，也不把 Manager::detach 误当成可 await 的清理接口。
//!
//! # 七、初始化取消安全与资源清理：必须先验证
//!
//! 这是实现 create 超时之前的必做项。deadpool 的超时会丢弃 Manager::create future，
//! 调用方取消 get 也有同样效果；池本身能归还容量，不代表 amqprs 的握手也可安全取消。
//!
//! - [ ] 阅读实施版本的 amqprs `Connection::open_channel`、资源注册和 reader 应答路径。
//!   当前 2.1.5 的资源注册和 responder 注册存在 `acker.send(...).expect(...)`；
//!   OpenChannelOk 转发失败也会成为 reader 错误。初始化中途丢弃 receiver 可能影响
//!   整个共享 connection，不可只用 timeout 包住现有构造后就声称取消安全。
//! - [ ] 若实施版本仍有该问题，Manager 使用独立 Tokio 初始化任务：先取得一个
//!   OwnedSemaphorePermit，再 spawn 任务，在任务内完整 await publisher 构造。
//!   Manager 仅等待 JoinHandle，等待被取消时不 abort 已进入握手的初始化任务。
//! - [ ] 等待初始化许可阶段可以安全取消；取得许可后、spawn 之前不要插入 await。
//!   创建许可移入任务，由任务持有到初始化及必要清理结束。任务开始握手前再检查一次连接。
//! - [ ] 初始化任务共享独立 semaphore，上限建议不超过池容量。create 超时之后，
//!   deadpool 的容量可能已释放，但旧初始化仍在运行；这层许可避免反复超时导致
//!   后台握手任务无限堆积。它只限制初始化，不在发布时重复获取许可。
//! - [ ] 调用方放弃等待后，任务若最终成功，产生的 publisher 必须被销毁和清理，
//!   不能遗留在无人消费的结果缓存里；任务失败或 panic 也必须释放许可。
//!   JoinError 映射为获取阶段错误，避免直接 unwrap；若需转换为原错误类型，
//!   使用语义明确的初始化失败包装并保留可用的原因信息。
//! - [ ] 分别覆盖打开 channel 之前、握手中和拿到 channel 后启用 confirm 期间的取消。
//!   不声称“create 超时表示服务端没有创建 channel”：它只保证本次业务消息未发布。
//! - [ ] 有界后台初始化仍可能等待连接恢复或关闭，create_timeout 限制的是调用方等待。
//!   不偷偷关闭共享 connection，也不因某个借用者超时而 abort 正在握手的任务。
//!   外层负责按连接生命周期执行关闭；测试该关闭能结束未完成初始化。
//! - [ ] 确认 amqprs Channel 析构会尝试异步关闭，且需要存活的 Tokio runtime。
//!   不把 drop 当成已经收到 Channel.CloseOk；反复淘汰/替换时检查服务端 channel 数量。
//! - [ ] `max_size` 约束池容量，不保证 broker 上“正在初始化、正在关闭、可借用”
//!   三类 channel 的瞬时总数也不超过它；结合 connection.channel_max 和其他 channel
//!   的使用量留出余量。正常复用时不应持续增长。
//! - [ ] 初始化任务封装尽量放在 pool 内部；只有需要访问 v2 私有资源才能正确清理时，
//!   才添加最小的内部辅助接口。不要顺带公开 channel 或更改直接发布的结果语义。
//!
//! # 八、借用对象与发布流程
//!
//! - [ ] 定义公开 `PooledRabbitMqPublisher`，内部封装私有 Manager 对应的 Object。
//!   实现 `Deref<Target = RabbitMqPublisher>` 和 `AsRef<RabbitMqPublisher>`，
//!   以兼容原 publish 和 send_mail_v2；不通过公开别名泄露私有 Manager 类型。
//! - [ ] 借用对象不实现 Clone，不提供脱离池的原始对象提取或底层 channel 访问。
//!   普通字段析构即可触发 deadpool 的同步归还，不重复实现一套释放许可逻辑。
//! - [ ] `get` 将获取失败映射为 Acquire，只在确认取得对象后返回借用包装器。
//!   池句柄可以跨任务共享；借用对象只持有到本次发布或明确需要的连续操作结束。
//! - [ ] 池的 publish 按下述流程实现，整个过程没有全池级发布锁：
//!
//! ```text
//! 借用对象 = get().await                         // 失败 -> Acquire，尚未发布
//! 结果 = 借用对象.publish(原始参数).await          // 原确认逻辑，错误 -> Publish
//! 销毁借用对象                                  // 归还；失效对象下次 recycle 淘汰
//! 返回结果                                      // 不自动换 channel 重发
//! ```
//!
//! - [ ] 必须把借用对象持有到 publish future 完成或被取消，不能先归还再等待确认。
//!   不把发布 spawn 到无人持有借用对象的后台任务，不为每条消息另建通道。
//! - [ ] 同一个借用对象允许顺序多次发布；若用户并发调用同一对象，仍由它自己的
//!   publish_lock 串行化，不会额外获得池内并发能力。
//! - [ ] recycle 通过后到 publish 之前通道仍可能关闭。保留原 publisher 的发布前检查，
//!   将 PublisherUnavailable / PublisherUnavailableChannelClosed 原样作为 Publish 返回。
//! - [ ] 默认不对获取后失败做换对象重试，确定未发布的错误也交给调用方决定。
//!   deadpool 淘汰空闲坏对象后的资源替换不属于业务消息重发，应在文档中区分。
//! - [ ] 发布中取消依赖原 InFlightGuard 停用当前 publisher；借用对象随后归还，
//!   下次 recycle 必须淘汰。其他 channel 上已借出的发布不应被池层主动取消。
//!   共享 connection 自身故障造成的连带失败则按真实状态返回。
//! - [ ] 等待池许可时取消，没有开始业务发布；等待 publisher 内部串行锁时取消，
//!   沿用现有“尚未开始本次发布”的语义。不要把所有取消统一描述为已发送或未发送。
//!
//! # 九、超时预算与背压
//!
//! - [ ] wait 控制等待池许可；create 控制单次对象创建等待；recycle 控制单次回收检查。
//!   confirm_timeout 仍从取得 publisher 内部发布锁并通过可用性检查后开始，
//!   覆盖客户端入队和等待 confirm，不包含池等待时间。
//! - [ ] 有限 wait 超时提供背压，突发流量下会出现确定未发布的获取失败；
//!   None 允许无限等待，零时长允许立即尝试。说明这是配置选择，不保证突发调用全部成功。
//! - [ ] 对便利 publish，实际总耗时为“get 实际耗时 + publish 实际耗时”。
//!   get 可能连续淘汰多个坏对象；不能把 wait + create + confirm_timeout 宣称为严格上限。
//! - [ ] 手动借用对象后发生的业务处理耗时，以及同一借用对象上其他发布的排队时间，
//!   也不计入单次 confirm_timeout；示例中避免持有对象执行无关慢操作。
//! - [ ] 基础版本提供阶段超时，不额外承诺全流程总截止时间。
//!   调用方若用 timeout 包住整个 publish，超时发生在发布期间时结果可能未知，
//!   原取消保护仍必须生效；不能把外层 timeout 一律解释为确定未发布。
//! - [ ] 如未来增加总截止时间接口，单独设计阶段识别、错误分类和取消测试。
//!   仅包住 get 的获取总预算仍属于未发布，但也要满足上一节的初始化取消安全要求。
//!
//! # 十、连接故障、关闭与生命周期
//!
//! - [ ] 池不拥有外部 connection 的独占关闭权。池 close 不调用 Connection::close，
//!   避免影响直接使用的 publisher、消费者或同连接上的其他业务 channel。
//! - [ ] 池 close 禁止之后的借用并唤醒等待许可的调用；空闲对象被移除，
//!   已借出的对象不强制中断，正常完成或取消后再销毁。
//! - [ ] 核对选定版本里已经取得许可、正在 create/recycle 的 get 与 close 的竞争语义，
//!   写明确切行为并测试，不把 close 描述为“所有正在获取或发布的任务立即终止”。
//!   若封装层要求 close 后不再交付新对象，在交付前复查关闭状态并丢弃该对象，
//!   同时定义仍可能与 close 并发完成的边界，不能声称消除了全部竞争。
//! - [ ] 关闭池时不触发业务重发，不把当前 Publish 的结果未知改写成 Acquire::Closed。
//!   同一池句柄的任一 clone 调用 close，其他 clone 也观察到这个池已关闭。
//! - [ ] connection 关闭后，所有 channel 都不可用。旧 Manager 持有的 clone 不会自动
//!   变成新连接；外层建立新 connection 后要新建池，再停止接收旧池的新请求并收尾。
//! - [ ] 不在 create 中针对已经关闭的 connection 无限循环重建 channel。
//!   快速返回原连接不可用，让外层采取明确的恢复策略。
//! - [ ] 停机顺序写入文档：停止新请求，等待或明确取消持有借用对象的任务，
//!   关闭池并清理 channel，然后由 connection 所有者关闭连接，最后退出 Tokio runtime。
//!   若存在进行中的初始化任务，按第七节先完成或通过外层关闭连接使其结束。
//! - [ ] 基础 close 是关闭借用入口，不承诺等待所有 channel 完成协议关闭；
//!   如需可 await 的优雅停机，再单独增加 drain/shutdown 设计与清理时限。
//!
//! # 十一、使用示例与中文文档任务
//!
//! - [ ] 提供直接使用示例：创建一个 RabbitMqPublisher，再调用其 publish 或 send_mail_v2。
//! - [ ] 提供池化便利接口示例：创建容量为 N 的池，多任务克隆同一个池句柄，
//!   分别调用 pool.publish，观察多个 channel 同时等待确认。
//! - [ ] 提供池化邮件示例：先 `let publisher = pool.get().await?`，
//!   再 `send_mail_v2(&publisher, &mail).await?`，通过 Deref 转换复用原接口。
//!   提醒调用方错误边界有“获取失败”和“邮件序列化/发布失败”两层。
//! - [ ] 不为适配池强迫 send_mail_v2 泛型化，也不复制一份邮件正文构造流程。
//! - [ ] 对新公开类型、字段、构造函数、借用/归还、错误判断、超时、取消和 close
//!   补全中文注释；明确成功仍不表示消费者已处理，池化也不提供 exactly-once。
//! - [ ] 初始化是按需进行；如果用户要求启动即检查 broker，可显式 get 后归还一个对象，
//!   这只验证一个 publisher，不声称已经预热全部 N 个 channel。
//! - [ ] 跨 channel 的 delivery tag 不构成全局业务编号；即使未来接口返回 tag，
//!   也不能用它独立完成业务去重或跨 channel 对账。
//!
//! # 十二、测试任务与验收标准
//!
//! 测试继续放在源文件末尾的 `#[cfg(test)]` 模块，不新增顶层 tests 目录。
//! 不依赖 broker 的单元测试默认运行；需要 broker 的测试显式标记并说明运行条件，
//! 用 #[ignore] 或项目认可的方式执行，不在缺少环境时静默跳过并冒充通过。
//!
//! - [ ] 配置/构建：拒绝零容量、无法支持的容量和超大超时；验证零时长语义、
//!   默认时间来源、显式超时不联动、Clone 共享容量、Tokio runtime 配置与 feature 组合。
//! - [ ] 错误映射：Acquire 各来源均为 false；Publish 的已知/未知结果与原错误一致；
//!   source 链和 timeout 阶段保留，原始 CloseChannel 详情不被池层抹掉。
//! - [ ] 容量与归还：持有 N 个对象后，第 N+1 个获取等待或 Wait 超时；
//!   释放一个后可以获取，取消排队不泄漏许可。通过计数/同步信号验证，不靠长时间 sleep。
//! - [ ] 实际并发：两个借用对象使用不同 channel，能在第一条确认尚未完成时发起第二条；
//!   同一对象仍串行。不得仅测试借用数就声称端到端发布已并发。
//! - [ ] 确认隔离：不同 channel 的 ACK/NACK/RETURN 不串扰；BrokerAccepted 和
//!   RoutedToQueue 都保留现有语义，已知 RETURN 不被后续断线或超时覆盖。
//! - [ ] 失效替换：空闲对象被 broker 关闭后，下次借用淘汰旧对象并新建；
//!   发布超时/取消停用的对象同样淘汰，旧对象的迟到确认不影响新对象。
//! - [ ] 回收竞争：recycle 检查通过后关闭 channel，实际 publish 返回原错误，
//!   不自动换 channel 重发；其他健康对象仍能使用。
//! - [ ] 初始化取消：在资源注册、OpenChannelOk 和 confirm_select 等阶段制造延迟，
//!   分别取消 get 或触发 create 超时，确认共享连接未因此 panic/关闭，
//!   后台任务数量受限、许可释放、无接收者的成功对象最终清理。
//! - [ ] 断线：共享连接关闭后 create 快速失败，所有旧对象停止复用；
//!   用新连接新建池后恢复发布，不把旧 connection clone 当成自动重连句柄。
//! - [ ] 关闭：等待者得到 Closed；已借出对象按约定收尾；close/create 的竞争符合文档；
//!   关闭池不主动关闭外部 connection，同连接上不属于池的 channel 可继续使用。
//! - [ ] 资源稳定：多轮创建、归还、淘汰、取消和关闭后检查 channel 数量及初始化任务，
//!   确认不会持续增长；测试失败时清理临时交换器、队列和连接。
//! - [ ] 兼容性：原直接 publisher 的确认测试继续通过；send_mail_v2 可接收池借用对象；
//!   公共类型不泄露私有 Manager；编译示例验证真实公开路径。
//!
//! 实现完成后的检查命令：
//!
//! ```text
//! cargo check --no-default-features
//! cargo check --no-default-features --features mq
//! cargo build --all-features
//! cargo test --all-features
//! cargo clippy --all-features
//! cargo doc --all-features --no-deps
//! cargo fmt --all -- --check
//! ```
//!
//! 有锁文件后使用 --locked；缓存齐全时可加 --offline。需要 broker 的测试单独执行并记录
//! 实际结果。若全仓检查被已有问题阻塞，明确区分既有问题与本次变更，不顺带修改无关文件。
//! 仓库使用外部 Cargo target 目录时遵循实际写入权限，不为跑检查悄悄改全局配置。
//!
//! 最终验收：直接使用和池化使用均有清晰示例；多个 channel 可并发等待确认；
//! 错误结果语义保持准确；取消、超时、失效替换和关闭均有测试；未引入自动业务重试；
//! 初始化取消不会破坏共享连接，正常运行和故障收尾后资源数量保持稳定。
