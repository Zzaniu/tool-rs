pub async fn wait_for_quit() {
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.unwrap();
    }
    #[cfg(unix)]
    {
        use log::info;
        use tokio::signal::unix::SignalKind;
        // SIGINT, ctrl_c/kill -2
        let mut signal_ctrl_c = tokio::signal::unix::signal(SignalKind::interrupt())
            .expect("Failed to catch the SIGINT signal");
        // SIGTERM, kill
        let mut signal_term = tokio::signal::unix::signal(SignalKind::terminate())
            .expect("Failed to catch the SIGTERM signal");
        tokio::select! {
            _ = signal_ctrl_c.recv() => info!("Received SIGINT signal"),
            _ = signal_term.recv() => info!("Received SIGTERM signal"),
        }
    }
}
