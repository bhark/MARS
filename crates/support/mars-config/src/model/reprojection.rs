use mars_types::CrsCode;
use serde::{Deserialize, Serialize};

/// Reprojection allowlist.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Reprojection {
    /// Allowed CRS authority codes.
    #[serde(default)]
    pub allowlist: Vec<CrsCode>,
}
