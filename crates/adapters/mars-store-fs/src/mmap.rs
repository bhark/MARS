#![allow(unsafe_code)]

use std::path::Path;

use bytes::Bytes;
use mars_store::StoreError;

/// open `path` and return its contents as `Bytes` backed by an mmap. the
/// `File` handle is dropped immediately after mapping; the kernel keeps the
/// mapping alive via the `Mmap`. returns `Ok(None)` if the file is absent.
///
/// # safety
/// `Mmap` is unsafe because external mutation of the file (or truncation)
/// while the map is live is undefined behaviour. we only mmap files we wrote
/// atomically (rename-into-place) and never mutate in place; eviction unlinks
/// but the map keeps the inode pinned until `Bytes` drops.
pub(crate) fn read_mmap(path: &Path) -> Result<Option<Bytes>, StoreError> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError::Backend(format!("cache open: {e}"))),
    };
    let meta = file
        .metadata()
        .map_err(|e| StoreError::Backend(format!("cache stat: {e}")))?;
    if meta.len() == 0 {
        return Ok(Some(Bytes::new()));
    }
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| StoreError::Backend(format!("cache mmap: {e}")))?;
    Ok(Some(Bytes::from_owner(mmap)))
}
