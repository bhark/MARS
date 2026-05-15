//! filesystem-backed local cache bench.
//!
//! drives `FsCache::get_or_fetch` against an in-memory origin (`InMemoryStore`
//! from `mars-store/test-utils`). measures four scenarios:
//! * `cold_miss`         - empty cache, single fetch from origin
//! * `warm_hit`          - pre-populated cache, repeated reads
//! * `eviction_pressure` - 10 MiB budget, sequence of 1 MiB artifacts forces
//!   LRU churn on every insert
//! * `mixed_80_20`       - stable working set: 80 % warm hits, 20 % cold
//!   miss (forces fetch + verify path)
//!
//! the cold-miss number includes one BLAKE3 verify pass + atomic_write +
//! LRU bookkeeping; the warm-hit number is the mmap-open + verify cost on
//! a hit.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_artifact::compute_content_hash;
use mars_store::mem::InMemoryStore;
use mars_store::{LocalCache, ObjectStore};
use mars_store_fs::FsCache;
use mars_types::ArtifactKey;
use tempfile::TempDir;

const ONE_MIB: usize = 1024 * 1024;

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// fixed-size deterministic blob; pseudo-random contents so blake3 work
/// reflects production (a flat zero buffer compresses too well in some
/// downstream contexts to be representative).
fn make_blob(seed: u64, size: usize) -> Bytes {
    let mut v = vec![0u8; size];
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for chunk in v.chunks_mut(8) {
        x ^= x >> 30;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 27;
        let bytes = x.to_le_bytes();
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = bytes[i];
        }
    }
    Bytes::from(v)
}

fn k(s: &str) -> ArtifactKey {
    ArtifactKey::new(s)
}

fn bench_cold_miss(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("store_fs_cache_cold_miss");
    for &size_mib in &[1usize, 5, 50] {
        let size = size_mib * ONE_MIB;
        group.throughput(Throughput::Bytes(size as u64));
        let id = BenchmarkId::from_parameter(format!("size_{size_mib}MiB"));
        group.bench_function(id, |b| {
            b.iter_with_setup(
                || {
                    // fresh tempdir + fresh origin per iteration; sized for
                    // headroom so a single cold fetch never triggers eviction.
                    let td = TempDir::new().unwrap();
                    let cache = FsCache::new(td.path(), (size as u64) * 4).unwrap();
                    let origin: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
                    let key = k("page/cold.bin");
                    let blob = make_blob(0xC01D, size);
                    let hash = compute_content_hash(&blob);
                    rt.block_on(async {
                        origin.put(&key, blob).await.unwrap();
                    });
                    (td, cache, origin, key, hash)
                },
                |(_td, cache, origin, key, hash)| {
                    let bytes = rt.block_on(async { cache.get_or_fetch(&key, hash, origin.as_ref()).await.unwrap() });
                    black_box(bytes);
                },
            );
        });
    }
    group.finish();
}

fn bench_warm_hit(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("store_fs_cache_warm_hit");
    for &size_mib in &[1usize, 5, 50] {
        let size = size_mib * ONE_MIB;
        group.throughput(Throughput::Bytes(size as u64));

        // build cache + populate once outside the iter loop so we measure
        // the warm hit, not the initial fetch.
        let td = TempDir::new().unwrap();
        let cache = FsCache::new(td.path(), (size as u64) * 4).unwrap();
        let origin: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
        let key = k("page/warm.bin");
        let blob = make_blob(0xC0FFEE, size);
        let hash = compute_content_hash(&blob);
        rt.block_on(async {
            origin.put(&key, blob.clone()).await.unwrap();
            // first call populates cache + verifies; subsequent calls are warm.
            cache.get_or_fetch(&key, hash, origin.as_ref()).await.unwrap();
        });

        let id = BenchmarkId::from_parameter(format!("size_{size_mib}MiB"));
        group.bench_function(id, |b| {
            b.iter(|| {
                let bytes = rt.block_on(cache.get_or_fetch(&key, hash, origin.as_ref())).unwrap();
                black_box(bytes);
            });
        });
        // keep tempdir alive across bench iterations
        drop(td);
    }
    group.finish();
}

