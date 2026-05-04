//! filesystem-backed adapter for `mars-store::ObjectStore`, `LocalCache`,
//! and `ManifestStore`. SPEC §8.5 / §10.2 / §10.3.

#![forbid(unsafe_code)]

mod cache;
mod key;
mod manifest;
mod store;

pub use cache::FsCache;
pub use manifest::FsPublisher;
pub use store::FsStore;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
