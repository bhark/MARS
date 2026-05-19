use serde::{Deserialize, Serialize};

/// Scale-band table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scales {
    /// Bands ordered fine-to-coarse.
    pub bands: Vec<Band>,
}

/// Single scale band entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Band {
    /// Band name.
    pub name: String,
    /// Exclusive upper bound on the scale denominator covered by this band:
    /// the threshold itself falls into the next band.
    #[serde(rename = "max_denom_exclusive")]
    pub max_denom: u64,
}
