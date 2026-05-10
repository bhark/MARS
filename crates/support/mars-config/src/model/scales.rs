use std::collections::BTreeMap;

use mars_types::Bbox;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::units;

/// Scale-band table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scales {
    /// Bands ordered fine-to-coarse.
    pub bands: Vec<Band>,
}

/// Single scale band entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Band {
    /// Band name (referenced from `cells.size_per_band`).
    pub name: String,
    /// Exclusive upper bound on the scale denominator covered by this band:
    /// the threshold itself falls into the next band.
    #[serde(rename = "max_denom_exclusive")]
    pub max_denom: u64,
}

/// Cell grid configuration. **Deprecated:** retained only for backward
/// compatibility with earlier fixtures. The page-keyed substrate does not
/// consume any of these fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cells {
    /// Grid kind. Ignored.
    #[serde(default)]
    pub grid: String,
    /// Origin in the canonical CRS. Ignored.
    #[serde(default)]
    pub origin: [f64; 2],
    /// Per-band cell size (unit-suffixed metres). Ignored.
    #[serde(default)]
    pub size_per_band: BTreeMap<String, String>,
    /// Service-wide extent in canonical CRS units. Ignored.
    #[serde(default)]
    pub extent: Option<Bbox>,
}

impl Cells {
    /// Resolve `size_per_band` values to metres.
    pub fn size_per_band_m(&self) -> Result<BTreeMap<String, f64>, ConfigError> {
        self.size_per_band
            .iter()
            .map(|(k, v)| units::parse_distance_m(v).map(|d| (k.clone(), d)))
            .collect()
    }
}
