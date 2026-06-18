mod tools;

use std::time::{SystemTime, UNIX_EPOCH};
pub use tools::*;

pub type AnyResult<T> = anyhow::Result<T>;

pub fn error_caused_str(mut err: &(dyn std::error::Error + 'static)) -> String {
    use std::fmt::Write;
    let mut msg = format!("{err}");
    while let Some(source) = err.source() {
        let _ = write!(msg, "\n\nCaused by: {source}");
        err = source;
    }
    msg
}

pub fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

pub fn unix_timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
