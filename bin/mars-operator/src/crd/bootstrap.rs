//! Reusable secret-ref + teardown-policy types shared by the cluster-side
//! bootstrap reconciler. The per-service bootstrap path is gone; these stay
//! because `MarsServiceCluster.spec.sourcesCatalog[].bootstrap` uses the same
//! key bundle shape.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SecretKeyRef {
    pub(crate) name: String,
    pub(crate) key: String,
}

/// Component-style admin credentials reference. Defaults match the
/// `*-superuser` Secret CNPG emits, which is the most common shape in
/// K8s-native Postgres deployments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminCredentialsRef {
    /// Name of the Secret holding the component-style admin credentials.
    pub(crate) secret_name: String,

    #[serde(default = "default_username_key")]
    pub(crate) username_key: String,

    #[serde(default = "default_password_key")]
    pub(crate) password_key: String,

    /// Override key for the host. Falls back to host parsed from the source DSN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) host_key: Option<String>,

    /// Override key for the port. Falls back to port parsed from the source DSN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) port_key: Option<String>,

    /// Override key for the database name. Falls back to dbname parsed from the source DSN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) database_key: Option<String>,
}

fn default_username_key() -> String {
    "username".into()
}

fn default_password_key() -> String {
    "password".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeardownPolicy {
    #[serde(default = "super::defaults::default_true")]
    pub(crate) slot: bool,
    #[serde(default = "super::defaults::default_true")]
    pub(crate) publication: bool,
    #[serde(default)]
    pub(crate) role: bool,
}

impl Default for TeardownPolicy {
    fn default() -> Self {
        Self {
            slot: true,
            publication: true,
            role: false,
        }
    }
}
