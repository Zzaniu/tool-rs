use anyhow::{anyhow, Result as AnyResult};
use derive_builder::Builder;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MessageBuilder, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::hash::{DefaultHasher, Hash, Hasher};

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

// 邮件信息结构体
#[allow(unused)]
#[derive(Debug, Builder)]
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
    pub attach: Option<Vec<AttachmentInfo>>,
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
        if let Some(attach) = &self.attach {
            attach.hash(state);
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

#[derive(Debug, Builder)]
pub struct Mailer {
    #[builder(setter(into))]
    pub addr: String,
    #[builder(setter(into))]
    pub pass_word: String,
    #[builder(setter(into), default="倍通邮件助手.to_string()")]
    pub name: String,
    #[builder(setter(into))]
    pub server_url: String,
}

impl Mailer {
    pub async fn send_mail(&self, mail_info: MailInfo) -> AnyResult<()> {
        let mut builder = self.get_message_builder()?;
        builder = builder.subject(mail_info.title);

        for v in mail_info.to.split(',') {
            builder = builder.to(v.parse().map_err(|err| anyhow!("邮箱格式错误: {}", err))?);
        }

        if let Some(cc) = mail_info.cc {
            if !cc.is_empty() {
                for v in cc.split(',') {
                    builder = builder.cc(v
                        .parse()
                        .map_err(|err| anyhow!("抄送邮箱 格式错误: {}", err))?);
                }
            }
        }

        if let Some(bcc) = mail_info.bcc {
            if !bcc.is_empty() {
                for v in bcc.split(',') {
                    builder = builder.bcc(
                        v.parse()
                            .map_err(|err| anyhow!("密送邮箱 格式错误: {}", err))?,
                    );
                }
            }
        }

        // see https://docs.rs/lettre/0.10.0-rc.4/lettre/message/index.html#complex-mime-body
        let email = if let Some(att) = mail_info.attach {
            // 如果有附件的话, 添加附件
            let mut part = MultiPart::related().singlepart(SinglePart::html(mail_info.body));
            for v in att {
                part = part.singlepart(
                    Attachment::new(v.name).body(
                        v.content,
                        v.content_type
                            .parse()
                            .map_err(|err| anyhow!("附件类型错误: {}", err))?,
                    ),
                );
            }
            builder
                .multipart(
                    MultiPart::mixed() // 混合
                        .multipart(part),
                )
                .map_err(|err| anyhow!("构建附件邮件失败: {}", err))?
        } else {
            builder
                .header(ContentType::TEXT_HTML)
                .body(mail_info.body)
                .map_err(|err| anyhow!("构建普通邮件失败: {}", err))?
        };

        self.get_transporter()?
            .send(email)
            .await
            .map_err(|err| anyhow!("发送邮件失败: {}", err))?;
        Ok(())
    }

    fn get_message_builder(&self) -> AnyResult<MessageBuilder> {
        // note: 用户直接回复邮件时, reply-to 就是默认的收件人. 如果用户不指定它, from 就是默认的收件人
        Ok(Message::builder().from(Mailbox::new(
            Some(self.name.clone()),
            self.addr.clone()
                .parse::<Address>()
                .map_err(|err| anyhow!("get_message_builder from 格式错误: {}", err))?,
        )))
    }

    pub fn get_transporter(&self) -> AnyResult<AsyncSmtpTransport<Tokio1Executor>> {
        // TODO: 是否需要池化? 参考 pool_config
        Ok(AsyncSmtpTransport::<Tokio1Executor>::relay(self.server_url.as_str())
            .map_err(|e| anyhow!("AsyncSmtpTransport::<Tokio1Executor>::relay 失败: {}", e))?
            .credentials(Credentials::new(
                self.name.clone(),
                self.pass_word.to_owned(),
            ))
            .build())
    }
}


