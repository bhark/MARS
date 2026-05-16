//! OWS Common metadata shared across capabilities-emitting protocols
//! (WMS + WMTS today; WCS / WFS later). Anything WMS-only lives on
//! [`super::ServiceWms`] instead.

use serde::{Deserialize, Serialize};

/// Service-level OWS metadata. Surfaced into both WMS and WMTS capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceOws {
    /// Service-level keywords surfaced in WMS `<KeywordList>` and WMTS
    /// `ows:Keywords`. Empty = element omitted.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Public URL used as `OnlineResource` href on the service block and
    /// per-operation `DCPType/HTTP/Get`. None = element omitted.
    #[serde(default)]
    pub online_resource: Option<String>,
    /// Free-text fees clause, MapServer `ows_fees`. None = element omitted.
    #[serde(default)]
    pub fees: Option<String>,
    /// Free-text access-constraints clause, MapServer `ows_accessconstraints`.
    #[serde(default)]
    pub access_constraints: Option<String>,
    /// XML processing-instruction `encoding="..."`. None = "UTF-8".
    #[serde(default)]
    pub encoding: Option<String>,
}

impl ServiceOws {
    /// XML encoding to emit in the capabilities document declaration.
    /// Defaults to UTF-8 when unset.
    #[must_use]
    pub fn xml_encoding(&self) -> &str {
        self.encoding.as_deref().unwrap_or("UTF-8")
    }
}
