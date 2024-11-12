use super::rabbitmq;
use crate::error_caused_str;
use crate::mq::rabbitmq::rabbitmq::{RabbitMqConnectInfo, RabbitMqConnectInfoBuilder};
use amqprs::channel::{BasicPublishArguments, Channel};
use amqprs::{BasicProperties, DELIVERY_MODE_PERSISTENT};
use anyhow::{anyhow, Result as AnyResult};
use base64::{engine::general_purpose, Engine as _};
use derive_builder::Builder;
use log::info;

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

pub async fn send_mail(rabbitmq_channel: &Channel, mail: &MailInfo<'_>) -> AnyResult<()> {
    // DELIVERY_MODE_PERSISTENT: 这个消息是持久化的, 不会随着重启而丢弃, 前提是队列和交换机都是持久化的
    let basic_properties = BasicProperties::default()
        .with_delivery_mode(DELIVERY_MODE_PERSISTENT)
        .finish();
    let arguments = BasicPublishArguments::new("better", "better_mail");
    let content = serde_json::to_vec(mail)?;
    rabbitmq_channel
        .basic_publish(basic_properties, content, arguments)
        .await
        .map_err(|err| anyhow!("发送邮件失败, err = {}", error_caused_str(&err)))?;
    info!("publish mail message success");
    Ok(())
}

pub async fn get_mail_rabbitmq_conn(
    connect_info: RabbitMqConnectInfo<'_>,
) -> Result<rabbitmq::ConnAndChannel, amqprs::error::Error> {
    let queue = "mail";
    let exchange_name = "better";
    let routing_key = "better_mail";
    let exchange_type = "direct";
    let dlx_queue = "dead-mail-queue";
    let dlx_exchange_name = "dead-mail-exchange";
    let dlx_routing_key = "dead-mail-queue";
    let dlx_exchange_type = "fanout";

    let decl_info =
        rabbitmq::RabbitMqDeclareInfo::new(queue, exchange_name, routing_key, exchange_type);
    let dlx_info = rabbitmq::RabbitMqDeclareInfo::new(
        dlx_queue,
        dlx_exchange_name,
        dlx_routing_key,
        dlx_exchange_type,
    );
    let conn = rabbitmq::new(&connect_info, &decl_info, Some(&dlx_info)).await?;
    Ok(conn)
}

pub async fn get_mail_rabbitmq_conn_with_env(
) -> Result<rabbitmq::ConnAndChannel, amqprs::error::Error> {
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
