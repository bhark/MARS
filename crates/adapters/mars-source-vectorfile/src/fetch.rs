//! Object-store-backed bytes fetcher with disk-cache integration.
//!
//! Resolves an `Arc<dyn ObjectStore>` per `(scheme, authority)` from the
//! URI, caches it across calls, HEADs the object to learn its etag,
//! consults the disk cache, and pulls the body only on miss.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::client::ClientOptions;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::http::HttpBuilder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as OsPath;
use object_store::{ObjectStore, ObjectStoreExt, ObjectStoreScheme};
use tokio::sync::RwLock;

use crate::cache::DiskCache;
use crate::error::VectorFileError;

/// Per-`(scheme, authority)` ObjectStore cache. Resolves a new backend
/// on first use of a host and reuses it for subsequent fetches.
pub struct Fetcher {
    stores: RwLock<HashMap<StoreKey, Arc<dyn ObjectStore>>>,
    allow_http: bool,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher")
            .field("allow_http", &self.allow_http)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StoreKey {
    scheme: String,
    authority: String,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Fetcher {
    /// Construct an empty fetcher. `allow_http` controls whether plain
    /// `http://` URIs are routed through the HTTP backend; default-false is
    /// the secure-by-default posture (object_store rejects http otherwise).
    #[must_use]
    pub fn new(allow_http: bool) -> Self {
        Self {
            stores: RwLock::new(HashMap::new()),
            allow_http,
        }
    }

    /// Pull `uri` through the cache. Returns the cached bytes on hit;
    /// fetches+caches+returns on miss.
    pub async fn fetch_cached(&self, uri: &str, cache: &DiskCache) -> Result<Bytes, VectorFileError> {
        let parsed = ParsedUri::parse(uri)?;
        let store = self.resolve(&parsed).await?;
        let path = OsPath::from(parsed.object_path.as_str());

        // probe etag first so a stable object can be served entirely from
        // disk. backends without etag round-trip as "unknown"; we still
        // cache under that synthetic key.
        let head = store.head(&path).await.map_err(|e| VectorFileError::ObjectStore {
            what: "head",
            source: e,
        })?;
        let etag = head.e_tag.clone().unwrap_or_else(|| "unknown".to_string());

        if let Some(hit) = cache.get(uri, &etag).await? {
            return Ok(hit);
        }

        let resp = store
            .get(&path)
            .await
            .map_err(|e| VectorFileError::ObjectStore { what: "get", source: e })?;
        let bytes = resp.bytes().await.map_err(|e| VectorFileError::ObjectStore {
            what: "get_bytes",
            source: e,
        })?;
        cache.put(uri, &etag, &bytes).await?;
        Ok(bytes)
    }

    /// HEAD the URI and return its current etag. Used by the change feed.
    pub async fn head_etag(&self, uri: &str) -> Result<Option<String>, VectorFileError> {
        let parsed = ParsedUri::parse(uri)?;
        let store = self.resolve(&parsed).await?;
        let path = OsPath::from(parsed.object_path.as_str());
        let head = store.head(&path).await.map_err(|e| VectorFileError::ObjectStore {
            what: "head",
            source: e,
        })?;
        Ok(head.e_tag)
    }

    async fn resolve(&self, parsed: &ParsedUri) -> Result<Arc<dyn ObjectStore>, VectorFileError> {
        let key = StoreKey {
            scheme: parsed.scheme.clone(),
            authority: parsed.authority.clone(),
        };
        if let Some(s) = self.stores.read().await.get(&key) {
            return Ok(s.clone());
        }
        let store = build_store(parsed, self.allow_http)?;
        let mut w = self.stores.write().await;
        // re-check under write lock in case a peer raced us.
        if let Some(existing) = w.get(&key) {
            return Ok(existing.clone());
        }
        w.insert(key, store.clone());
        Ok(store)
    }
}

/// Parsed URI: scheme, authority (bucket/host), and the object path
/// within that backend.
#[derive(Debug, Clone)]
pub(crate) struct ParsedUri {
    pub scheme: String,
    pub authority: String,
    pub object_path: String,
}

impl ParsedUri {
    fn parse(uri: &str) -> Result<Self, VectorFileError> {
        let (scheme, rest) = uri
            .split_once("://")
            .ok_or_else(|| VectorFileError::UnsupportedScheme { uri: uri.to_string() })?;
        let scheme = scheme.to_ascii_lowercase();
        match scheme.as_str() {
            "s3" | "gs" => {
                // <bucket>/<key>
                let (bucket, key) = rest
                    .split_once('/')
                    .ok_or_else(|| VectorFileError::UnsupportedScheme { uri: uri.to_string() })?;
                if bucket.is_empty() || key.is_empty() {
                    return Err(VectorFileError::UnsupportedScheme { uri: uri.to_string() });
                }
                Ok(Self {
                    scheme,
                    authority: bucket.to_string(),
                    object_path: key.to_string(),
                })
            }
            "file" => {
                // file://<absolute_path>. LocalFileSystem is rooted at "/",
                // so the path inside object_store is `rest` with any leading
                // slash preserved (object_store strips a single leading slash
                // internally).
                let path = if let Some(stripped) = rest.strip_prefix('/') {
                    stripped.to_string()
                } else {
                    rest.to_string()
                };
                Ok(Self {
                    scheme,
                    authority: String::new(),
                    object_path: path,
                })
            }
            "http" | "https" => {
                // authority is everything up to the first '/' after scheme;
                // the rest is the path within that origin.
                let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
                if host.is_empty() {
                    return Err(VectorFileError::UnsupportedScheme { uri: uri.to_string() });
                }
                Ok(Self {
                    scheme,
                    authority: host.to_string(),
                    object_path: path.to_string(),
                })
            }
            _ => Err(VectorFileError::UnsupportedScheme { uri: uri.to_string() }),
        }
    }
}

fn build_store(parsed: &ParsedUri, allow_http: bool) -> Result<Arc<dyn ObjectStore>, VectorFileError> {
    match parsed.scheme.as_str() {
        "s3" => {
            let mut b = AmazonS3Builder::from_env().with_bucket_name(&parsed.authority);
            if let Ok(region) = std::env::var("AWS_REGION").or_else(|_| std::env::var("AWS_DEFAULT_REGION")) {
                b = b.with_region(region);
            }
            let store = b.build().map_err(|e| VectorFileError::ObjectStore {
                what: "build_s3",
                source: e,
            })?;
            Ok(Arc::new(store))
        }
        "gs" => {
            let store = GoogleCloudStorageBuilder::from_env()
                .with_bucket_name(&parsed.authority)
                .build()
                .map_err(|e| VectorFileError::ObjectStore {
                    what: "build_gs",
                    source: e,
                })?;
            Ok(Arc::new(store))
        }
        "file" => {
            let store = LocalFileSystem::new();
            Ok(Arc::new(store))
        }
        "http" | "https" => {
            let url = format!("{}://{}", parsed.scheme, parsed.authority);
            // plain http must be explicitly opted into by the operator;
            // object_store rejects it otherwise. https never needs the flag.
            let store = HttpBuilder::new()
                .with_url(url)
                .with_client_options(ClientOptions::new().with_allow_http(allow_http))
                .build()
                .map_err(|e| VectorFileError::ObjectStore {
                    what: "build_http",
                    source: e,
                })?;
            Ok(Arc::new(store))
        }
        _ => Err(VectorFileError::UnsupportedScheme {
            uri: format!("{}://{}", parsed.scheme, parsed.authority),
        }),
    }
}

// silence unused import warning when no feature uses the type alias.
#[allow(dead_code)]
type _Scheme = ObjectStoreScheme;

#[cfg(test)]
mod tests;
