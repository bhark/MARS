//! Artifact-store PVC fields. Only consulted when `spec.config.artifacts.store.type == "fs"`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactStoreSpec {
    #[serde(default = "default_artifact_size")]
    pub(crate) size: String,
    #[serde(default)]
    pub(crate) storage_class: String,
    #[serde(default = "default_access_modes")]
    pub(crate) access_modes: Vec<String>,
}

impl Default for ArtifactStoreSpec {
    fn default() -> Self {
        Self {
            size: default_artifact_size(),
            storage_class: String::new(),
            access_modes: default_access_modes(),
        }
    }
}

fn default_artifact_size() -> String {
    "5Gi".into()
}

fn default_access_modes() -> Vec<String> {
    vec!["ReadWriteOnce".into()]
}
