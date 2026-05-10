//! core value types shared across MARS. pure data, no i/o, no async.

#![forbid(unsafe_code)]

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// current `Manifest::format_version`. Bumped to 4 when
/// `LevelMetadata.hilbert_range_table` started carrying the per-entry
/// `PageId` so dirty-page lookup stays correct after rebalance allocates
/// fresh ids. Readers reject anything other than this exact value (no
/// "accept `<= max`" — see `mars-store-fs` / `mars-store-s3` manifest
/// readers).
pub const MANIFEST_FORMAT_VERSION: u32 = 4;

/// upper bound on the on-disk pointer string. Versions are `v\d+` so 32 chars
/// (`v` + 31 decimal digits) covers anything `u64` can represent comfortably.
const MANIFEST_POINTER_MAX_LEN: usize = 32;

/// Validate a manifest pointer (`vN`, N a positive integer, capped length).
/// Both manifest-store adapters consume this so the contract for a "valid
/// pointer" lives in one place; lax acceptance (dotted names, very long
/// strings) is a footgun for the GC / rollover path.
pub fn validate_manifest_pointer(pointer: &str) -> Result<(), ManifestPointerError> {
    if pointer.is_empty() {
        return Err(ManifestPointerError::Empty);
    }
    if pointer.len() > MANIFEST_POINTER_MAX_LEN {
        return Err(ManifestPointerError::TooLong);
    }
    let Some(rest) = pointer.strip_prefix('v') else {
        return Err(ManifestPointerError::BadShape);
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ManifestPointerError::BadShape);
    }
    Ok(())
}

/// Reasons a manifest pointer string fails validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestPointerError {
    #[error("manifest pointer is empty")]
    Empty,
    #[error("manifest pointer exceeds {} chars", MANIFEST_POINTER_MAX_LEN)]
    TooLong,
    #[error(r#"manifest pointer must match `v\d+`"#)]
    BadShape,
}

/// inclusive bounding box in canonical CRS units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bbox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bbox {
    #[must_use]
    pub const fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    #[must_use]
    pub fn width(self) -> f64 {
        self.max_x - self.min_x
    }

    #[must_use]
    pub fn height(self) -> f64 {
        self.max_y - self.min_y
    }
}

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

/// Identifier of a single page within a `(binding, level)` slice. wider than
/// strictly needed (page counts top out around the low thousands) so that the
/// 16-char lower-hex form serialised into object keys is comfortably stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageId(pub u64);

impl PageId {
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for PageId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 16-hex chars, lower case. matches the `p{page_hex}` segment in keys.
        write!(f, "{:016x}", self.0)
    }
}

/// Decimation level of a binding's page artifacts. 0 = native fidelity;
/// higher numbers = coarser. u8 dwarfs the handful of levels actually used,
/// but keeps the on-disk representation stable next to other page metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecimationLevel(pub u8);

