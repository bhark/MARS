use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::units;

/// Artifact storage configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifacts {
    /// Long-term artifact store.
    pub store: ArtifactStore,
    /// Local on-disk cache.
    pub cache: ArtifactCache,
}

/// Long-term artifact store config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtifactStore {
    /// Store kind (`s3`, `fs`, ...).
    #[serde(rename = "type")]
    pub kind: String,
    /// Endpoint URL for object stores.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Bucket name for object stores.
    #[serde(default)]
    pub bucket: Option<String>,
    /// Key prefix for object stores.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Filesystem path for `type: fs`.
    #[serde(default)]
    pub path: Option<String>,
    /// Permit plaintext (non-TLS) `http://` endpoints for object stores. Off
    /// by default; required to allow `http://` so a typo in production cannot
    /// silently drop TLS. Useful for local minio/moto fixtures only.
    #[serde(default)]
    pub allow_http: bool,
    /// When true, fall back to non-atomic manifest publish if the backend
    /// does not support conditional put. Defaults to false (fail loudly).
    #[serde(default)]
    pub allow_non_atomic_publish: bool,
    /// Override conditional-put behaviour in the object_store S3 client.
    /// `etag` (default) uses If-Match / If-None-Match. `disabled` turns
    /// conditional puts off entirely, which is needed for backends such as
    /// Garage that do not enforce these headers.
    #[serde(default)]
    pub conditional_put: Option<String>,
}

/// Local artifact cache config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactCache {
    /// Cache directory.
    pub path: String,
    /// Max disk size as a unit-suffixed literal (`50GiB`).
    pub max_size: String,
    /// Eviction policy.
    #[serde(default = "default_eviction")]
    pub eviction: String,
    /// When true, the cache treats the content-hashed key path as authority
    /// and verifies each artifact only once per process via BLAKE3. Cuts
    /// hot-path cost on hits at the price of skipping bit-rot detection
    /// after the first verification.
    ///
    /// Default: true. Cache writes are atomic and content-addressed, so a
    /// per-hit rehash is safety theatre against bit-rot. Operators concerned
    /// about silent disk corruption can flip this off.
    #[serde(default = "default_trust_path_hash")]
    pub trust_path_hash: bool,
}

fn default_eviction() -> String {
    "lru".to_string()
}

fn default_trust_path_hash() -> bool {
    true
}

impl ArtifactCache {
    /// Resolve `max_size` to bytes.
    pub fn max_size_bytes(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.max_size)
    }
}
