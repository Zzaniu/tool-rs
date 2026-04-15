use amqprs::channel::{Channel, ConfirmSelectArguments};
use amqprs::{
    Ack, BasicProperties, Cancel, Close, CloseChannel, Nack, Return, callbacks,
    connection::{Connection, OpenConnectionArguments},
};
use async_trait::async_trait;
use derive_builder::Builder;
use log::info;

#[derive(Debug, Builder)]
pub struct RabbitMqConnectInfo<'a> {
    pub host: &'a str,
    pub port: u16,
    pub username: &'a str,
    pub password: &'a str,
    #[builder(setter(into), default)]
    pub virtual_host: Option<&'a str>,
}

impl<'a> RabbitMqConnectInfo<'a> {
    pub fn new(
        host: &'a str,
        port: u16,
        username: &'a str,
        password: &'a str,
        virtual_host: Option<&'a str>,
    ) -> RabbitMqConnectInfo<'a> {
        Self {
            host,
            port,
            username,
            password,
            virtual_host,
        }
    }
}

pub struct ConnectionCallback;
pub struct ChannelCallback;

#[async_trait]
impl callbacks::ConnectionCallback for ConnectionCallback {
    async fn close(
        &mut self,
        connection: &Connection,
        close: Close,
    ) -> Result<(), amqprs::error::Error> {
        info!("handle close request for connection {connection}, cause: {close}",);
        Ok(())
    }

    async fn blocked(&mut self, connection: &Connection, reason: String) {
        info!("handle blocked request for connection {connection}, reason: {reason}",);
    }

    async fn unblocked(&mut self, connection: &Connection) {
        info!("handle unblocked request for connection {connection}");
    }

    async fn secret_updated(&mut self, connection: &Connection) {
        info!("handle secret_updated request for connection {connection}");
    }
}

#[async_trait]
impl callbacks::ChannelCallback for ChannelCallback {
    async fn close(
        &mut self,
        channel: &Channel,
        close: CloseChannel,
    ) -> Result<(), amqprs::error::Error> {
        info!(
            "handle close request for channel {}, cause: {}",
            channel, close
        );
        Ok(())
    }

    async fn cancel(
        &mut self,
        channel: &Channel,
        cancel: Cancel,
    ) -> Result<(), amqprs::error::Error> {
        info!(
            "handle cancel request for consumer {} on channel {}",
            cancel.consumer_tag(),
            channel
        );
        Ok(())
    }

    async fn flow(
        &mut self,
        channel: &Channel,
        active: bool,
    ) -> Result<bool, amqprs::error::Error> {
        info!(
            "handle flow request active={} for channel {}",
            active, channel
        );
        Ok(true)
    }

    // 感觉 ack 的没啥需要记录的
    async fn publish_ack(&mut self, _channel: &Channel, _ack: Ack) {}

    async fn publish_nack(&mut self, channel: &Channel, nack: Nack) {
        info!(
            "handle publish nack delivery_tag={} on channel {}",
            nack.delivery_tag(),
            channel
        )
    }

    async fn publish_return(
        &mut self,
        channel: &Channel,
        ret: Return,
        basic_properties: BasicProperties,
        content: Vec<u8>,
    ) {
        info!(
            "handle publish return {} on channel {}, content size: {}, basic_properties = {}",
            ret,
            channel,
            content.len(),
            basic_properties
        )
    }
}

pub async fn new_channel_with_consume(
    connection: &Connection,
) -> Result<Channel, amqprs::error::Error> {
    new_channel(connection, false).await
}

pub async fn new_channel_with_publish(
    connection: &Connection,
) -> Result<Channel, amqprs::error::Error> {
    new_channel(connection, true).await
}

pub async fn new_channel(
    connection: &Connection,
    confirm: bool,
) -> Result<Channel, amqprs::error::Error> {
    // None 表示 channel 是使用的随机 ID
    let channel = connection.open_channel(None).await?;
    channel.register_callback(ChannelCallback).await?;

    // 此方法将通道设置为使用发布者确认, 客户端只能在非事务性通道上使用此方法.
    if confirm {
        channel
            .confirm_select(ConfirmSelectArguments::default())
            .await?;
    }
    Ok(channel)
}

pub async fn connect(
    connect_info: &RabbitMqConnectInfo<'_>,
) -> Result<Connection, amqprs::error::Error> {
    let mut args = OpenConnectionArguments::new(
        connect_info.host,
        connect_info.port,
        connect_info.username,
        connect_info.password,
    );

    if let Some(virtual_host) = connect_info.virtual_host {
        args.virtual_host(virtual_host);
    }

    let connection = Connection::open(&args).await?;
    connection.register_callback(ConnectionCallback).await?;

    Ok(connection)
}
