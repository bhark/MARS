//! cluster handle. assumes `scripts/run-e2e.sh` set up the kind cluster and
//! exported `KUBECONFIG`; we only build a `kube::Client` from that.
//!
//! a fresh client is built per call. caching across `#[tokio::test]` boundaries
//! is unsafe: kube::Client wraps a tower::buffer worker task that's spawned on
//! whichever runtime is live at construction time, and dies when that runtime
//! drops. namespace isolation remains the boundary between tests.

use anyhow::{Context, Result};
use kube::Client;
use std::sync::Arc;

pub async fn client() -> Result<Arc<Client>> {
    init_tracing();
    let client = Client::try_default()
        .await
        .context("build kube::Client from KUBECONFIG")?;
    Ok(Arc::new(client))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_test_writer()
        .try_init();
}