impl DecimationLevel {
    #[must_use]
    pub const fn new(level: u8) -> Self {
        Self(level)
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl core::fmt::Display for DecimationLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Hilbert curve key over a binding's `combined_bbox` extent (32-bit per axis,
/// packed into a u64). defines the spatial sort order of pages within a
/// `(binding, level)` slice; range tables on `LevelMetadata` store inclusive
/// `(lo, hi)` pairs of these for binary-search lookup at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HilbertKey(pub u64);

impl HilbertKey {
    #[must_use]
    pub const fn new(key: u64) -> Self {
        Self(key)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// smallest key on the curve. useful as a sentinel in range comparisons.
    #[must_use]
    pub const fn min() -> Self {
        Self(u64::MIN)
    }

    /// largest key on the curve. useful as a sentinel in range comparisons.
    #[must_use]
    pub const fn max() -> Self {
        Self(u64::MAX)
    }
}

impl core::fmt::Display for HilbertKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// addresses a single page within `(binding, level)`. the `object_key`
/// helper renders the page's on-disk key shape `bnd/{binding}/L{level}/
/// p{page_hex}/{hash}.mars`; consumers never assemble keys by hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PageKey {
    pub binding_id: BindingId,
    pub level: DecimationLevel,
    pub page_id: PageId,
}

impl PageKey {
    /// build the canonical object-store key for the page artifact identified
    /// by this `PageKey` and the given content hash.
    pub fn object_key(&self, hash: &ContentHash) -> Result<ArtifactKey, ArtifactKeyError> {
        let binding_s = self.binding_id.as_str();
        if !is_safe_segment(binding_s) {
            return Err(ArtifactKeyError::Malformed {
                key: format!("bnd/{binding_s}/..."),
            });
        }
        Ok(ArtifactKey::new(format!(
            "bnd/{binding_s}/L{lvl}/p{pid}/{hex}.mars",
            lvl = self.level.get(),
            pid = self.page_id,
            hex = hash.to_hex(),
        )))
    }
}

/// manifest-level summary of one page artifact within `(binding, level)`.
/// `pages` on the manifest is sorted by `(binding_id, level, hilbert_range.0)`
/// so that the slice-scan lookup at render time is a binary search plus a
/// bounded linear scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageEntry {
    pub key: PageKey,
    pub content_hash: ContentHash,
    pub spatial_bbox: Bbox,
    /// inclusive `(lo, hi)` Hilbert key range covered by this page.
    pub hilbert_range: (HilbertKey, HilbertKey),
    pub feature_count: u64,
    pub size_bytes: u64,
}

/// per-decimation-level metadata on a binding. `hilbert_range_table`
/// duplicates the page-level Hilbert ranges in level-local sort order so
/// change-feed events can resolve `HilbertKey -> page` via a single binary
/// search without scanning the global `pages` vector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelMetadata {
    pub level: DecimationLevel,
    pub vertex_tolerance_m: f64,
    pub geometry_min_size_m: f64,
    pub label_min_priority: u32,
    pub page_count: u32,
    pub combined_bbox: Bbox,
    /// per-page `(hilbert_lo, hilbert_hi, page_id)` sorted ascending by
    /// `hilbert_lo`; binary-searchable. `page_id` is carried alongside the
    /// range because rebalance allocates fresh page ids that no longer
    /// match the table position; consumers must read `page_id` directly
    /// rather than reconstructing it from the array index.
    pub hilbert_range_table: Vec<(HilbertKey, HilbertKey, PageId)>,
}

/// per-binding metadata. one entry per `(table_or_view, geometry_column,
/// attribute_set, native_crs)` tuple in config; multi-table joined sources
/// are explicitly unsupported in v1 and are rejected at config-validation
/// time by `mars-config`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingMetadata {
    pub binding_id: BindingId,
    pub source_table: String,
    pub native_crs: CrsCode,
    pub feature_count_total: u64,
    pub levels: Vec<LevelMetadata>,
    /// `(feature_id, hilbert_key)` sidecar pinned by the manifest commit.
    /// `None` when a binding runs in `REPLICA IDENTITY FULL` mode (old-row
    /// geometry comes from the change event itself, no sidecar needed).
    pub page_membership_sidecar: Option<ArtifactEntry>,
}

/// kind of per-layer page sidecar artifact. class sidecars carry
/// `ClassAssignment` + `StyleRefs`; label sidecars carry `LabelCandidates`.
/// stored separately so a style-only change rewrites class sidecars without
/// touching page artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayerSidecarKind {
    Class,
    Label,
}

impl LayerSidecarKind {
    /// object-store prefix segment for sidecars of this kind.
    #[must_use]
    pub const fn key_prefix(self) -> &'static str {
        match self {
            Self::Class => "cls",
            Self::Label => "lbl",
        }
    }
}

/// manifest-level summary of one per-layer page sidecar artifact.
/// `object_key` renders the canonical `cls/...` or `lbl/...` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerSidecarEntry {
    pub layer_id: LayerId,
    pub page_key: PageKey,
    pub content_hash: ContentHash,
    pub size_bytes: u64,
    pub kind: LayerSidecarKind,
}

impl LayerSidecarEntry {
    /// build the canonical object-store key for this sidecar artifact:
    /// `{cls|lbl}/{layer}/{binding}/L{level}/p{page_hex}/{hash}.mars`.
    pub fn object_key(&self) -> Result<ArtifactKey, ArtifactKeyError> {
        let prefix = self.kind.key_prefix();
        let layer_s = self.layer_id.as_str();
        let binding_s = self.page_key.binding_id.as_str();
        if !is_safe_segment(layer_s) || !is_safe_segment(binding_s) {
            return Err(ArtifactKeyError::Malformed {
                key: format!("{prefix}/{layer_s}/{binding_s}/..."),
            });
        }
        Ok(ArtifactKey::new(format!(
            "{prefix}/{layer_s}/{binding_s}/L{lvl}/p{pid}/{hex}.mars",
            lvl = self.page_key.level.get(),
            pid = self.page_key.page_id,
            hex = self.content_hash.to_hex(),
        )))
    }
}

/// 32-byte content hash (BLAKE3) used as physical artifact addressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    #[must_use]
    pub const fn zero() -> Self {
        Self([0u8; 32])
    }

    /// lowercase hex (64 chars). Matches the `{hash}.mars` segment in keys.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use core::fmt::Write;
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            // infallible: pre-allocated string
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

