//! cluster handle. assumes `scripts/run-e2e.sh` set up the kind cluster and
//! exported `KUBECONFIG`; we only build a `kube::Client` from that.

use anyhow::{Context, Result};
use kube::Client;
use std::sync::Arc;
use tokio::sync::OnceCell;

/// process-wide singleton. tests share one client; namespace isolation is the
/// boundary between tests.
static CLIENT: OnceCell<Arc<Client>> = OnceCell::const_new();

pub async fn client() -> Result<Arc<Client>> {
    let c = CLIENT
        .get_or_try_init(|| async {
            init_tracing();
            let client = Client::try_default()
                .await
                .context("build kube::Client from KUBECONFIG")?;
            Ok::<Arc<Client>, anyhow::Error>(Arc::new(client))
        })
        .await?;
    Ok(c.clone())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_test_writer()
        .try_init();
}
