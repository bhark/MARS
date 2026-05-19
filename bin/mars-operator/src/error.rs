//! Error type used by the reconcile loop.

use crate::compose::ComposeError;
use crate::crd::spec::SpecValidationError;
use crate::definition::ResolveError;
use crate::poller::ManagerError;

#[derive(Debug, thiserror::Error)]
pub(crate) enum OperatorError {
    #[error("kube client error: {0}")]
    Kube(#[from] kube::Error),

    #[error("config validation failed: {0}")]
    ConfigInvalid(String),

    #[error("mars-config error: {0}")]
    MarsConfig(#[from] mars_config::ConfigError),

    #[error("yaml serialisation error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error("json serialisation error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("missing required field: {0}")]
    MissingField(String),

    #[error("spec admission failed: {0}")]
    SpecInvalid(#[from] SpecValidationError),

    #[error("MarsServiceCluster '{0}' not found")]
    ClusterNotFound(String),

    #[error("definition source: {0}")]
    DefinitionResolve(#[from] ResolveError),

    #[error("definition fetch: {0}")]
    DefinitionFetch(#[from] mars_definition_source::DefinitionSourceError),

    #[error("definition decode: {0}")]
    DefinitionDecode(String),

    #[error("compose: {0}")]
    Compose(#[from] ComposeError),
}

impl From<ManagerError> for OperatorError {
    fn from(e: ManagerError) -> Self {
        match e {
            ManagerError::Resolve(r) => OperatorError::DefinitionResolve(r),
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, OperatorError>;
