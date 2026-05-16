//! Error enum for the vector-file adapter.

use mars_source::SourceError;

/// Adapter-level error. Implements `Into<SourceError>` so the surface
/// trait keeps its port-level error type.
#[derive(Debug, thiserror::Error)]
pub enum VectorFileError {
    /// Synchronous I/O on the cache or filesystem.
    #[error("io: {what}: {source}")]
    Io {
        /// short label describing the operation.
        what: &'static str,
        /// underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// Object-store transport error (resolve/get/head).
    #[error("object_store: {what}: {source}")]
    ObjectStore {
        /// short label describing the operation.
        what: &'static str,
        /// underlying object_store error.
        #[source]
        source: object_store::Error,
    },
    /// Local cache integrity / layout error.
    #[error("cache: {0}")]
    Cache(String),
    /// Decoder error (FGB / GeoJSON parse, attribute coercion).
    #[error("decoder: {0}")]
    Decoder(#[from] DecoderError),
    /// Reprojection error from `mars-proj`.
    #[error("reproject: {0}")]
    Reproject(#[from] ReprojectError),
    /// URI scheme not supported by this adapter.
    #[error("unsupported scheme: {uri}")]
    UnsupportedScheme {
        /// the offending URI.
        uri: String,
    },
    /// Config rejected at construct time.
    #[error("invalid config: {what}")]
    InvalidConfig {
        /// human-readable cause.
        what: &'static str,
    },
    /// Stub variant for surfaces this adapter has not implemented yet.
    #[error("not implemented: {what}")]
    NotImplemented {
        /// operation name.
        what: &'static str,
    },
}

impl From<VectorFileError> for SourceError {
    fn from(e: VectorFileError) -> Self {
        match e {
            VectorFileError::UnsupportedScheme { uri } => {
                SourceError::InvalidBinding(format!("unsupported scheme: {uri}"))
            }
            VectorFileError::InvalidConfig { what } => SourceError::InvalidBinding(what.to_string()),
            VectorFileError::NotImplemented { what } => SourceError::NotImplemented { what },
            other => SourceError::backend("vectorfile", other),
        }
    }
}

/// Decoder-side error. Carried inside [`VectorFileError::Decoder`].
#[derive(Debug, thiserror::Error)]
pub enum DecoderError {
    /// Underlying parser error.
    #[error("{format}: {source}")]
    Parse {
        /// decoder identifier (e.g. `"flatgeobuf"`, `"geojson"`).
        format: &'static str,
        /// parser error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// Format does not contain the binding's geometry / id fields.
    #[error("{0}")]
    Schema(String),
    /// The binding requested a format this adapter does not register a
    /// decoder for.
    #[error("no decoder for format {0:?}")]
    NoDecoder(mars_config::VectorFileFormat),
}

/// Reprojection error. Carried inside [`VectorFileError::Reproject`].
#[derive(Debug, thiserror::Error)]
pub enum ReprojectError {
    /// proj initialisation or transform failed.
    #[error("proj: {0}")]
    Proj(#[from] mars_proj::ProjError),
    /// WKB walker failed to decode an input geometry.
    #[error("wkb: {0}")]
    Wkb(String),
}
