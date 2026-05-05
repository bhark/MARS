//! filesystem-backed adapter for `mars-store::ObjectStore`, `LocalCache`,
//! and `ManifestStore`. SPEC §8.5 / §10.2 / §10.3.

// `deny` rather than `forbid`: cache.rs uses a single `unsafe { Mmap::map(..) }`
// at the file-boundary read path with item-level `#[allow(unsafe_code)]`. the
// hexagonal architecture check still gates on crate-level `#![allow(...)]`,
// which is not used here.
#![deny(unsafe_code)]

use std::time::Duration;

mod cache;
mod key;
mod manifest;
mod store;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub use cache::FsCache;
pub use manifest::FsPublisher;
pub use store::FsStore;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
