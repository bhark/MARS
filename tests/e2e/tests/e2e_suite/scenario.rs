//! shared scenario builder. each test calls `Scenario::up(prefix).await?` to
//! get a freshly provisioned namespace with seaweedfs + postgis + the fixture
//! loaded + a MarsService applied. drop = namespace teardown (unless
//! MARS_E2E_KEEP=1).

use anyhow::{Context, Result, anyhow};
use mars_e2e_kind::{cluster, deploy, fixtures, namespace::NamespaceGuard, wait};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct Scenario {
    pub(crate) client: Arc<kube::Client>,
    pub(crate) ns: NamespaceGuard,
}

/// knobs the per-scenario MarsService template is parameterised on.
#[derive(Debug, Clone)]
pub(crate) struct ScenarioOptions {
    pub(crate) runtime_replicas: u32,
}

impl Default for ScenarioOptions {
    fn default() -> Self {
        Self { runtime_replicas: 1 }
    }
}

impl Scenario {
    pub(crate) async fn up(prefix: &str) -> Result<Self> {
        Self::up_with(prefix, ScenarioOptions::default()).await
    }

    /// Variant of [`Scenario::up`] that exercises the operator's bootstrap
    /// Job: skips manual create-replication, applies a MarsService with
    /// `spec.bootstrap` set, and waits for the bootstrap Job + runtime to
    /// come up. Returns once `BootstrapReady=True` is observed.
    pub(crate) async fn up_with_bootstrap(prefix: &str) -> Result<Self> {
        let client = cluster::client().await?;
        let ns = NamespaceGuard::create(client.clone(), prefix).await?;
        let disc = deploy::discovery(client.clone()).await?;
        let mtmpl = manifests_dir();

        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("seaweedfs.yaml.tmpl"),
            &HashMap::new(),
        )
        .await
        .context("apply seaweedfs manifest")?;
        wait::deployment_ready(client.clone(), &ns.name, "seaweedfs", Duration::from_secs(120)).await?;
        wait::job_succeeded(
            client.clone(),
            &ns.name,
            "seaweedfs-bucket-init",
            Duration::from_secs(120),
        )
        .await?;

        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("postgis.yaml.tmpl"),
            &HashMap::new(),
        )
        .await
        .context("apply postgis manifest")?;
        wait::deployment_ready(client.clone(), &ns.name, "postgis", Duration::from_secs(120)).await?;

        let dump = fixtures::host_fixture_path()?;
        let dump_filename = fixtures::fixture_filename(&dump)?;
        fixtures::apply_sql_configmap(client.clone(), &ns.name).await?;
        fixtures::apply_images_configmap(client.clone(), &ns.name).await?;
        let mut loader_vars = HashMap::new();
        loader_vars.insert("FIXTURE_FILENAME", dump_filename.as_str());
        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("fixture-loader-no-replication.yaml.tmpl"),
            &loader_vars,
        )
        .await
        .context("apply fixture-loader-no-replication manifest")?;
        wait::job_succeeded(
            client.clone(),
            &ns.name,
            "fixture-loader-bootstrap",
            Duration::from_secs(600),
        )
        .await?;

        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("marsservice-bootstrap.yaml.tmpl"),
            &HashMap::new(),
        )
        .await
        .context("apply marsservice-bootstrap manifest")?;

        // wait for the operator-rendered bootstrap Job to succeed before the
        // runtime Deployment exists.
        wait::until(
            "bootstrap Job succeeded",
            Duration::from_secs(180),
            || {
                let client = client.clone();
                let ns = ns.name.clone();
                async move { bootstrap_job_succeeded(client, &ns).await }
            },
        )
        .await?;

        wait::deployment_ready(
            client.clone(),
            &ns.name,
            "mars-bs-runtime",
            Duration::from_secs(300),
        )
        .await?;

        Ok(Self { client, ns })
    }

    pub(crate) async fn up_with(prefix: &str, opts: ScenarioOptions) -> Result<Self> {
        let client = cluster::client().await?;
        let ns = NamespaceGuard::create(client.clone(), prefix).await?;

        let disc = deploy::discovery(client.clone()).await?;
        let mtmpl = manifests_dir();

        // seaweedfs first; the bundled bucket-init Job creates mars-artifacts
        // (mars-store-s3 never calls CreateBucket) and the Secret with static
        // AK/SK is part of the same manifest, so MarsService can come up next.
        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("seaweedfs.yaml.tmpl"),
            &HashMap::new(),
        )
        .await
        .context("apply seaweedfs manifest")?;
        wait::deployment_ready(client.clone(), &ns.name, "seaweedfs", Duration::from_secs(120)).await?;
        wait::job_succeeded(
            client.clone(),
            &ns.name,
            "seaweedfs-bucket-init",
            Duration::from_secs(120),
        )
        .await?;

        // postgis in parallel with the fixture loader's setup steps would be
        // ideal; for simplicity, serial.
        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("postgis.yaml.tmpl"),
            &HashMap::new(),
        )
        .await
        .context("apply postgis manifest")?;
        wait::deployment_ready(client.clone(), &ns.name, "postgis", Duration::from_secs(120)).await?;

        // fixture loader. the dump lives on the host at the path returned by
        // `host_fixture_path()` and is exposed to the node via the kind extra-
        // mount in tests/e2e/kind.yaml.tmpl; the loader Job consumes it through a
        // hostPath. the assert + replication SQL come from a driver-built
        // ConfigMap so the template stays free of multi-line interpolation.
        let dump = fixtures::host_fixture_path()?;
        let dump_filename = fixtures::fixture_filename(&dump)?;
        fixtures::apply_sql_configmap(client.clone(), &ns.name).await?;
        fixtures::apply_images_configmap(client.clone(), &ns.name).await?;
        let mut loader_vars = HashMap::new();
        loader_vars.insert("FIXTURE_FILENAME", dump_filename.as_str());
        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("fixture-loader.yaml.tmpl"),
            &loader_vars,
        )
        .await
        .context("apply fixture-loader manifest")?;
        wait::job_succeeded(client.clone(), &ns.name, "fixture-loader", Duration::from_secs(600)).await?;

        // MarsService — the operator (already running cluster-wide) reconciles
        // this into ConfigMap + PVCs + compiler/runtime Deployments + Service.
        // The operator owns image construction now; only runtime replicas
        // remain template-substituted.
        let runtime_replicas = opts.runtime_replicas.to_string();
        let mut vars = HashMap::new();
        vars.insert("RUNTIME_REPLICAS", runtime_replicas.as_str());
        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("marsservice.yaml.tmpl"),
            &vars,
        )
        .await
        .context("apply MarsService manifest")?;

        // wait for the operator-rendered runtime Deployment to have ready
        // replicas before returning. otherwise per-test `wait::until` loops
        // hit the api-server service proxy while the Service still has no
        // endpoints, producing a 503 ERROR log per poll. names follow the
        // child-builder convention `{svc}-{role}` (bin/mars-operator/src/
        // children/labels.rs).
        wait::deployment_ready(
            client.clone(),
            &ns.name,
            "mars-e2e-runtime",
            Duration::from_secs(300),
        )
        .await?;

        Ok(Self { client, ns })
    }
}

