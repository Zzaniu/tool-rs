use super::rabbitmq;
use super::v2::{RabbitMqPublishError, RabbitMqPublisher};
use crate::error_caused_str;
use crate::mq::rabbitmq::rabbitmq::{RabbitMqConnectInfo, RabbitMqConnectInfoBuilder};
use amqprs::channel::{BasicPublishArguments, Channel};
use amqprs::connection::Connection;
use amqprs::{BasicProperties, DELIVERY_MODE_PERSISTENT};
use anyhow::{Result as AnyResult, anyhow};
use base64::{Engine as _, engine::general_purpose};
use derive_builder::Builder;
use log::info;

const MAIL_EXCHANGE: &str = "better";
const MAIL_ROUTING_KEY: &str = "better_mail";

#[derive(serde::Serialize)]
pub struct MailInfo<'a> {
    pub mail: &'a Mail<'a>,
}

impl<'a> MailInfo<'a> {
    pub fn new(mail: &'a Mail<'a>) -> Self {
        Self { mail }
    }
}

#[derive(Debug, serde::Serialize, Builder)]
pub struct Mail<'a> {
    pub to: &'a str,
    #[builder(setter(into, strip_option), default)]
    pub cc: Option<&'a str>,
    #[builder(setter(into, strip_option), default)]
    pub bcc: Option<&'a str>,
    pub title: &'a str,
    pub body: &'a str,
    #[builder(setter(into, strip_option), default)]
    pub attachment: Option<Vec<[String; 2]>>,
    #[builder(default = "false")]
    pub foreign: bool,
}

pub trait ToAttachment {
    fn trans(&self) -> Vec<[String; 2]>;
}

impl ToAttachment for Vec<Attachment<'_>> {
    fn trans(&self) -> Vec<[String; 2]> {
        let mut attachment = Vec::new();
        for item in self.iter() {
            attachment.push([
                item.name.to_owned(),
                general_purpose::STANDARD.encode(item.content),
            ]); // 如果是附件的话, 需要使用 base64 进行编码
        }
        attachment
    }
}

pub struct Attachment<'a> {
    pub name: &'a str,
    pub content: &'a [u8],
}

impl<'a> Attachment<'a> {
    pub fn new(name: &'a str, content: &'a [u8]) -> Self {
        Self { name, content }
    }
}

impl<'a> Mail<'a> {
    pub fn new<'b: 'a>(
        to: &'b str,
        title: &'b str,
        body: &'b str,
        foreign: bool,
        cc: Option<&'b str>,
        bcc: Option<&'b str>,
        attachment: Option<&'b Vec<Attachment<'b>>>,
    ) -> Mail<'a> {
        let attachment = match attachment {
            None => None,
            Some(info) => {
                let mut attachment = Vec::new();
                for item in info {
                    attachment.push([
                        item.name.to_owned(),
                        general_purpose::STANDARD.encode(item.content),
                    ]); // 如果是附件的话, 需要使用 base64 进行编码
                }
                Some(attachment)
            }
        };
        Mail {
            to,
            cc,
            bcc,
            title,
            body,
            foreign,
            attachment,
        }
    }
}

/// 使用旧版 RabbitMQ channel 发布邮件。
///
/// 返回 `Ok(())` 只表示消息已加入 amqprs 客户端的发送队列；该接口不会等待
/// RabbitMQ Publisher Confirm，且 `mandatory` 默认为 `false`，因此不可路由消息
/// 可能被 broker 丢弃。需要确认 broker 已接手消息时，请使用 [`send_mail_v2`]。
/// 即使 channel 启用了 confirm，也不会在本函数中等待或关联 ACK/NACK。
#[deprecated(note = "本接口不等待 broker 确认；请使用 send_mail_v2 确认消息发布结果")]
pub async fn send_mail(rabbitmq_channel: &Channel, mail: &MailInfo<'_>) -> AnyResult<()> {
    // DELIVERY_MODE_PERSISTENT: 这个消息是持久化的, 不会随着重启而丢弃, 前提是队列和交换机都是持久化的
    let basic_properties = BasicProperties::default()
        .with_delivery_mode(DELIVERY_MODE_PERSISTENT)
        .finish();
    let arguments = BasicPublishArguments::new(MAIL_EXCHANGE, MAIL_ROUTING_KEY);
    let content = serde_json::to_vec(mail)?;
    rabbitmq_channel
        .basic_publish(basic_properties, content, arguments)
        .await
        .map_err(|err| anyhow!("发送邮件失败, err = {}", error_caused_str(&err)))?;
    info!("邮件消息已加入客户端发送队列，尚未等待 broker 确认");
    Ok(())
}

/// 使用 V2 publisher 发送邮件。
///
/// V2 默认收到 RabbitMQ ACK 即成功；如果 publisher 使用
/// `PublishSuccessMode::RoutedToQueue`，则还会要求消息至少路由到一个队列。
pub async fn send_mail_v2(
    publisher: &RabbitMqPublisher,
    mail: &MailInfo<'_>,
) -> Result<(), RabbitMqPublishError> {
    let basic_properties = BasicProperties::default()
        .with_delivery_mode(DELIVERY_MODE_PERSISTENT)
        .finish();
    let arguments = BasicPublishArguments::new(MAIL_EXCHANGE, MAIL_ROUTING_KEY);
    let content = serde_json::to_vec(mail).map_err(RabbitMqPublishError::from)?;
    publisher
        .publish(basic_properties, content, arguments)
        .await
}

pub async fn get_mail_rabbitmq_conn(
    connect_info: RabbitMqConnectInfo<'_>,
) -> Result<Connection, amqprs::error::Error> {
    rabbitmq::connect(&connect_info).await
}

pub async fn get_mail_rabbitmq_conn_with_env() -> Result<Connection, amqprs::error::Error> {
    let rabbitmq_host = std::env::var("RABBITMQ_HOST").expect("rabbitmq 服务地址错误");
    let rabbitmq_port = std::env::var("RABBITMQ_PORT")
        .expect("rabbitmq 服务端口号错误")
        .parse::<u16>()
        .expect("rabbitmq 服务端口号转换错误");
    let rabbitmq_user = std::env::var("RABBITMQ_USER").expect("rabbitmq 用户名错误");
    let rabbitmq_pass = std::env::var("RABBITMQ_PASS").expect("rabbitmq 密码错误");
    let virtual_host_result = std::env::var("RABBITMQ_VIRTUAL_HOST");
    let virtual_host = virtual_host_result.as_ref().map(|x| x.as_str()).ok();
    let connect_info = RabbitMqConnectInfoBuilder::default()
        .host(rabbitmq_host.as_str())
        .port(rabbitmq_port)
        .username(rabbitmq_user.as_str())
        .password(rabbitmq_pass.as_str())
        .virtual_host(virtual_host)
        .build()
        .unwrap();
    get_mail_rabbitmq_conn(connect_info).await
}
