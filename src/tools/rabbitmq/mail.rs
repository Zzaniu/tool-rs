//! 邮件消息体与发送封装。
//!
//! 载荷类型与 `crate::mq::rabbitmq::mail` 保持一致，但定义在本模块内独立演进：
//! 该 feature 不依赖 `mq`，因此不复用旧模块的类型。
//!
//! 发送接口只提供等待确认的版本。旧模块里不等待 broker 确认的 `send_mail`
//! 不会被复刻——它无法判断消息是否真的被服务端接收。

use crate::rabbitmq::lapin::BasicProperties;
use crate::rabbitmq::publisher::{PublishError, Publisher};
use base64::{Engine as _, engine::general_purpose};
use derive_builder::Builder;

/// 邮件交换机名称，与生产端的既有约定保持一致。
const MAIL_EXCHANGE: &str = "better";

/// 邮件路由键，与生产端的既有约定保持一致。
const MAIL_ROUTING_KEY: &str = "better_mail";

/// AMQP 持久化投递模式。消息不会随 broker 重启丢失，前提是交换机与队列也持久化。
const DELIVERY_MODE_PERSISTENT: u8 = 2;

/// 邮件载荷包装：序列化后作为消息体发送。
#[derive(serde::Serialize)]
pub struct MailInfo<'a> {
    /// 待发送的邮件内容。
    pub mail: &'a Mail<'a>,
}

impl<'a> MailInfo<'a> {
    /// 包装一封邮件。
    pub fn new(mail: &'a Mail<'a>) -> Self {
        Self { mail }
    }
}

/// 邮件内容。
#[derive(Debug, serde::Serialize, Builder)]
pub struct Mail<'a> {
    /// 收件人。
    pub to: &'a str,
    /// 抄送。
    #[builder(setter(into, strip_option), default)]
    pub cc: Option<&'a str>,
    /// 密送。
    #[builder(setter(into, strip_option), default)]
    pub bcc: Option<&'a str>,
    /// 标题。
    pub title: &'a str,
    /// 正文。
    pub body: &'a str,
    /// 附件，每项为 `[文件名, base64 内容]`。
    #[builder(setter(into, strip_option), default)]
    pub attachment: Option<Vec<[String; 2]>>,
    /// 是否海外邮件。
    #[builder(default = "false")]
    pub foreign: bool,
}

/// 附件转换为 `[文件名, base64 内容]` 的形式。
pub trait ToAttachment {
    /// 执行转换。
    fn trans(&self) -> Vec<[String; 2]>;
}

impl ToAttachment for Vec<Attachment<'_>> {
    fn trans(&self) -> Vec<[String; 2]> {
        let mut attachment = Vec::new();
        for item in self.iter() {
            attachment.push([
                item.name.to_owned(),
                general_purpose::STANDARD.encode(item.content),
            ]); // 附件内容需要 base64 编码
        }
        attachment
    }
}

/// 待编码的附件。
pub struct Attachment<'a> {
    /// 文件名。
    pub name: &'a str,
    /// 原始二进制内容。
    pub content: &'a [u8],
}

impl<'a> Attachment<'a> {
    /// 创建附件。
    pub fn new(name: &'a str, content: &'a [u8]) -> Self {
        Self { name, content }
    }
}

impl<'a> Mail<'a> {
    /// 创建邮件内容。
    ///
    /// `attachment` 会被立即 base64 编码一次，之后可直接交给 [`send_mail`]。
    pub fn new<'b: 'a>(
        to: &'b str,
        title: &'b str,
        body: &'b str,
        foreign: bool,
        cc: Option<&'b str>,
        bcc: Option<&'b str>,
        attachment: Option<&'b Vec<Attachment<'b>>>,
    ) -> Mail<'a> {
        let attachment = attachment.map(|info| {
            let mut encoded = Vec::new();
            for item in info {
                encoded.push([
                    item.name.to_owned(),
                    general_purpose::STANDARD.encode(item.content),
                ]); // 附件内容需要 base64 编码
            }
            encoded
        });
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

/// 使用发布器发送邮件消息，并等待服务端确认。
///
/// 成功表示消息满足发布器配置的成功条件（默认仅要求 broker 接收；若发布器使用
/// [`crate::rabbitmq::publisher::PublishSuccessMode::RoutedToQueue`]，还要求消息
/// 至少路由到一个队列），不表示消费者已处理。
///
/// 本函数不声明交换机或队列，也不做重试；失败时可用
/// [`PublishError::is_outcome_unknown`] 判断消息结果是否已确定。
pub async fn send_mail(publisher: &Publisher, mail: &MailInfo<'_>) -> Result<(), PublishError> {
    let content = serde_json::to_vec(mail)?;
    let properties = BasicProperties::default()
        .with_delivery_mode(DELIVERY_MODE_PERSISTENT)
        .with_content_type("application/json".into());
    publisher
        .publish(MAIL_EXCHANGE, MAIL_ROUTING_KEY, &content, properties)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 附件在构造时完成 base64 编码，发送阶段不再改动内容。
    #[test]
    fn attachments_are_base64_encoded() {
        let content = b"hello";
        let attachments = vec![Attachment::new("a.txt", content)];
        let encoded = attachments.trans();

        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0][0], "a.txt");
        assert_eq!(encoded[0][1], general_purpose::STANDARD.encode(content));
    }

    /// 邮件序列化后字段名与既有消费者约定一致。
    #[test]
    fn mail_serializes_with_expected_field_names() {
        let mail = Mail::new("to@example.com", "title", "body", false, None, None, None);
        let json = serde_json::to_value(MailInfo::new(&mail)).unwrap();
        let body = json.get("mail").unwrap();

        assert_eq!(body["to"], "to@example.com");
        assert_eq!(body["title"], "title");
        assert_eq!(body["body"], "body");
        assert_eq!(body["foreign"], false);
        assert!(body.get("cc").is_none() || body["cc"].is_null());
    }

    /// 空附件列表不产生 attachment 字段以外的副作用。
    #[test]
    fn mail_with_attachment_keeps_name_and_content_pair() {
        let attachments = vec![Attachment::new("a.txt", b"hi")];
        let mail = Mail::new(
            "to@example.com",
            "title",
            "body",
            false,
            None,
            None,
            Some(&attachments),
        );
        let json = serde_json::to_value(MailInfo::new(&mail)).unwrap();
        let attachment = json["mail"]["attachment"].as_array().unwrap();

        assert_eq!(attachment.len(), 1);
        assert_eq!(attachment[0][0], "a.txt");
        assert_eq!(attachment[0][1], general_purpose::STANDARD.encode(b"hi"));
    }
}
