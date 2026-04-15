use anyhow::{anyhow, Result as AnyResult};
use base64::Engine;
use derive_builder::Builder;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MessageBuilder, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::PoolConfig;
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Deserializer};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::OnceLock;

#[derive(Debug, Clone, Builder)]
pub struct AttachmentInfo {
    #[builder(setter(into))]
    pub name: String,
    #[builder(setter(into), default = "mime::APPLICATION_OCTET_STREAM.to_string()")]
    pub content_type: String,
    pub content: Vec<u8>,
}

impl Hash for AttachmentInfo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // content_type 暂时不参与 hash
        self.name.hash(state);
        self.content.hash(state);
    }
}

impl<'de> Deserialize<'de> for AttachmentInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let [name, content_base64]: [String; 2] = Deserialize::deserialize(deserializer)?;
        let content = base64::engine::general_purpose::STANDARD
            .decode(content_base64)
            .map_err(serde::de::Error::custom)?;
        Ok(AttachmentInfo {
            name,
            content_type: "application/octet-stream".to_string(),
            content,
        })
    }
}

impl AttachmentInfo {
    pub fn into_lettre_attachment(self) -> AnyResult<SinglePart> {
        Ok(Attachment::new(self.name).body(
            self.content,
            self.content_type
                .parse()
                .map_err(|err| anyhow!("附件类型错误: {}", err))?,
        ))
    }
}

// 邮件信息结构体
#[allow(unused)]
#[derive(Debug, Deserialize, Builder)]
pub struct MailInfo {
    #[builder(setter(into))]
    pub title: String,
    #[builder(setter(into))]
    pub body: String,
    #[builder(setter(into))]
    pub to: String,
    #[builder(setter(into, strip_option), default)]
    pub cc: Option<String>,
    #[builder(setter(into, strip_option), default)]
    pub bcc: Option<String>,
    #[builder(setter(strip_option), default)]
    pub attachment: Option<Vec<AttachmentInfo>>,
}

impl Hash for MailInfo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.title.hash(state);
        self.body.hash(state);
        self.to.hash(state);
        if let Some(cc) = &self.cc {
            cc.hash(state);
        }
        if let Some(bcc) = &self.bcc {
            bcc.hash(state);
        }
        if let Some(attachment) = &self.attachment {
            attachment.hash(state);
        }
    }
}

impl MailInfo {
    pub fn get_hash_value(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

#[derive(Debug, Deserialize)]
pub struct MailInfoWrapper {
    pub mail: MailInfo,
}

fn default_mailer_name() -> &'static str {
    "倍通邮件助手"
}

#[derive(Debug, Builder)]
pub struct Mailer {
    #[builder(setter(into))]
    pub addr: String,
    #[builder(setter(into))]
    pub pass_word: String,
    #[builder(setter(into), default = "default_mailer_name().to_string()")]
    pub name: String,
    #[builder(setter(into))]
    pub server_url: String,

    #[builder(setter(skip), default)]
    transport: OnceLock<AsyncSmtpTransport<Tokio1Executor>>,
    #[builder(setter(skip), default)]
    from_mailbox: OnceLock<Mailbox>,
}

impl Mailer {
    pub async fn send_mail(&self, mail_info: MailInfo) -> AnyResult<()> {
        let mut builder = Message::builder()
            .from(self.get_from_mailbox()?)
            .subject(mail_info.title);

        // 解析并添加收件人/抄送/密送
        builder = self.apply_mailboxes(builder, mail_info.to, "收件人", MessageBuilder::to)?;

        if let Some(cc) = mail_info.cc
            && !cc.is_empty()
        {
            builder = self.apply_mailboxes(builder, cc, "抄送", MessageBuilder::cc)?;
        }

        if let Some(bcc) = mail_info.bcc
            && !bcc.is_empty()
        {
            builder = self.apply_mailboxes(builder, bcc, "密送", MessageBuilder::bcc)?;
        }

        let email = if let Some(att) = mail_info.attachment {
            let mut multipart = MultiPart::mixed().singlepart(SinglePart::html(mail_info.body));
            for v in att {
                multipart = multipart.singlepart(v.into_lettre_attachment()?);
            }
            builder.multipart(multipart)
        } else {
            builder.header(ContentType::TEXT_HTML).body(mail_info.body)
        }
        .map_err(|err| anyhow!("构建邮件失败: {}", err))?;

        self.get_transporter()?
            .send(email)
            .await
            .map_err(|err| anyhow!("发送邮件失败: {}", err))?;
        Ok(())
    }

    fn apply_mailboxes(
        &self,
        mut builder: MessageBuilder,
        input: String,
        label: &str,
        f: fn(MessageBuilder, Mailbox) -> MessageBuilder,
    ) -> AnyResult<MessageBuilder> {
        for v in input.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let mailbox = v
                .parse()
                .map_err(|err| anyhow!("{}邮箱格式错误 '{}': {}", label, v, err))?;
            builder = f(builder, mailbox);
        }
        Ok(builder)
    }

    fn get_from_mailbox(&self) -> AnyResult<Mailbox> {
        if let Some(mailbox) = self.from_mailbox.get() {
            return Ok(mailbox.clone());
        }
        let mailbox = Mailbox::new(
            Some(self.name.clone()),
            self.addr
                .parse::<Address>()
                .map_err(|err| anyhow!("发件人邮箱格式错误: {}", err))?,
        );
        // 已经被初始化则会返回 Err(value)
        let _ = self.from_mailbox.set(mailbox.clone());
        Ok(self.from_mailbox.get().unwrap().clone())
    }

    pub fn get_transporter(&self) -> AnyResult<AsyncSmtpTransport<Tokio1Executor>> {
        if let Some(transport) = self.transport.get() {
            return Ok(transport.clone());
        }
        println!("获取链接");
        let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(self.server_url.as_str())
            .map_err(|e| anyhow!("AsyncSmtpTransport::<Tokio1Executor>::relay 失败: {}", e))?
            .credentials(Credentials::new(
                self.addr.clone(),
                self.pass_word.to_owned(),
            ))
            .pool_config(PoolConfig::new().max_size(10)) // 默认其实也是10
            .build();
        let _ = self.transport.set(transport.clone());
        Ok(self.transport.get().unwrap().clone())
    }
}
