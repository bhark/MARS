//! page-membership sidecars fetched for a pipeline run.
//!
//! bundles the owned `Bytes` per binding with a helper that materialises
//! [`SidecarReader`]s borrowing from those bytes. removes the two-call
//! lifetime dance the cycle and rebalance pipelines used to write inline
//! (fetch bytes into a local map, then build readers borrowing from the
//! map) which forced callers to structure let-bindings just so for the
//! readers to outlive the bytes. one owned value here is enough.
//!
//! readers are not cached on the struct because that would require a
//! self-referential type. [`SidecarReader::open`] does an ascending-order
//! validation scan, so callers materialise once per pipeline and pass
//! `&HashMap<_, SidecarReader<'_>>` thereafter.

use std::collections::HashMap;

use bytes::Bytes;
use mars_store::ObjectStore;
use mars_types::{BindingId, BindingMetadata};

use crate::CompilerError;
use crate::sidecar::SidecarReader;

pub(crate) struct OwnedSidecars {
    bytes: HashMap<BindingId, Bytes>,
}

impl OwnedSidecars {
    /// fetch every binding's page-membership sidecar from object storage.
    /// bindings without a sidecar reference are skipped (the resulting map
    /// has no entry for them, mirroring the pre-existing behaviour the
    /// cycle and rebalance paths rely on for first-time bindings).
    pub(crate) async fn fetch(store: &dyn ObjectStore, bindings: &[BindingMetadata]) -> Result<Self, CompilerError> {
        let mut out = HashMap::with_capacity(bindings.len());
        for meta in bindings {
            if let Some(entry) = &meta.page_membership_sidecar {
                let bytes = store.get(&entry.key, entry.hash).await?;
                out.insert(meta.binding_id.clone(), bytes);
            }
        }
        Ok(Self { bytes: out })
    }

    /// open a [`SidecarReader`] over each fetched payload. readers borrow
    /// from `self.bytes` for the returned map's lifetime.
    pub(crate) fn readers(&self) -> Result<HashMap<BindingId, SidecarReader<'_>>, CompilerError> {
        self.bytes
            .iter()
            .map(|(id, b)| Ok((id.clone(), SidecarReader::open(b)?)))
            .collect()
    }
}
