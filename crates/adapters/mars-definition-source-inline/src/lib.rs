//! Inline-literal adapter for [`mars_definition_source::DefinitionSource`].
//!
//! Captures a `RenderDefinition` payload at construction (typically the
//! `spec.definition.inline` string from a `MarsService` CR) and serves it back
//! verbatim on every `fetch`. `watch` returns an empty stream: there are no
//! in-band change events for inline payloads because the CR's own
//! `resourceVersion` change drives reconcile via the operator's existing kube
//! watch on `MarsService`.

#![deny(unsafe_code)]
#![deny(missing_docs)]

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::{StreamExt, stream};
use mars_definition_source::{Change, DefinitionBytes, DefinitionSource, DefinitionSourceError};

// 16 hex chars = 64 bits of blake3. plenty for an inline payload that doesn't
// churn; collision risk is irrelevant since revisions are only ever compared
// for equality with the prior observed value on the same CR.
const REVISION_HEX_LEN: usize = 16;

/// Adapter that wraps a fixed payload and serves it without I/O.
#[derive(Debug, Clone)]
pub struct InlineDefinitionSource {
    data: Bytes,
    revision: String,
}

impl InlineDefinitionSource {
    /// Build from any byte-like payload (typically the literal CR string).
    pub fn new(payload: impl Into<Bytes>) -> Self {
        let data = payload.into();
        let revision = blake3::hash(&data).to_hex()[..REVISION_HEX_LEN].to_string();
        Self { data, revision }
    }
}

#[async_trait]
impl DefinitionSource for InlineDefinitionSource {
    async fn fetch(&self) -> Result<DefinitionBytes, DefinitionSourceError> {
        Ok(DefinitionBytes {
            data: self.data.clone(),
            revision: self.revision.clone(),
        })
    }

    fn watch(&self) -> BoxStream<'static, Change> {
        stream::empty().boxed()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
