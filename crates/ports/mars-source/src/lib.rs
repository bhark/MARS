//! Port traits for source backends and change feeds.
//!
//! `Source` is the read interface used by the compiler to materialise
//! geometries and attributes per page. `ChangeFeed` is the subscription
//! interface that produces dirty-page events. `RasterSource` covers
//! tile-pyramid backends. `LeaderLock` is the coordination primitive that
//! keeps at most one compiler instance active per service. All traits are
//! runtime-agnostic - concrete adapters live in `crates/adapters/mars-source-*`.
//!
//! `CompileSession` exposes a snapshot-stable `stream_rows` that reuses the
//! pass-1 transaction.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod access;

mod binding;
mod change;
mod coord;
mod error;
mod raster;
mod vector;

pub use access::RowAttrs;
pub use binding::SourceBinding;
pub use change::{
    BindingHealth, ChangeBatch, ChangeEvent, ChangeFeed, ChangeSubscription, GeometryEnvelope, RebindReason,
};
pub use coord::{LeaderLock, LeaderLockGuard};
pub use error::SourceError;
pub use mars_types::SourceCollectionId;
pub use raster::{RasterBinding, RasterSource, TileBytes};
pub use vector::{AttrValue, CompileSession, RowBytes, RowSummary, Source, SourceRowKey};
