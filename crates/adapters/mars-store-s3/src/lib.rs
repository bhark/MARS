//! object_store-backed adapter for `mars-store::ObjectStore` and
//! `mars-store::ManifestStore`. Supports S3 / MinIO / R2 / GCS via the
//! `object_store` crate, plus an in-memory backend for unit tests.
//!
//! atomicity for `manifests/current` is via conditional put (`PutMode::Update`
//! with the prior etag). on bucket backends without CAS support the impl
//! falls back to overwrite and logs a warning.

#![forbid(unsafe_code)]

mod config;
mod manifest;
mod store;

pub use config::S3Config;
pub use manifest::S3Publisher;
pub use store::S3Store;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
