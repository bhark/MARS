//! manifest-lookup → cache.get_or_fetch → ArtifactReader::open.

use mars_artifact::ArtifactReader;
use mars_store::{LocalCache, ObjectStore};
use mars_types::{ArtifactEntry, Cell, LayerId};

use crate::{
    RuntimeError,
    state::{LayerCellState, RuntimeState},
};

pub(crate) async fn fetch_layer(
    state: &RuntimeState,
    cache: &dyn LocalCache,
    store: &dyn ObjectStore,
    layer: &LayerId,
    cell: &Cell,
) -> Result<Option<ArtifactReader>, RuntimeError> {
    let state = state
        .layer_index
        .get(&(layer.clone(), cell.band.clone(), (cell.x, cell.y)))
        .ok_or_else(|| RuntimeError::ManifestEntryMissing {
            layer: layer.as_str().to_owned(),
            band: cell.band.as_str().to_owned(),
            cell: (cell.x, cell.y),
        })?;
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
        .get(&(collection.to_owned(), cell.band.clone(), (cell.x, cell.y)))
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
