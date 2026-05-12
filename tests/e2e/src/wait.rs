//! generic poll-with-timeout helpers. each poll catches errors and continues
//! until either the predicate returns Some, or the deadline is hit.

use anyhow::{Result, anyhow};
use k8s_openapi::api::apps::v1::Deployment;
use kube::Client;
use kube::api::Api;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::debug;

pub async fn until<F, Fut, T>(label: &str, timeout: Duration, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let deadline = Instant::now() + timeout;
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match f().await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {
                debug!(%label, attempt, "condition not yet met");
            }
            Err(e) => {
                debug!(%label, attempt, error = %e, "poll error, will retry");
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for: {label}"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub async fn deployment_ready(client: Arc<Client>, ns: &str, name: &str, timeout: Duration) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced((*client).clone(), ns);
    until(&format!("deployment {ns}/{name} ready"), timeout, || async {
        let dep = api.get(name).await?;
        let spec_replicas = dep.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
        let ready = dep.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0);
        if ready >= spec_replicas { Ok(Some(())) } else { Ok(None) }
    })
    .await
}

pub async fn job_succeeded(client: Arc<Client>, ns: &str, name: &str, timeout: Duration) -> Result<()> {
    use k8s_openapi::api::batch::v1::Job;
    let api: Api<Job> = Api::namespaced((*client).clone(), ns);
    until(&format!("job {ns}/{name} succeeded"), timeout, || async {
        let job = api.get(name).await?;
        let status = job.status.unwrap_or_default();
        if status.succeeded.unwrap_or(0) >= 1 {
            Ok(Some(()))
        } else if status.failed.unwrap_or(0) >= 1 {
            Err(anyhow!("job {ns}/{name} failed (status.failed >= 1)"))
        } else {
            Ok(None)
        }
    })
    .await
}