fn bench_eviction_pressure(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("store_fs_cache_eviction_pressure");
    let size = ONE_MIB; // 1 MiB per artifact
    let budget = 10 * ONE_MIB; // cache holds ~10
    let n_artifacts = 50; // 5x oversubscribed; every insert evicts
    group.throughput(Throughput::Elements(n_artifacts as u64));

    let id = BenchmarkId::from_parameter(format!("budget_{}MiB_count_{n_artifacts}", budget / ONE_MIB));
    group.bench_function(id, |b| {
        b.iter_with_setup(
            || {
                let td = TempDir::new().unwrap();
                let cache = FsCache::new(td.path(), budget as u64).unwrap();
                let origin: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
                let mut entries = Vec::with_capacity(n_artifacts);
                rt.block_on(async {
                    for i in 0..n_artifacts {
                        let key = k(&format!("page/p{i:04}.bin"));
                        let blob = make_blob(i as u64, size);
                        let hash = compute_content_hash(&blob);
                        origin.put(&key, blob).await.unwrap();
                        entries.push((key, hash));
                    }
                });
                (td, cache, origin, entries)
            },
            |(_td, cache, origin, entries)| {
                rt.block_on(async {
                    for (key, hash) in &entries {
                        let bytes = cache.get_or_fetch(key, *hash, origin.as_ref()).await.unwrap();
                        black_box(bytes);
                    }
                });
            },
        );
    });
    group.finish();
}

fn bench_mixed(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("store_fs_cache_mixed_80_20");
    let size = ONE_MIB;
    let working_set = 8;
    let cold_miss_count = 2;
    let total = working_set + cold_miss_count;
    group.throughput(Throughput::Elements(total as u64));

    // cache budget large enough to fit the working set comfortably but not
    // the cold misses (forces eviction-on-miss + re-fetch on hit window).
    let budget = (working_set as u64) * (size as u64) * 2;

    let id = BenchmarkId::from_parameter(format!("ws_{working_set}_misses_{cold_miss_count}"));
    group.bench_function(id, |b| {
        b.iter_with_setup(
            || {
                let td = TempDir::new().unwrap();
                let cache = FsCache::new(td.path(), budget).unwrap();
                let origin: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
                let mut warm = Vec::with_capacity(working_set);
                let mut cold = Vec::with_capacity(cold_miss_count);
                rt.block_on(async {
                    for i in 0..working_set {
                        let key = k(&format!("page/warm_{i:04}.bin"));
                        let blob = make_blob(0x1000 + i as u64, size);
                        let hash = compute_content_hash(&blob);
                        origin.put(&key, blob).await.unwrap();
                        // pre-populate: now warm.
                        cache.get_or_fetch(&key, hash, origin.as_ref()).await.unwrap();
                        warm.push((key, hash));
                    }
                    for i in 0..cold_miss_count {
                        let key = k(&format!("page/cold_{i:04}.bin"));
                        let blob = make_blob(0x2000 + i as u64, size);
                        let hash = compute_content_hash(&blob);
                        origin.put(&key, blob).await.unwrap();
                        cold.push((key, hash));
                    }
                });
                (td, cache, origin, warm, cold)
            },
            |(_td, cache, origin, warm, cold)| {
                rt.block_on(async {
                    // 80/20 mix in a single pass: all warm hits + a few cold misses.
                    for (key, hash) in &warm {
                        let b = cache.get_or_fetch(key, *hash, origin.as_ref()).await.unwrap();
                        black_box(b);
                    }
                    for (key, hash) in &cold {
                        let b = cache.get_or_fetch(key, *hash, origin.as_ref()).await.unwrap();
                        black_box(b);
                    }
                });
            },
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_cold_miss,
    bench_warm_hit,
    bench_eviction_pressure,
    bench_mixed
);
criterion_main!(benches);
