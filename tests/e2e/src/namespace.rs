//! per-test namespace allocation + teardown. each test gets its own namespace
//! `mars-e2e-<short>-<rand6>`; namespaces are deleted on drop unless the env
//! `MARS_E2E_KEEP=1` is set, in which case they're left for inspection.

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{DeleteParams, ObjectMeta, PostParams};
use kube::{Api, Client};
use rand::RngExt;
use rand::distr::Alphanumeric;
use std::sync::Arc;
use tracing::info;

pub struct NamespaceGuard {
    pub name: String,
    client: Arc<Client>,
    keep_on_drop: bool,
}

impl NamespaceGuard {
    pub async fn create(client: Arc<Client>, prefix: &str) -> Result<Self> {
        let suffix: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(6)
            .map(|c| char::from(c).to_ascii_lowercase())
            .collect();
        let name = format!("mars-e2e-{prefix}-{suffix}");
        let api: Api<Namespace> = Api::all((*client).clone());
        let ns = Namespace {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                labels: Some(
                    [
                        ("mars.forn.dk/e2e".to_string(), "1".to_string()),
                        ("mars.forn.dk/test-prefix".to_string(), prefix.to_string()),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        api.create(&PostParams::default(), &ns)
            .await
            .with_context(|| format!("create namespace {name}"))?;
        info!(%name, "created test namespace");
        let keep_on_drop = std::env::var_os("MARS_E2E_KEEP").is_some();
        Ok(Self {
            name,
            client,
            keep_on_drop,
        })
    }
}

impl Drop for NamespaceGuard {
    fn drop(&mut self) {
        if self.keep_on_drop {
            info!(name = %self.name, "MARS_E2E_KEEP set; leaving namespace");
            return;
        }
        // best-effort blocking delete via a fresh tokio runtime so Drop works
        // outside an async context. timeouts handled by k8s GC.
        let name = self.name.clone();
        let client = self.client.clone();
        let _ = std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!(error = %e, "namespace drop: cannot spin tokio runtime");
                    return;
                }
            };
            rt.block_on(async move {
                let api: Api<Namespace> = Api::all((*client).clone());
                if let Err(e) = api.delete(&name, &DeleteParams::background()).await {
                    tracing::warn!(%name, error = %e, "namespace drop: delete failed");
                }
            });
        })
        .join();
    }
}
