//! XYZ-template HTTP raster source adapter.
//!
//! Implements [`mars_source::RasterSource`] against any HTTP endpoint whose
//! tiles are addressable via `{z}/{x}/{y}` URL templating (slippy-map
//! convention). The `RasterBinding.locator` string is the template; substitution
//! is plain text replacement, not RFC-6570. Supported response content types
//! are `image/png` and `image/jpeg`; anything else surfaces as a typed
//! [`mars_source::SourceError::Backend`].
//!
//! Intentionally minimal: no per-source cache, no retry policy, no concurrency
//! cap. The runtime caps fan-out at the call site via
//! `config.render.page_fetch_concurrency`.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use mars_source::{RasterBinding, RasterSource, SourceError, TileBytes};

/// HTTP XYZ raster source. One instance can serve every raster collection
/// the bin registers, since the underlying `reqwest::Client` already pools
/// connections per host.
#[derive(Debug, Clone)]
pub struct XyzRasterSource {
    client: reqwest::Client,
}

impl XyzRasterSource {
    /// Construct with a caller-supplied [`reqwest::Client`]. The bin owns
    /// client configuration (timeouts, proxy, TLS roots); the adapter does
    /// not impose defaults.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RasterSource for XyzRasterSource {
    async fn read_tile(&self, binding: &RasterBinding, z: u32, x: u32, y: u32) -> Result<TileBytes, SourceError> {
        let url = substitute_locator(&binding.locator, z, x, y)?;
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| SourceError::backend("xyz.tile.http_send", e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(SourceError::backend_msg(
                "xyz.tile.http_status",
                format!("upstream returned status {status} for {url}"),
            ));
        }
        let content_type = classify_content_type(&response)?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| SourceError::backend("xyz.tile.http_body", e))?;
        Ok(TileBytes { bytes, content_type })
    }
}

/// Resolve `{z}/{x}/{y}` placeholders in the locator template. Requires all
/// three placeholders to be present; bindings missing any of them are
/// rejected as configuration errors so silent fan-out into a constant URL
/// can never happen.
fn substitute_locator(template: &str, z: u32, x: u32, y: u32) -> Result<String, SourceError> {
    for token in ["{z}", "{x}", "{y}"] {
        if !template.contains(token) {
            return Err(SourceError::InvalidBinding(format!(
                "xyz locator missing required placeholder {token}: {template:?}"
            )));
        }
    }
    Ok(template
        .replace("{z}", &z.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string()))
}

/// Map a successful HTTP response's `Content-Type` to the small set of
/// statically-known media types the port can carry. Unknown / missing types
/// fail closed.
fn classify_content_type(response: &reqwest::Response) -> Result<&'static str, SourceError> {
    let raw = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| SourceError::backend_msg("xyz.tile.content_type", "missing or non-ascii Content-Type header"))?;
    // strip parameters: "image/png; charset=binary" -> "image/png"
    let main = raw.split(';').next().unwrap_or("").trim();
    match main {
        "image/png" => Ok("image/png"),
        "image/jpeg" | "image/jpg" => Ok("image/jpeg"),
        other => Err(SourceError::backend_msg(
            "xyz.tile.content_type",
            format!("unsupported tile content type {other:?}"),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn substitute_locator_replaces_all_three_placeholders() {
        let s = substitute_locator("https://t/{z}/{x}/{y}.png", 6, 33, 22).unwrap();
        assert_eq!(s, "https://t/6/33/22.png");
    }

    #[test]
    fn substitute_locator_preserves_query_strings() {
        let s = substitute_locator("https://t/{z}/{x}/{y}.png?key=abc", 0, 1, 2).unwrap();
        assert_eq!(s, "https://t/0/1/2.png?key=abc");
    }

    #[test]
    fn substitute_locator_rejects_missing_placeholder() {
        let err = substitute_locator("https://t/{z}/{x}.png", 0, 0, 0).expect_err("missing {y}");
        assert!(matches!(err, SourceError::InvalidBinding(ref m) if m.contains("{y}")));
    }

    /// Asserts the adapter remains thread-safe / object-safe per the port
    /// contract (`Send + Sync + 'static`). Sole compile-time test, no runtime
    /// assertion needed beyond the trait-object construction.
    #[test]
    fn xyz_raster_source_implements_raster_source_trait_object() {
        fn assert_obj<T: RasterSource + ?Sized>(_: &T) {}
        let src = XyzRasterSource::new(reqwest::Client::new());
        let boxed: Box<dyn RasterSource> = Box::new(src);
        assert_obj(&*boxed);
    }
}
