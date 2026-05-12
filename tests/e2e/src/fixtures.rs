//! fixture-loader orchestration. the SQL dump itself is fetched out-of-band
//! by `scripts/fetch-fixture.sh`; this module just wires it into the cluster
//! via a ConfigMap (the loader script) + a Job that mounts the dump from a
//! kind hostPath mount.

use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;

/// resolves the canonical fixture path on the host. errors clearly if the
/// dump is missing; the caller should surface a pointer to fetch-fixture.sh.
pub fn host_fixture_path() -> Result<PathBuf> {
    let env_path = std::env::var("MARS_E2E_FIXTURE_PATH").ok();
    let path = match env_path {
        Some(p) => PathBuf::from(p),
        None => {
            let repo = repo_root()?;
            repo.join("target/e2e-fixtures/local-map-subset.sql.gz")
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
