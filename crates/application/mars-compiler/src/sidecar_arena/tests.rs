#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn round_trip_preserves_order_and_count() {
    let mut w = SidecarArenaWriter::new(std::env::temp_dir().as_path()).unwrap();
    for i in 0..1000u64 {
        w.push(i, HilbertKey::new(i * 3 + 7)).unwrap();
    }
    let a = w.finish().unwrap();
    assert_eq!(a.len(), 1000);
    let v = a.drain_into_vec().unwrap();
    assert_eq!(v.len(), 1000);
    for (i, (id, k)) in v.iter().enumerate() {
        assert_eq!(*id, i as u64);
        assert_eq!(k.get(), (i as u64) * 3 + 7);
    }
}

#[test]
fn empty_arena_round_trips() {
    let w = SidecarArenaWriter::new(std::env::temp_dir().as_path()).unwrap();
    let a = w.finish().unwrap();
    assert!(a.is_empty());
    let v = a.drain_into_vec().unwrap();
    assert!(v.is_empty());
}

#[test]
fn drop_cleans_scratch() {
    let path;
    {
        let w = SidecarArenaWriter::new(std::env::temp_dir().as_path()).unwrap();
        path = w.path.clone();
        assert!(path.exists());
        let _a = w.finish().unwrap();
        assert!(path.exists());
        // arena drop releases the last Arc<TempDir>.
    }
    assert!(!path.exists());
}
