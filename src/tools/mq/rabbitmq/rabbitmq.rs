pub use amqprs::channel::Channel;
use amqprs::channel::{
    ConfirmSelectArguments, ExchangeDeclareArguments, QueueBindArguments, QueueDeclareArguments,
};
use amqprs::{
    callbacks,
    connection::{Connection, OpenConnectionArguments},
    Ack, BasicProperties, Cancel, Close, CloseChannel, FieldTable, Nack, Return,
};
use async_trait::async_trait;
use derive_builder::Builder;
use log::info;

#[derive(Debug, Builder)]
pub struct RabbitMqConnectInfo<'a> {
    host: &'a str,
    port: u16,
    username: &'a str,
    password: &'a str,
    #[builder(setter(into), default)]
    virtual_host: Option<&'a str>,
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

pub struct RabbitMqDeclareInfo<'a> {
    pub queue: &'a str,
    pub exchange_name: &'a str,
    pub routing_key: &'a str,
    pub exchange_type: &'a str,
}

impl<'a> RabbitMqDeclareInfo<'a> {
    pub fn new(
        queue: &'a str,
        exchange_name: &'a str,
        routing_key: &'a str,
        exchange_type: &'a str,
    ) -> RabbitMqDeclareInfo<'a> {
        Self {
            queue,
            exchange_name,
            routing_key,
            exchange_type,
        }
    }
}

pub struct ConnAndChannel {
    pub connection: Connection,
    pub channel: Channel,
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
        info!(
            "handle close request for connection {}, cause: {}",
            connection, close
        );
        Ok(())
    }

    async fn blocked(&mut self, connection: &Connection, reason: String) {
        info!(
            "handle blocked request for connection {}, reason: {}",
            connection, reason
        );
    }

    async fn unblocked(&mut self, connection: &Connection) {
        info!("handle unblocked request for connection {}", connection);
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

pub async fn connect(
    connect_info: &RabbitMqConnectInfo<'_>,
) -> Result<ConnAndChannel, amqprs::error::Error> {
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

    let channel = connection.open_channel(None).await?; // None 表示 channel 是使用的随机 ID
    channel.register_callback(ChannelCallback).await?;
    Ok(ConnAndChannel {
        connection,
        channel,
    })
}

async fn init(
    channel: &Channel,
    declare_info: &RabbitMqDeclareInfo<'_>,
    dlx_info: Option<&RabbitMqDeclareInfo<'_>>,
) -> Result<(), amqprs::error::Error> {
    if let Some(dlx_info) = dlx_info {
        let (queue_name, _, _) = channel
            .queue_declare(QueueDeclareArguments::durable_client_named(dlx_info.queue))
            .await?
            .unwrap();
        let exchange_arguments =
            ExchangeDeclareArguments::new(dlx_info.exchange_name, dlx_info.exchange_type)
                .durable(true)
                .finish();
        channel.exchange_declare(exchange_arguments).await?;
        channel
            .queue_bind(QueueBindArguments::new(
                queue_name.as_str(),
                dlx_info.exchange_name,
                dlx_info.routing_key,
            ))
            .await?;
    }
    let mut arg = QueueDeclareArguments::durable_client_named(declare_info.queue);
    if let Some(dlx_info) = dlx_info {
        let mut argument = FieldTable::new();
        argument.insert(
            "x-dead-letter-exchange".try_into().unwrap(),
            dlx_info.exchange_name.into(),
        );
        arg.arguments(argument);
    }
    let (queue_name, _, _) = channel.queue_declare(arg).await?.unwrap();
    let exchange_arguments =
        ExchangeDeclareArguments::new(declare_info.exchange_name, declare_info.exchange_type)
            .durable(true)
            .finish();
    channel.exchange_declare(exchange_arguments).await?;
    channel
        .queue_bind(QueueBindArguments::new(
            queue_name.as_str(),
            declare_info.exchange_name,
            declare_info.routing_key,
        ))
        .await?;
    Ok(())
}

pub async fn new(
    connect_info: &RabbitMqConnectInfo<'_>,
    declare_info: &RabbitMqDeclareInfo<'_>,
    dlx_info: Option<&RabbitMqDeclareInfo<'_>>,
) -> Result<ConnAndChannel, amqprs::error::Error> {
    let conn = connect(connect_info).await?;
    init(&conn.channel, declare_info, dlx_info).await?;
    // 此方法将通道设置为使用发布者确认, 客户端只能在非事务性通道上使用此方法.
    conn.channel
        .confirm_select(ConfirmSelectArguments::default())
        .await?;
    Ok(conn)
}
