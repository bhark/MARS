//! Error type used by the reconcile loop.

#[derive(Debug, thiserror::Error)]
pub(crate) enum OperatorError {
    #[error("kube client error")]
    Kube(#[from] kube::Error),

    #[error("config validation failed: {0}")]
    ConfigInvalid(String),

    #[error("mars-config error")]
    MarsConfig(#[from] mars_config::ConfigError),

    #[error("yaml serialisation error")]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error("json serialisation error")]
    Json(#[from] serde_json::Error),

    #[error("missing required field: {0}")]
    MissingField(String),
}

pub(crate) type Result<T> = std::result::Result<T, OperatorError>;
