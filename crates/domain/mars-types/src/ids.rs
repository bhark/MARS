//! string newtype primitives + the macro that declares them.

/// declares a transparent `Arc<str>` newtype with the standard accessor surface
/// (`new`, `as_str`), `Display`, `AsRef<str>`, `Borrow<str>`, `From<&str>`, and
/// serde transparent ser/de. clone is a refcount bump; hash/eq are content-based
/// (delegate to `str`), so swapping with the previous `String` repr is invisible
/// to `HashMap` consumers.
#[macro_export]
macro_rules! impl_string_newtype {
    ($(#[$meta:meta])* $vis:vis $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, ::serde::Serialize)]
        #[serde(transparent)]
        $vis struct $name(::std::sync::Arc<str>);

        impl $name {
            #[must_use]
            pub fn new(s: impl Into<::std::sync::Arc<str>>) -> Self {
                Self(s.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl ::core::convert::AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl ::core::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self::new(s)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                // String -> Arc<str> goes via Box<str>; one alloc, no copy beyond it
                Self(::std::sync::Arc::<str>::from(s))
            }
        }

        // manual Deserialize: read a String, hand off to Arc<str>. avoids
        // depending on serde's optional `rc` feature, whose semantics around
        // shared deserialization are not what we want here.
        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::core::result::Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                let s = String::deserialize(deserializer)?;
                Ok(Self(::std::sync::Arc::<str>::from(s)))
            }
        }
    };
}

impl_string_newtype!(
    /// CRS authority code, e.g. `EPSG:25832`. dedup axis.
    pub CrsCode
);

impl CrsCode {
    /// engine's supported raster source/plan CRS set. single source of truth
    /// for both `mars validate` (config-time) and the runtime raster path
    /// (request-time defense in depth). grow this when a second adapter CRS
    /// lands.
    pub const SUPPORTED_RASTER: &'static [&'static str] = &["EPSG:3857"];

    #[must_use]
    pub fn is_supported_raster(&self) -> bool {
        Self::SUPPORTED_RASTER.contains(&self.as_str())
    }
}

impl_string_newtype!(
    /// stable layer identifier inside a service. dedup axis.
    pub LayerId
);

impl_string_newtype!(
    /// object-store key for an artifact. dedup axis.
    pub ArtifactKey
);

impl_string_newtype!(
    /// per-request id, propagated end-to-end through tracing spans.
    pub RequestId
);

impl_string_newtype!(
    /// stable identifier for a source collection (logical name shared between
    /// the binding, change feed, and compiled artifact metadata).
    pub SourceCollectionId
);

impl_string_newtype!(
    /// stable identifier for a source binding (a `(table_or_view,
    /// geometry_column, attribute_set, native_crs)` tuple declared in config).
    /// appears in object-store keys, so segments must be path-safe; use
    /// [`BindingId::try_new`] at trust boundaries.
    pub BindingId
);

impl BindingId {
    /// validating constructor. rejects empty, oversized, slash-bearing,
    /// backslash-bearing, null-bearing, `.` and `..` ids before they can
    /// land in an object key.
    pub fn try_new(s: impl Into<::std::sync::Arc<str>>) -> Result<Self, BindingIdError> {
        let id = Self(s.into());
        if !is_safe_segment(id.as_str()) {
            return Err(BindingIdError::Malformed {
                id: id.as_str().to_owned(),
            });
        }
        Ok(id)
    }
}

/// Reasons a `BindingId` fails validation at the trust boundary.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BindingIdError {
    #[error("malformed binding id '{id}'")]
    Malformed { id: String },
}

/// errors raised while building an [`ArtifactKey`] for a known on-disk shape.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArtifactKeyError {
    #[error("malformed artifact key '{key}'")]
    Malformed { key: String },
}

/// true when `s` is a non-empty, bounded, path-safe segment.
pub(crate) fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
        && s != "."
        && s != ".."
}

#[cfg(test)]
mod tests;
