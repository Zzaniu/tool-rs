//! RabbitMQ 连接参数与连接构造。
//!
//! 本模块只负责按 [`RabbitMqConnectInfo`] 拼装 AMQP URI 并建立连接，不维护连接
//! 状态机，也不自动重连。需要自动重连时，向 [`connect_with_properties`] 传入
//! 启用了 `enable_auto_recover` 的 [`ConnectionProperties`]。
//!
//! URI 中的用户名、密码与 vhost 一律做百分号编码，因此包含 `/`、`@`、`%` 等
//! 字符时也能正确连接；vhost 缺省为 `/`（编码为 `%2F`）。

use crate::rabbitmq::lapin;
use derive_builder::Builder;
use lapin::options::ConfirmSelectOptions;
use lapin::{Channel, Connection, ConnectionProperties};
use thiserror::Error;

/// RabbitMQ 的默认 vhost。
const DEFAULT_VHOST: &str = "/";

/// 百分号编码时保持原样的字符集合（RFC 3986 unreserved 中除字母数字外的部分）。
const UNRESERVED: &[u8] = b"-._~";

/// RabbitMQ 连接参数。
///
/// 使用 [`RabbitMqConnectInfoBuilder`] 构造，或调用 [`RabbitMqConnectInfo::new`]。
/// `heartbeat` 为 `None` 时使用服务端协商值。
#[derive(Debug, Builder)]
pub struct RabbitMqConnectInfo<'a> {
    /// 服务地址，可以是域名、IPv4 或 IPv6 字面量。
    pub host: &'a str,
    /// 服务端口，AMQP 默认 5672、AMQPS 默认 5671。
    pub port: u16,
    /// 用户名。
    pub username: &'a str,
    /// 密码。
    pub password: &'a str,
    /// vhost，`None` 或空串表示默认 vhost `/`。
    #[builder(setter(into), default)]
    pub virtual_host: Option<&'a str>,
    /// 心跳间隔（秒），`None` 时由服务端协商。
    #[builder(setter(into), default)]
    pub heartbeat: Option<u16>,
}

impl<'a> RabbitMqConnectInfo<'a> {
    /// 创建连接参数。
    pub fn new(
        host: &'a str,
        port: u16,
        username: &'a str,
        password: &'a str,
        virtual_host: Option<&'a str>,
        heartbeat: Option<u16>,
    ) -> RabbitMqConnectInfo<'a> {
        RabbitMqConnectInfo {
            host,
            port,
            username,
            password,
            virtual_host,
            heartbeat,
        }
    }

    /// 按当前参数拼装 AMQP URI。
    ///
    /// 用户名、密码与 vhost 会做百分号编码；IPv6 字面量会自动加方括号；
    /// `heartbeat` 以查询参数形式附加。
    pub fn build_uri(&self) -> String {
        let username = percent_encode(self.username);
        let password = percent_encode(self.password);
        let host = format_host(self.host);
        // 空串与 None 等价：不指定 vhost 时使用默认 vhost，URI 中必须显式编码。
        let virtual_host = match self.virtual_host {
            Some(value) if !value.is_empty() => percent_encode(value),
            _ => percent_encode(DEFAULT_VHOST),
        };
        let mut uri = format!(
            "amqp://{username}:{password}@{host}:{}/{virtual_host}",
            self.port
        );
        if let Some(heartbeat) = self.heartbeat {
            uri.push_str(&format!("?heartbeat={heartbeat}"));
        }
        uri
    }
}