fn manifests_dir() -> PathBuf {
    // tests/e2e is the crate root at run-time; manifests are siblings of src/.
    std::env::current_dir()
        .ok()
        .map(|p| p.join("manifests"))
        .unwrap_or_else(|| PathBuf::from("manifests"))
}

// silence unused-anyhow import in narrow configurations.
#[allow(dead_code)]
fn _unused() -> Result<()> {
    Err(anyhow!("not used"))
}

/// Look up Job objects in `ns` matching the operator's bootstrap label
/// (`app.kubernetes.io/component=bootstrap`) and return Some(()) once at
/// least one has succeeded. Used by `up_with_bootstrap` to gate the rest
/// of the scenario on bootstrap completion.
async fn bootstrap_job_succeeded(
    client: std::sync::Arc<kube::Client>,
    ns: &str,
) -> Result<Option<()>> {
    use k8s_openapi::api::batch::v1::Job;
    use kube::api::{Api, ListParams};
    let api: Api<Job> = Api::namespaced(client.as_ref().clone(), ns);
    let lp = ListParams::default().labels("app.kubernetes.io/component=bootstrap");
    let jobs = api.list(&lp).await.context("list bootstrap jobs")?;
    for j in jobs.items {
        if j.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) >= 1 {
            return Ok(Some(()));
        }
    }
    Ok(None)
}
