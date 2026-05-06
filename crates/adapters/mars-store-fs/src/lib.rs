//! filesystem-backed adapter for `mars-store::ObjectStore`, `LocalCache`,
//! and `ManifestStore`. SPEC §8.5 / §10.2 / §10.3.
#![deny(unsafe_code)]

use std::time::Duration;

mod cache;
mod key;
mod manifest;
mod mmap;
mod store;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub use cache::FsCache;
pub use manifest::FsPublisher;
pub use store::FsStore;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