/// 连接失败原因：环境变量缺失/非法，或 AMQP 建连失败。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConnectEnvError {
    /// 必需的环境变量未设置。
    #[error("环境变量 {0} 未设置")]
    Missing(&'static str),
    /// 端口环境变量无法解析为 `u16`。
    #[error("环境变量 {name} 的值无法解析为端口: {value}")]
    InvalidPort {
        /// 环境变量名。
        name: &'static str,
        /// 原始值。
        value: String,
    },
    /// 心跳环境变量无法解析为 `u16`。
    #[error("环境变量 {name} 的值无法解析为心跳间隔: {value}")]
    InvalidHeartbeat {
        /// 环境变量名。
        name: &'static str,
        /// 原始值。
        value: String,
    },
    /// AMQP 建连失败。
    #[error(transparent)]
    Connection(#[from] lapin::Error),
}

/// 使用默认 [`ConnectionProperties`] 建立连接。
pub async fn connect(info: &RabbitMqConnectInfo<'_>) -> lapin::Result<Connection> {
    connect_with_properties(info, ConnectionProperties::default()).await
}

/// 使用调用方提供的 [`ConnectionProperties`] 建立连接。
///
/// 自动重连、连接名、认证提供者等都在 `properties` 上配置；本函数不做额外覆盖。
pub async fn connect_with_properties(
    info: &RabbitMqConnectInfo<'_>,
    properties: ConnectionProperties,
) -> lapin::Result<Connection> {
    Connection::connect(info.build_uri().as_str(), properties).await
}

/// 从环境变量读取连接参数并建立连接。
///
/// 读取的变量：`RABBITMQ_HOST`、`RABBITMQ_PORT`、`RABBITMQ_USER`、
/// `RABBITMQ_PASS`、`RABBITMQ_VIRTUAL_HOST`（可选）、`RABBITMQ_HEARTBEAT`（可选）。
///
/// 与旧模块 `mq::rabbitmq::mail::get_mail_rabbitmq_conn_with_env` 的差异：变量
/// 缺失或非法时返回错误而不是 panic，因为库代码不应替调用方决定是否终止进程。
pub async fn connect_with_env() -> Result<Connection, ConnectEnvError> {
    let host = env_var("RABBITMQ_HOST")?;
    let port_value = env_var("RABBITMQ_PORT")?;
    let port = port_value
        .parse::<u16>()
        .map_err(|_| ConnectEnvError::InvalidPort {
            name: "RABBITMQ_PORT",
            value: port_value,
        })?;
    let username = env_var("RABBITMQ_USER")?;
    let password = env_var("RABBITMQ_PASS")?;
    let virtual_host = std::env::var("RABBITMQ_VIRTUAL_HOST").ok();
    let heartbeat = match std::env::var("RABBITMQ_HEARTBEAT") {
        Ok(value) => Some(
            value
                .parse::<u16>()
                .map_err(|_| ConnectEnvError::InvalidHeartbeat {
                    name: "RABBITMQ_HEARTBEAT",
                    value,
                })?,
        ),
        Err(_) => None,
    };

    let info = RabbitMqConnectInfo {
        host: host.as_str(),
        port,
        username: username.as_str(),
        password: password.as_str(),
        virtual_host: virtual_host.as_deref(),
        heartbeat,
    };
    Ok(connect(&info).await?)
}

/// 在连接上创建一个未启用发布确认的普通通道。
pub async fn open_channel(connection: &Connection) -> lapin::Result<Channel> {
    connection.create_channel().await
}

/// 在连接上创建通道并启用发布确认。
///
/// 只有启用确认的通道才能通过 `basic_publish` 返回的 `PublisherConfirm` 拿到
/// 服务端的发布结果；[`crate::rabbitmq::publisher::Publisher`] 内部使用此函数。
pub async fn open_confirm_channel(connection: &Connection) -> lapin::Result<Channel> {
    let channel = connection.create_channel().await?;
    channel
        .confirm_select(ConfirmSelectOptions::default())
        .await?;
    Ok(channel)
}

/// 读取必需的环境变量，缺失时返回 [`ConnectEnvError::Missing`]。
fn env_var(name: &'static str) -> Result<String, ConnectEnvError> {
    std::env::var(name).map_err(|_| ConnectEnvError::Missing(name))
}

/// IPv6 字面量需要方括号，其余地址原样使用。
fn format_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// 对 URI 组成部分做百分号编码，未保留字符以外的字节一律编码为 `%XX`。
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => encoded.push(byte as char),
            byte if UNRESERVED.contains(&byte) => encoded.push(byte as char),
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info<'a>(virtual_host: Option<&'a str>, heartbeat: Option<u16>) -> RabbitMqConnectInfo<'a> {
        RabbitMqConnectInfo::new("127.0.0.1", 5672, "guest", "guest", virtual_host, heartbeat)
    }

    /// 未指定 vhost 时使用默认 vhost，且路径必须编码为 `%2F`。
    #[test]
    fn default_virtual_host_is_percent_encoded() {
        assert_eq!(
            info(None, None).build_uri(),
            "amqp://guest:guest@127.0.0.1:5672/%2F"
        );
        assert_eq!(
            info(Some(""), None).build_uri(),
            "amqp://guest:guest@127.0.0.1:5672/%2F"
        );
    }

    /// vhost 内的 `/` 必须编码，否则会被解析成多层路径。
    #[test]
    fn virtual_host_separators_are_escaped() {
        assert_eq!(
            info(Some("app/prod"), None).build_uri(),
            "amqp://guest:guest@127.0.0.1:5672/app%2Fprod"
        );
    }

    /// 用户名与密码中的保留字符必须编码，避免破坏 URI 结构。
    #[test]
    fn credentials_special_characters_are_escaped() {
        let info =
            RabbitMqConnectInfo::new("broker", 5671, "user@corp", "p@ss:word%", Some("vh"), None);
        assert_eq!(
            info.build_uri(),
            "amqp://user%40corp:p%40ss%3Aword%25@broker:5671/vh"
        );
    }

    /// IPv6 字面量需要方括号，已经是方括号形式时不再重复添加。
    #[test]
    fn ipv6_host_is_bracketed() {
        let info = RabbitMqConnectInfo::new("::1", 5672, "guest", "guest", None, None);
        assert_eq!(info.build_uri(), "amqp://guest:guest@[::1]:5672/%2F");

        let info = RabbitMqConnectInfo::new("[::1]", 5672, "guest", "guest", None, None);
        assert_eq!(info.build_uri(), "amqp://guest:guest@[::1]:5672/%2F");
    }

    /// 心跳以查询参数形式附加，未设置时不出现查询串。
    #[test]
    fn heartbeat_is_appended_as_query() {
        assert_eq!(
            info(Some("vh"), Some(30)).build_uri(),
            "amqp://guest:guest@127.0.0.1:5672/vh?heartbeat=30"
        );
        assert!(!info(None, None).build_uri().contains('?'));
    }

    /// builder 需要能覆盖可选字段，保持与旧模块一致的使用方式。
    #[test]
    fn builder_sets_optional_fields() {
        let info = RabbitMqConnectInfoBuilder::default()
            .host("broker")
            .port(5672)
            .username("user")
            .password("pass")
            .virtual_host("vh")
            .heartbeat(60u16)
            .build()
            .unwrap();

        assert_eq!(
            info.build_uri(),
            "amqp://user:pass@broker:5672/vh?heartbeat=60"
        );
    }
}
