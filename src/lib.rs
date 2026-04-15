mod tools;

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

pub async fn wait_for_quit() {
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.unwrap();
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::SignalKind;
        // SIGINT, ctrl_c/kill -2
        let mut signal_ctrl_c = tokio::signal::unix::signal(SignalKind::interrupt())
            .expect("Failed to catch the SIGINT signal");
        // SIGTERM, kill
        let mut signal_term = tokio::signal::unix::signal(SignalKind::terminate())
            .expect("Failed to catch the SIGTERM signal");
        tokio::select! {
            _ = signal_ctrl_c.recv() => info!("Received SIGTERM signal"),
            _ = signal_term.recv() => info!("Received SIGINT signal"),
        }
    }
}
