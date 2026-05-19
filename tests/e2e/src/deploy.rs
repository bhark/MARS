//! applies hand-rolled YAML manifests via Server-Side Apply. templating is
//! intentionally trivial: a string-replacement pass over `${VAR}` placeholders.
//! we avoid pulling in handlebars/tera for two placeholders; if this list grows
//! past a handful, swap it for a real templating crate.

use anyhow::{Context, Result, anyhow};
use kube::api::{Patch, PatchParams};
use kube::core::{DynamicObject, GroupVersionKind, TypeMeta};
use kube::discovery::Discovery;
use kube::{Api, Client};
use serde::Deserialize;
use serde_yaml_ng as yaml;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tracing::info;

const FIELD_MANAGER: &str = "mars-e2e-kind";

/// substitute `{{KEY}}` placeholders in `template` and parse one-or-many yaml
/// documents. uses double braces to avoid colliding with mars-config's
/// `${VAR}` env-substitution syntax which appears in MarsServiceCluster
/// source DSNs and the inline RenderDefinition payload.
pub fn render(template: &str, vars: &HashMap<&str, &str>) -> Result<Vec<yaml::Value>> {
    let mut rendered = template.to_string();
    for (k, v) in vars {
        rendered = rendered.replace(&format!("{{{{{k}}}}}"), v);
    }
    if let Some(start) = rendered.find("{{") {
        let end = rendered[start..].find("}}").unwrap_or(rendered.len() - start);
        return Err(anyhow!("unfilled placeholder: {}", &rendered[start..start + end + 2]));
    }
    let mut docs = Vec::new();
    for doc in serde_yaml_ng::Deserializer::from_str(&rendered) {
        let v = yaml::Value::deserialize(doc).context("parse rendered yaml")?;
        if !v.is_null() {
            docs.push(v);
        }
    }
    Ok(docs)
}

/// apply each manifest document via server-side apply into the given namespace.
/// objects without an explicit namespace are scoped to `ns` if they're namespaced.
pub async fn apply_all(client: Arc<Client>, discovery: &Discovery, ns: &str, docs: Vec<yaml::Value>) -> Result<()> {
    for doc in docs {
        apply_one(client.clone(), discovery, ns, doc).await?;
    }
    Ok(())
}

async fn apply_one(client: Arc<Client>, discovery: &Discovery, ns: &str, doc: yaml::Value) -> Result<()> {
    let mut obj: DynamicObject = yaml::from_value(doc).context("deserialize manifest as dynamic object")?;
    let tm = obj
        .types
        .clone()
        .ok_or_else(|| anyhow!("manifest missing apiVersion/kind"))?;
    let (ar, caps) = resolve(discovery, &tm)?;
    if caps.scope == kube::discovery::Scope::Namespaced && obj.metadata.namespace.is_none() {
        obj.metadata.namespace = Some(ns.to_string());
    }
    let name = obj
        .metadata
        .name
        .clone()
        .ok_or_else(|| anyhow!("manifest missing metadata.name (kind={})", tm.kind))?;
    let api: Api<DynamicObject> = if caps.scope == kube::discovery::Scope::Namespaced {
        Api::namespaced_with((*client).clone(), ns, &ar)
    } else {
        Api::all_with((*client).clone(), &ar)
    };
    let patch = Patch::Apply(&obj);
    api.patch(&name, &PatchParams::apply(FIELD_MANAGER).force(), &patch)
        .await
        .with_context(|| format!("apply {}/{name} in {ns}", tm.kind))?;
    info!(kind = %tm.kind, %name, namespace = %ns, "applied");
    Ok(())
}

fn resolve(
    discovery: &Discovery,
    tm: &TypeMeta,
) -> Result<(kube::discovery::ApiResource, kube::discovery::ApiCapabilities)> {
    let gvk = GroupVersionKind::try_from(tm).context("parse apiVersion/kind")?;
    let (ar, caps) = discovery
        .resolve_gvk(&gvk)
        .ok_or_else(|| anyhow!("unknown gvk: {}/{}/{}", gvk.group, gvk.version, gvk.kind))?;
    Ok((ar, caps))
}

/// read a manifest template file and render+apply it.
pub async fn apply_template(
    client: Arc<Client>,
    discovery: &Discovery,
    ns: &str,
    path: impl AsRef<Path>,
    vars: &HashMap<&str, &str>,
) -> Result<()> {
    let path = path.as_ref();
    let body = fs::read_to_string(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let docs = render(&body, vars)?;
    apply_all(client, discovery, ns, docs).await
}

pub async fn discovery(client: Arc<Client>) -> Result<Discovery> {
    Discovery::new((*client).clone()).run().await.context("api discovery")
}
