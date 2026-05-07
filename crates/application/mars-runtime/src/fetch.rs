//! manifest-lookup → cache.get_or_fetch → ArtifactReader::open.

use mars_artifact::ArtifactReader;
use mars_store::{LocalCache, ObjectStore};
use mars_types::{ArtifactEntry, Cell, LayerId};

use crate::{
    RuntimeError,
    state::{LayerCellRef, LayerCellState, RuntimeState, SourceCellRef},
};

pub(crate) async fn fetch_layer(
    state: &RuntimeState,
    cache: &dyn LocalCache,
    store: &dyn ObjectStore,
    layer: &LayerId,
    cell: &Cell,
) -> Result<Option<ArtifactReader>, RuntimeError> {
    let Some(state) = state.layer_index.get(&LayerCellRef {
        layer: layer.as_str(),
        band: cell.band.as_str(),
        x: cell.x,
        y: cell.y,
    }) else {
        // a layer with no binding for the picked band contributes nothing —
        // soft-skip so a wide+narrow band composite still renders the wide
        // layer rather than aborting the whole request.
        tracing::debug!(
            layer = %layer.as_str(),
            band = %cell.band.as_str(),
            cell.x = cell.x,
            cell.y = cell.y,
            "no manifest binding for layer at this band; skipping",
        );
        return Ok(None);
    };
    match state {
        LayerCellState::Empty => Ok(None),
        LayerCellState::Present(entry) => fetch_entry(cache, store, entry).await.map(Some),
    }
}

pub(crate) async fn fetch_source(
    state: &RuntimeState,
    cache: &dyn LocalCache,
    store: &dyn ObjectStore,
    collection: &str,
    cell: &Cell,
) -> Result<ArtifactReader, RuntimeError> {
    let entry = state
        .source_index
        .get(&SourceCellRef {
            collection,
            band: cell.band.as_str(),
            x: cell.x,
            y: cell.y,
        })
        .ok_or_else(|| RuntimeError::SourceMissing {
            collection: collection.to_owned(),
            band: cell.band.as_str().to_owned(),
            cell: (cell.x, cell.y),
        })?;
    fetch_entry(cache, store, entry).await
}

async fn fetch_entry(
    cache: &dyn LocalCache,
    store: &dyn ObjectStore,
    entry: &ArtifactEntry,
) -> Result<ArtifactReader, RuntimeError> {
    let bytes = cache.get_or_fetch(&entry.key, entry.hash, store).await?;
    let reader = ArtifactReader::open(bytes)?;
    Ok(reader)
}
