//! Bootstrap-related CR fields: postgres catalog bootstrap orchestration,
//! admin credential references, and teardown policy.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BootstrapSpec {
    /// When false, the operator skips Job-driven bootstrap and runs a read-
    /// only preflight against postgres with the runtime credential, gating
    /// the compiler/runtime children on the prerequisites already existing.
    #[serde(default = "super::defaults::default_true")]
    pub(crate) enabled: bool,

    /// Secret reference for the admin DSN as a single libpq URI string
    /// (CREATE ROLE / CREATE PUBLICATION / pg_create_logical_replication_slot
    /// privileges). Mutually exclusive with `adminCredentialsRef`; exactly one
    /// must be set when `enabled` is true. Preferred shape for non-Kubernetes
    /// Postgres (RDS, bare metal) where the user controls the DSN end-to-end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) admin_secret_ref: Option<SecretKeyRef>,

    /// Component-style admin credentials. Each subfield names a key inside a
    /// single Secret so the operator can consume the multi-key Secret shape
    /// emitted by Postgres operators (CNPG, Zalando, Crunchy) without forcing
    /// the user to synthesise a DSN string. Mutually exclusive with
    /// `adminSecretRef`; exactly one must be set when `enabled` is true.
    /// Missing host/port/database keys fall back to the values parsed out of
    /// the bootstrap-bearing `spec.config.sources[].dsn` so a single
    /// config-level DSN can supply connection targeting while credentials
    /// come from the Postgres operator's Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) admin_credentials_ref: Option<AdminCredentialsRef>,

    /// Secret reference for the runtime role password. Optional when
    /// `enabled` is true: when omitted, the operator generates a random
    /// password on first reconcile and persists it in a Secret named
    /// `<msvc>-runtime-credentials` (key `password`) owned by the
    /// MarsService so deletion of the CR garbage-collects it. The resolved
    /// Secret is consumed by the bootstrap Job and projected as
    /// `MARS_RUNTIME_PASSWORD` into the compiler/runtime pods so user DSN
    /// templates can reference it directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_password_secret_ref: Option<SecretKeyRef>,

    /// What to drop on CR delete. Role removal defaults off so shared roles
    /// survive a service teardown.
    #[serde(default)]
    pub(crate) teardown_on_delete: TeardownPolicy,
}

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

    /// Override key for the host. When unset the operator falls back to the
    /// host parsed out of the bootstrap-bearing `spec.config.sources[].dsn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) host_key: Option<String>,

    /// Override key for the port. When unset the operator falls back to the
    /// port parsed out of the bootstrap-bearing `spec.config.sources[].dsn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) port_key: Option<String>,

    /// Override key for the database name. When unset the operator falls
    /// back to the dbname parsed out of the bootstrap-bearing
    /// `spec.config.sources[].dsn`.
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
