//! In-cluster `ConfigMap` adapter for [`mars_definition_source::DefinitionSource`].
//!
//! Resolves the `RenderDefinition` payload by reading a single key out of a
//! named `ConfigMap` in a given namespace, and surfaces changes by watching
//! that same object's `resourceVersion`. The reconciler treats every
//! [`Change`] as "re-fetch and re-compose" - the payload itself is not
//! carried on the stream.
//!
//! Construction takes a ready `kube::Client` (the operator owns client
//! lifecycle), the target `namespace`, `name`, and `key`. No RBAC concerns
//! here beyond the kube client's existing role bindings.

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::Api;
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self as kube_watcher, Config as WatcherConfig};
use mars_definition_source::{Change, DefinitionBytes, DefinitionSource, DefinitionSourceError};

/// Adapter that fetches a `RenderDefinition` from a named ConfigMap key.
#[derive(Clone)]
pub struct ConfigMapDefinitionSource {
    client: kube::Client,
    namespace: String,
    name: String,
    key: String,
}

impl ConfigMapDefinitionSource {
    /// Build from a ready kube client and the resolved `ConfigMapKeyRef` triple.
    pub fn new(client: kube::Client, namespace: String, name: String, key: String) -> Self {
        Self {
            client,
            namespace,
            name,
            key,
        }
    }

    fn api(&self) -> Api<ConfigMap> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn target_label(&self) -> String {
        format!("configmap {}/{}", self.namespace, self.name)
    }
}

#[async_trait]
impl DefinitionSource for ConfigMapDefinitionSource {
    async fn fetch(&self) -> Result<DefinitionBytes, DefinitionSourceError> {
        let cm = self
            .api()
            .get(&self.name)
            .await
            .map_err(|e| map_kube_error(e, &self.target_label()))?;

        let value = cm
            .data
            .as_ref()
            .and_then(|d| d.get(&self.key))
            .ok_or_else(|| DefinitionSourceError::NotFound {
                what: format!("{}: key {}", self.target_label(), self.key),
            })?;

        // safety belt - the apiserver always stamps resourceVersion on GET responses
        let revision = cm.metadata.resource_version.ok_or(DefinitionSourceError::Decode {
            what: "configmap missing resourceVersion",
            source: Box::new(MissingResourceVersion),
        })?;

        Ok(DefinitionBytes {
            data: Bytes::from(value.clone().into_bytes()),
            revision,
        })
    }

    fn watch(&self) -> BoxStream<'static, Change> {
        // field-selector-narrowed watch on the single named configmap; apiserver
        // supports metadata.name on ConfigMap. each subscription gets its own
        // dedup cell so revisions only emit once per change.
        let cfg = WatcherConfig::default().fields(&format!("metadata.name={}", self.name));
        let last_seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        kube_watcher::watcher(self.api(), cfg)
            .applied_objects()
            .filter_map(move |res| {
                let last_seen = Arc::clone(&last_seen);
                async move {
                    let cm = res.ok()?;
                    let rv = cm.metadata.resource_version?;
                    let mut guard = last_seen.lock().ok()?;
                    if guard.as_deref() == Some(rv.as_str()) {
                        return None;
                    }
                    *guard = Some(rv.clone());
                    Some(Change { revision: rv })
                }
            })
            .boxed()
    }
}

pub(crate) fn map_kube_error(e: kube::Error, target: &str) -> DefinitionSourceError {
    match e {
        kube::Error::Api(status) if status.code == 404 => DefinitionSourceError::NotFound {
            what: target.to_string(),
        },
        kube::Error::Api(status) if status.code == 401 || status.code == 403 => DefinitionSourceError::Auth {
            what: format!("{target}: {}", status.reason),
        },
        other => DefinitionSourceError::network("configmap get", other),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("configmap missing resourceVersion")]
struct MissingResourceVersion;

// adapter integration coverage (live apiserver) lives in the operator's
// e2e suite — kube::Client cannot be meaningfully exercised here without
// spinning a control plane.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
