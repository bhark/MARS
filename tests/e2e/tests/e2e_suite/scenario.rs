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

impl Scenario {
    pub(crate) async fn up(prefix: &str) -> Result<Self> {
        let client = cluster::client().await?;
        let ns = NamespaceGuard::create(client.clone(), prefix).await?;
        let mars_image_repo =
            std::env::var("MARS_E2E_IMAGE_REPOSITORY").unwrap_or_else(|_| "localhost/mars".to_string());
        let mars_image_tag = std::env::var("MARS_E2E_IMAGE_TAG").unwrap_or_else(|_| "e2e".to_string());

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
        let mut vars = HashMap::new();
        vars.insert("IMAGE_REPOSITORY", mars_image_repo.as_str());
        vars.insert("IMAGE_TAG", mars_image_tag.as_str());
        deploy::apply_template(
            client.clone(),
            &disc,
            &ns.name,
            mtmpl.join("marsservice.yaml.tmpl"),
            &vars,
        )
        .await
        .context("apply MarsService manifest")?;

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
