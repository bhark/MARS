//! Process-wide signal handling for service modes.

use tokio_util::sync::CancellationToken;

/// Spawn a SIGINT/SIGTERM watcher. The first signal cancels the returned
/// token (graceful shutdown). A second signal escalates to `exit(130)` so
/// operators can break out of a stuck drain.
pub(crate) fn install_signal_handler() -> CancellationToken {
    let token = CancellationToken::new();
    let watcher = token.clone();
    tokio::spawn(async move {
        if let Err(e) = wait_for_termination().await {
            tracing::warn!(error = %e, "signal handler unavailable; signal-based shutdown disabled");
            return;
        }
        tracing::info!("signal received; initiating graceful shutdown");
        watcher.cancel();
        // second signal escalates: force exit so a stuck task can't trap the
        // operator. exit code 130 = killed by SIGINT.
        if wait_for_termination().await.is_ok() {
            tracing::warn!("second signal received; forcing exit");
            std::process::exit(130);
        }
    });
    token
}

/// Resolve when either SIGINT (ctrl_c) or SIGTERM is received. Production
/// orchestrators (k8s, systemd) send SIGTERM at pod stop; without this the
/// graceful drain never runs and the kernel kills the process at the grace
/// deadline.
async fn wait_for_termination() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            res = tokio::signal::ctrl_c() => res,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}