impl core::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// true when `s` is a non-empty, bounded, path-safe segment.
fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
        && s != "."
        && s != ".."
}

/// errors raised while building an [`ArtifactKey`] for a known on-disk shape.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArtifactKeyError {
    #[error("malformed artifact key '{key}'")]
    Malformed { key: String },
}

/// raster image format the renderer encodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// MIME type string for HTTP `Content-Type` headers.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

/// manifest v3 data-transfer object.
///
/// substrate is `(binding × decimation_level × page)`. each render-time page
/// lookup is a binary search of `pages` (sorted by `(binding_id, level,
/// hilbert_range.0)`) plus a bounded linear scan for spatial-bbox hits.
/// `format_version` is bumped on incompatible changes to this struct; v3
/// readers reject anything other than the current value (no floor, no
/// "accept `<= max`" — that contract was retired).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// On-disk format version of this manifest envelope. Exact-match only.
    pub format_version: u32,
    pub version: u64,
    pub service: String,
    /// publication wall-clock time. SystemTime to avoid pulling chrono into
    /// the workspace; serde encodes as `{ secs_since_epoch, nanos_since_epoch }`.
    pub created_at: SystemTime,
    pub bindings: Vec<BindingMetadata>,
    /// page entries sorted by `(binding_id, level, hilbert_range.0)`; a render
    /// request resolves `(binding_id, level)` first, binary-searches into the
    /// matching slice, then linear-scans for spatial-bbox intersection.
    pub pages: Vec<PageEntry>,
    pub class_sidecars: Vec<LayerSidecarEntry>,
    pub label_sidecars: Vec<LayerSidecarEntry>,
    pub style_artifact: Option<ArtifactEntry>,
    /// opaque source-side cursor (e.g. pgoutput LSN) at which this manifest's
    /// state was captured. snapshot compiles set this to `None`.
    pub source_version: Option<String>,
    /// monotonic counter cross-checked by readers as a sanity gate against
    /// out-of-order manifest pointer publishes.
    pub epoch: u64,
}

impl Manifest {
    /// build the smallest valid v3 manifest: zero bindings, zero pages, zero
    /// sidecars. used by stubs and tests; production paths populate the
    /// collections from compiler output.
    #[must_use]
    pub fn empty(version: u64, service: impl Into<String>) -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            version,
            service: service.into(),
            created_at: SystemTime::now(),
            bindings: Vec::new(),
            pages: Vec::new(),
            class_sidecars: Vec::new(),
            label_sidecars: Vec::new(),
            style_artifact: None,
            source_version: None,
            epoch: 0,
        }
    }
}

