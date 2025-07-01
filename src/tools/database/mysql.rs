use anyhow::Result as AnyResult;
use log::LevelFilter;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
use sqlx::{self, ConnectOptions};
use sqlx::{Encode, Executor, FromRow, MySql, Type};
use std::env;

pub async fn connect_with_env() -> MySqlPool {
    let user_name = env::var("MYSQL_USER_NAME").unwrap();
    let user_pass = env::var("MYSQL_USER_PASS").unwrap();
    let host = env::var("MYSQL_HOST").unwrap();
    let port = env::var("MYSQL_PORT").unwrap().parse::<u16>().unwrap();
    let db = env::var("MYSQL_DB").unwrap();

    let opts = MySqlConnectOptions::new()
        .host(host.as_str())
        .username(user_name.as_str())
        .password(user_pass.as_str())
        .database(db.as_str())
        .port(port);

    #[cfg(debug_assertions)]
    let opts = opts.log_statements(LevelFilter::Info);
    #[cfg(not(debug_assertions))]
    let opts = opts.log_statements(LevelFilter::Off);

    MySqlPoolOptions::new()
        .connect_with(opts)
        .await
        .expect("无法链接数据库")
}

pub fn default_config() -> MySqlConnectOptions {
    let opts = MySqlConnectOptions::new();
    #[cfg(not(debug_assertions))]
    {
        opts.log_statements(LevelFilter::Off)
    }
    #[cfg(debug_assertions)]
    opts.log_statements(LevelFilter::Info)
}

pub async fn connect(config: MySqlConnectOptions) -> MySqlPool {
    // 默认最大10个链接, 最小0个链接. 空闲时间10分钟, 生命周期30分钟, acquire_timeout 30秒
    MySqlPoolOptions::new()
        .max_connections(5)
        .connect_with(config)
        .await
        .expect("无法链接数据库")
}

pub async fn list_option_db_info<'a, V, T>(
    executor: impl Executor<'_, Database = MySql>,
    no: V,
    sql_str: &'static str,
) -> AnyResult<Option<Vec<T>>>
where
    V: 'a + Send + Encode<'a, MySql> + Type<MySql>,
    T: for<'r> FromRow<'r, <MySql as sqlx::Database>::Row> + Send + Unpin,
{
    let data = sqlx::query_as::<_, T>(sql_str)
        .bind(no)
        .fetch_all(executor)
        .await?;
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(data))
}

#[deprecated(note = "use `fetch_optional` instead")]
pub async fn get_option_db_info<'a, V, T>(
    executor: impl Executor<'_, Database = MySql>,
    no: V,
    sql_str: &'static str,
) -> AnyResult<Option<T>>
where
    V: 'a + Send + Encode<'a, MySql> + Type<MySql>,
    T: for<'r> FromRow<'r, <MySql as sqlx::Database>::Row> + Send + Unpin,
{
    match sqlx::query_as::<_, T>(sql_str)
        .bind(no)
        .fetch_one(executor)
        .await
    {
        Ok(data) => Ok(Some(data)),
        Err(e) => {
            if let sqlx::Error::RowNotFound = e {
                Ok(None)
            } else {
                Err(e.into())
            }
        }
    }
}
