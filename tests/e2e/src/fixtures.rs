//! fixture-loader orchestration. the SQL dump itself is fetched out-of-band
//! by `scripts/fetch-fixture.sh`; this module wires it into the cluster via:
//!   - a `fixture-sql` ConfigMap built from the canonical derive + assert +
//!     replication SQL files in `tests/integration/fixtures/e2e-osm/`
//!     (driver-side to keep the YAML template clean of multi-line
//!     interpolation)
//!   - a kind hostPath mount of `target/e2e-fixtures` declared in
//!     `tests/e2e/kind.yaml.tmpl` that exposes the dump on the node, consumed
//!     by the loader Job's hostPath volume.

use anyhow::{Context, Result, anyhow};
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{ObjectMeta, Patch, PatchParams};
use kube::{Api, Client};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tracing::info;

/// resolves the canonical fixture path on the host. errors clearly if the
/// dump is missing; the caller should surface a pointer to fetch-fixture.sh.
pub fn host_fixture_path() -> Result<PathBuf> {
    let env_path = std::env::var("MARS_E2E_FIXTURE_PATH").ok();
    let path = match env_path {
        Some(p) => PathBuf::from(p),
        None => {
            let repo = repo_root()?;
            repo.join("target/e2e-fixtures/osm-parity.sql.gz")
        }
    };
    if !path.exists() {
        return Err(anyhow!(
            "fixture dump not found at {} — run scripts/fetch-fixture.sh or set MARS_E2E_FIXTURE_PATH",
            path.display(),
        ));
    }
    Ok(path)
}

/// filename component of the host fixture path. the kind extraMount exposes
/// the directory; the loader Job mounts the file by name, so the basename
/// flows into fixture-loader.yaml.tmpl as a template variable.
pub fn fixture_filename(path: &std::path::Path) -> Result<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("fixture path {} has no filename", path.display()))
}

/// create the `fixture-sql` ConfigMap with the canonical derive + assert +
/// replication SQL files, plus the e2e-only synthetic-poi extension consumed
/// by the loader and the mutate-source script applied by b_incremental.
/// server-side apply so reruns inside the same namespace are idempotent.
pub async fn apply_sql_configmap(client: Arc<Client>, ns: &str) -> Result<()> {
    let repo = repo_root()?;
    let shared = repo.join("tests/integration/fixtures/e2e-osm");
    let e2e_sql = repo.join("tests/e2e/sql");
    let derive = fs::read_to_string(shared.join("derive-e2e.sql"))
        .await
        .with_context(|| format!("read {}/derive-e2e.sql", shared.display()))?;
    let assert = fs::read_to_string(shared.join("assert-fixture.sql"))
        .await
        .with_context(|| format!("read {}/assert-fixture.sql", shared.display()))?;
    let replication = fs::read_to_string(shared.join("create-replication.sql"))
        .await
        .with_context(|| format!("read {}/create-replication.sql", shared.display()))?;
    let synthetic_poi = fs::read_to_string(e2e_sql.join("synthetic-poi.sql"))
        .await
        .with_context(|| format!("read {}/synthetic-poi.sql", e2e_sql.display()))?;
    let synthetic_pattern = fs::read_to_string(e2e_sql.join("synthetic-pattern.sql"))
        .await
        .with_context(|| format!("read {}/synthetic-pattern.sql", e2e_sql.display()))?;
    let mutate_source = fs::read_to_string(e2e_sql.join("mutate-source.sql"))
        .await
        .with_context(|| format!("read {}/mutate-source.sql", e2e_sql.display()))?;

    let mut data = BTreeMap::new();
    data.insert("derive-e2e.sql".to_string(), derive);
    data.insert("assert-fixture.sql".to_string(), assert);
    data.insert("create-replication.sql".to_string(), replication);
    data.insert("synthetic-poi.sql".to_string(), synthetic_poi);
    data.insert("synthetic-pattern.sql".to_string(), synthetic_pattern);
    data.insert("mutate-source.sql".to_string(), mutate_source);

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some("fixture-sql".to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let api: Api<ConfigMap> = Api::namespaced((*client).clone(), ns);
    api.patch(
        "fixture-sql",
        &PatchParams::apply("mars-e2e-kind").force(),
        &Patch::Apply(&cm),
    )
    .await
    .with_context(|| format!("apply fixture-sql configmap in {ns}"))?;
    info!(namespace = %ns, "applied fixture-sql configmap");
    Ok(())
}

/// create the `mars-images` ConfigMap from every file under
/// `tests/e2e/images/`. operator-side, the MarsService spec
/// `compiler.imagesConfigMap: mars-images` mounts this read-only at
/// `/var/lib/mars/images`; the MARS config's `compiler.images_dir` resolves
/// the names from there during pack. server-side apply so reruns inside the
/// same namespace are idempotent.
pub async fn apply_images_configmap(client: Arc<Client>, ns: &str) -> Result<()> {
    let repo = repo_root()?;
    let dir = repo.join("tests/e2e/images");
    let mut entries = fs::read_dir(&dir)
        .await
        .with_context(|| format!("read {}", dir.display()))?;
    let mut binary = BTreeMap::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("image fixture {} has no filename", path.display()))?;
        let bytes = fs::read(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        binary.insert(name, ByteString(bytes));
    }
    if binary.is_empty() {
        return Err(anyhow!("no image fixtures found under {}", dir.display()));
    }

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some("mars-images".to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        binary_data: Some(binary),
        ..Default::default()
    };

    let api: Api<ConfigMap> = Api::namespaced((*client).clone(), ns);
    api.patch(
        "mars-images",
        &PatchParams::apply("mars-e2e-kind").force(),
        &Patch::Apply(&cm),
    )
    .await
    .with_context(|| format!("apply mars-images configmap in {ns}"))?;
    info!(namespace = %ns, "applied mars-images configmap");
    Ok(())
}

fn repo_root() -> Result<PathBuf> {
    // cargo runs tests with CWD == crate root; the kind-e2e crate lives at
    // <repo>/tests/e2e. walk up two levels.
    let cwd = std::env::current_dir().context("get cwd")?;
    let repo = cwd
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("cannot derive repo root from cwd {}", cwd.display()))?;
    Ok(repo.to_path_buf())
}