/// pointer to one object-store-resident artifact carrying ancillary data
/// (style bundle, page-membership sidecar). page artifacts have richer
/// metadata and live in `PageEntry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub key: ArtifactKey,
    pub hash: ContentHash,
    pub size_bytes: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn manifest_pointer_accepts_versioned() {
        assert!(validate_manifest_pointer("v1").is_ok());
        assert!(validate_manifest_pointer("v0").is_ok());
        assert!(validate_manifest_pointer("v9999999").is_ok());
    }

    #[test]
    fn manifest_pointer_rejects_garbage() {
        assert_eq!(validate_manifest_pointer(""), Err(ManifestPointerError::Empty));
        assert_eq!(validate_manifest_pointer("v"), Err(ManifestPointerError::BadShape));
        assert_eq!(validate_manifest_pointer("vAA"), Err(ManifestPointerError::BadShape));
        assert_eq!(validate_manifest_pointer("1"), Err(ManifestPointerError::BadShape));
        assert_eq!(validate_manifest_pointer("v1.0"), Err(ManifestPointerError::BadShape));
        assert_eq!(
            validate_manifest_pointer("../etc/passwd"),
            Err(ManifestPointerError::BadShape)
        );
        let big = format!("v{}", "1".repeat(MANIFEST_POINTER_MAX_LEN));
        assert_eq!(validate_manifest_pointer(&big), Err(ManifestPointerError::TooLong));
    }

    #[test]
    fn bbox_dimensions() {
        let b = Bbox::new(0.0, 0.0, 10.0, 5.0);
        assert!((b.width() - 10.0).abs() < f64::EPSILON);
        assert!((b.height() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn manifest_empty_roundtrip() {
        let m = Manifest::empty(1, "demo");
        assert_eq!(m.format_version, MANIFEST_FORMAT_VERSION);
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn manifest_roundtrip_populated() {
        let pk = PageKey {
            binding_id: BindingId::try_new("buildings").unwrap(),
            level: DecimationLevel::new(0),
            page_id: PageId::new(7),
        };
        let mut m = Manifest::empty(42, "demo");
        m.epoch = 1;
        m.bindings.push(BindingMetadata {
            binding_id: pk.binding_id.clone(),
            source_table: "public.buildings".to_owned(),
            native_crs: CrsCode::new("EPSG:25832"),
            feature_count_total: 100,
            levels: vec![],
            page_membership_sidecar: None,
        });
        m.pages.push(PageEntry {
            key: pk.clone(),
            content_hash: ContentHash::zero(),
            spatial_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            hilbert_range: (HilbertKey::min(), HilbertKey::max()),
            feature_count: 100,
            size_bytes: 4096,
        });
        m.class_sidecars.push(LayerSidecarEntry {
            layer_id: LayerId::new("buildings"),
            page_key: pk,
            content_hash: ContentHash::zero(),
            size_bytes: 256,
            kind: LayerSidecarKind::Class,
        });
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn manifest_rejects_missing_format_version() {
        // v3 has no serde defaults: a manifest body lacking `format_version`
        // is a hard parse error, not a silent legacy floor.
        let valid = serde_json::to_string(&Manifest::empty(1, "x")).unwrap();
        assert!(serde_json::from_str::<Manifest>(&valid).is_ok());

        // strip the format_version field from the canonical body and confirm
        // serde refuses to default it.
        let stripped: String = valid.replacen(&format!(r#""format_version":{MANIFEST_FORMAT_VERSION},"#), "", 1);
        assert!(
            serde_json::from_str::<Manifest>(&stripped).is_err(),
            "missing format_version must be a parse error"
        );
    }

    #[test]
    fn image_format_mime() {
        assert_eq!(ImageFormat::Png.mime(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
    }

    #[test]
    fn newtype_serde_is_transparent() {
        let l = LayerId::new("parcels");
        let s = serde_json::to_string(&l).unwrap();
        assert_eq!(s, "\"parcels\"");
        let back: LayerId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn content_hash_display_is_hex() {
        let h = ContentHash([0xab; 32]);
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(s, h.to_hex());
    }

    #[test]
    fn binding_id_try_new_accepts_safe_segments() {
        assert!(BindingId::try_new("buildings").is_ok());
        assert!(BindingId::try_new("parcels-2024").is_ok());
        assert!(BindingId::try_new("plots_v3").is_ok());
    }

    #[test]
    fn binding_id_try_new_rejects_unsafe() {
        assert!(BindingId::try_new("").is_err(), "empty");
        assert!(BindingId::try_new("foo/bar").is_err(), "slash");
        assert!(BindingId::try_new("foo\\bar").is_err(), "backslash");
        assert!(BindingId::try_new("a\0b").is_err(), "null");
        assert!(BindingId::try_new("..").is_err(), "dotdot");
        assert!(BindingId::try_new(".").is_err(), "dot");
        let big = "x".repeat(129);
        assert!(BindingId::try_new(big).is_err(), "too long");
    }

    #[test]
    fn binding_id_serde_is_transparent() {
        let id = BindingId::try_new("buildings").unwrap();
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "\"buildings\"");
        let back: BindingId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn page_id_display_is_zero_padded_hex() {
        assert_eq!(PageId::new(0).to_string(), "0000000000000000");
        assert_eq!(PageId::new(0xdead_beef).to_string(), "00000000deadbeef");
        assert_eq!(PageId::new(u64::MAX).to_string(), "ffffffffffffffff");
    }

    #[test]
    fn page_id_serde_is_transparent() {
        let p = PageId::new(42);
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, "42");
        let back: PageId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn decimation_level_ordering() {
        assert!(DecimationLevel::new(0) < DecimationLevel::new(1));
        assert!(DecimationLevel::new(4) > DecimationLevel::new(2));
        assert_eq!(DecimationLevel::new(3).get(), 3);
        assert_eq!(DecimationLevel::new(7).to_string(), "7");
    }

    #[test]
    fn hilbert_key_ordering_and_bounds() {
        assert!(HilbertKey::min() < HilbertKey::max());
        assert_eq!(HilbertKey::min().get(), 0);
        assert_eq!(HilbertKey::max().get(), u64::MAX);
        let k = HilbertKey::new(0xcafe_babe);
        assert_eq!(k.to_string(), "00000000cafebabe");
    }

    #[test]
    fn hilbert_key_serde_is_transparent() {
        let k = HilbertKey::new(1234);
        let s = serde_json::to_string(&k).unwrap();
        assert_eq!(s, "1234");
        let back: HilbertKey = serde_json::from_str(&s).unwrap();
        assert_eq!(back, k);
    }

    fn sample_page_key() -> PageKey {
        PageKey {
            binding_id: BindingId::try_new("buildings").unwrap(),
            level: DecimationLevel::new(2),
            page_id: PageId::new(0xdead_beef),
        }
    }

    #[test]
    fn page_key_object_key_shape() {
        let pk = sample_page_key();
        let hash = ContentHash([0xab; 32]);
        let key = pk.object_key(&hash).unwrap();
        assert_eq!(
            key.as_str(),
            "bnd/buildings/L2/p00000000deadbeef/abababababababababababababababababababababababababababababababab.mars"
        );
    }

    #[test]
    fn page_key_object_key_rejects_unsafe_binding() {
        // BindingId::try_new is the trust boundary; defense in depth here lets
        // us catch a hand-constructed BindingId that bypassed validation.
        let pk = PageKey {
            binding_id: BindingId::new("foo/bar"),
            level: DecimationLevel::new(0),
            page_id: PageId::new(0),
        };
        let hash = ContentHash::zero();
        assert!(pk.object_key(&hash).is_err());
    }

    #[test]
    fn page_key_serde_roundtrip() {
        let pk = sample_page_key();
        let s = serde_json::to_string(&pk).unwrap();
        let back: PageKey = serde_json::from_str(&s).unwrap();
        assert_eq!(pk, back);
    }

    #[test]
    fn layer_sidecar_entry_object_key_shapes() {
        let pk = sample_page_key();
        let class = LayerSidecarEntry {
            layer_id: LayerId::new("roads"),
            page_key: pk.clone(),
            content_hash: ContentHash([0x11; 32]),
            size_bytes: 1024,
            kind: LayerSidecarKind::Class,
        };
        assert!(
            class
                .object_key()
                .unwrap()
                .as_str()
                .starts_with("cls/roads/buildings/L2/p00000000deadbeef/")
        );

        let label = LayerSidecarEntry {
            kind: LayerSidecarKind::Label,
            ..class
        };
        assert!(
            label
                .object_key()
                .unwrap()
                .as_str()
                .starts_with("lbl/roads/buildings/L2/p00000000deadbeef/")
        );
    }

    #[test]
    fn layer_sidecar_entry_rejects_unsafe_segments() {
        let pk = sample_page_key();
        let bad = LayerSidecarEntry {
            layer_id: LayerId::new(".."),
            page_key: pk,
            content_hash: ContentHash::zero(),
            size_bytes: 0,
            kind: LayerSidecarKind::Class,
        };
        assert!(bad.object_key().is_err());
    }

    #[test]
    fn page_entry_serde_roundtrip() {
        let pe = PageEntry {
            key: sample_page_key(),
            content_hash: ContentHash([0x33; 32]),
            spatial_bbox: Bbox::new(0.0, 0.0, 100.0, 100.0),
            hilbert_range: (HilbertKey::new(1), HilbertKey::new(2_000)),
            feature_count: 40_000,
            size_bytes: 5 * 1024 * 1024,
        };
        let s = serde_json::to_string(&pe).unwrap();
        let back: PageEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(pe, back);
    }

    #[test]
    fn level_metadata_serde_roundtrip() {
        let lm = LevelMetadata {
            level: DecimationLevel::new(1),
            vertex_tolerance_m: 0.5,
            geometry_min_size_m: 2.0,
            label_min_priority: 5,
            page_count: 12,
            combined_bbox: Bbox::new(-10.0, -10.0, 10.0, 10.0),
            hilbert_range_table: vec![
                (HilbertKey::new(0), HilbertKey::new(100), PageId::new(0)),
                (HilbertKey::new(101), HilbertKey::new(500), PageId::new(1)),
            ],
        };
        let s = serde_json::to_string(&lm).unwrap();
        let back: LevelMetadata = serde_json::from_str(&s).unwrap();
        assert_eq!(lm, back);
    }

    #[test]
    fn binding_metadata_serde_roundtrip() {
        let bm = BindingMetadata {
            binding_id: BindingId::try_new("buildings").unwrap(),
            source_table: "public.buildings".to_owned(),
            native_crs: CrsCode::new("EPSG:25832"),
            feature_count_total: 5_000_000,
            levels: vec![],
            page_membership_sidecar: None,
        };
        let s = serde_json::to_string(&bm).unwrap();
        let back: BindingMetadata = serde_json::from_str(&s).unwrap();
        assert_eq!(bm, back);
    }
}
