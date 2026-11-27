use tokio::signal::unix::{SignalKind, signal};

/// Resolves on SIGTERM (`docker stop`) or SIGINT (Ctrl-C).
///
/// In a `FROM scratch` image koma runs as PID 1, where the kernel ignores
/// signals that have no handler, so this handler is what makes stopping work.
pub async fn shutdown() {
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
