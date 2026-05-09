//! Disk-backed bucket spill for the bootstrap external sort.
//!
//! Bootstrap streams source rows through a bucketed Hilbert-key sort. The
//! in-memory hot path holds every row in a `Vec<KeyedRow>`; once the
//! accumulator exceeds the configured threshold the snapshot driver drains
//! the in-RAM tail into per-bucket spill files (one append-only file per
//! Hilbert-prefix bucket) and routes subsequent rows directly to disk. The
//! gather pass mmaps each bucket file in turn, decodes its rows, runs the
//! deterministic in-bucket tiebreak sort, and yields the resulting slice
//! to the page sweep.
//!
//! Bucket boundaries align with the top `bucket_bits` of the 64-bit Hilbert
//! key, so cross-bucket order is preserved by construction; only the
//! in-bucket comparator runs at gather time.
//!
//! This module owns:
//! 1. the on-disk frame for [`KeyedRow`] (encode / decode), and
//! 2. the bucket spill writer/reader (next step).
//!
//! Both pieces are private to `mars-compiler`. The on-disk frame is
//! ephemeral — spill files live for the duration of one binding compile —
//! so there is deliberately no version field or backwards-compatibility
//! shim.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mars_artifact::{decode_geometry_payload, encode_geometry_payload};
use mars_source::AttrValue;
use mars_types::HilbertKey;
use tempfile::{NamedTempFile, TempPath};

use crate::snapshot::KeyedRow;

/// Bits of Hilbert prefix used to partition rows across spill buckets. Each
/// bucket gets one open file at scatter time, so the bucket count caps the
/// spill path's FD budget. 9 bits = 512 buckets keeps a comfortable margin
/// under the typical 1024 soft RLIMIT_NOFILE while leaving in-bucket gather
/// sizes small enough to sort in RAM.
pub(crate) const SPILL_BUCKET_BITS: u8 = 9;
const SPILL_BUCKET_COUNT: usize = 1 << SPILL_BUCKET_BITS;
const WRITE_BUF_BYTES: usize = 64 * 1024;

/// Errors emitted by the spill codec.
#[derive(Debug, thiserror::Error)]
pub enum SpillError {
    /// Underlying I/O error.
    #[error("spill io: {context}")]
    Io {
        /// what operation failed.
        context: String,
        /// underlying std::io error.
        #[source]
        source: std::io::Error,
    },
    /// Geometry codec roundtrip failure.
    #[error("spill geometry: {0}")]
    Geometry(#[from] mars_artifact::ArtifactError),
    /// Frame is malformed (truncated, bad tag, length over budget).
    #[error("spill frame malformed: {0}")]
    Malformed(&'static str),
    /// Frame size exceeds the per-row safety ceiling — protects the gather
    /// pass from a corrupt header demanding a multi-GiB allocation.
    #[error("spill frame too large: {got} bytes (max {max})")]
    TooLarge {
        /// observed length.
        got: u64,
        /// hard ceiling.
        max: u64,
    },
    /// Total spill bytes for one binding crossed the configured scratch
    /// budget. Surfaced to the caller so it can be re-raised as
    /// [`crate::CompilerError::ScratchBudgetExceeded`].
    #[error("spill scratch budget exceeded for binding {binding}: {observed_bytes} > {budget_bytes}")]
    BudgetExceeded {
        /// Affected binding id.
        binding: String,
        /// Total bytes accumulated when the budget was crossed.
        observed_bytes: u64,
        /// Configured budget.
        budget_bytes: u64,
    },
}

const ATTR_NULL: u8 = 0;
const ATTR_BOOL: u8 = 1;
const ATTR_INT: u8 = 2;
const ATTR_FLOAT: u8 = 3;
const ATTR_STRING: u8 = 4;

/// Hard safety ceiling on a single encoded row. Mirrors the artifact
/// reader's MAX_ROW_BYTES posture: a corrupt or adversarial frame should
/// surface as a typed error rather than an unbounded allocation. 64 MiB is
/// far above any plausible single feature.
pub const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Encode one [`KeyedRow`] into the supplied buffer. Frame is self-delimiting
/// (length-prefixed segments), so concatenating frames produces a valid
/// stream readable by [`decode_one`] in order.
pub(crate) fn encode_one(row: &KeyedRow, out: &mut Vec<u8>) -> Result<(), SpillError> {
    out.extend_from_slice(&row.key.get().to_le_bytes());
    out.extend_from_slice(&row.feature.user_id.to_le_bytes());
    out.extend_from_slice(&row.row_fingerprint.to_le_bytes());
    out.extend_from_slice(&row.geom_bytes_estimate.to_le_bytes());
    for v in row.feature.bbox {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let geom_payload = encode_geometry_payload(std::slice::from_ref(&row.feature))?;
    let geom_len: u32 = geom_payload
        .len()
        .try_into()
        .map_err(|_| SpillError::Malformed("geom payload >= 4 GiB"))?;
    out.extend_from_slice(&geom_len.to_le_bytes());
    out.extend_from_slice(&geom_payload);

    let attr_count: u32 = row
        .attrs
        .len()
        .try_into()
        .map_err(|_| SpillError::Malformed("attr count overflow"))?;
    out.extend_from_slice(&attr_count.to_le_bytes());
    for (name, value) in row.attrs.iter() {
        let nlen: u32 = name
            .len()
            .try_into()
            .map_err(|_| SpillError::Malformed("attr name >= 4 GiB"))?;
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        match value {
            AttrValue::Null => out.push(ATTR_NULL),
            AttrValue::Bool(b) => {
                out.push(ATTR_BOOL);
                out.push(u8::from(*b));
            }
            AttrValue::Int(i) => {
                out.push(ATTR_INT);
                out.extend_from_slice(&i.to_le_bytes());
            }
            AttrValue::Float(f) => {
                out.push(ATTR_FLOAT);
                out.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            AttrValue::String(s) => {
                out.push(ATTR_STRING);
                let slen: u32 = s
                    .len()
                    .try_into()
                    .map_err(|_| SpillError::Malformed("attr string >= 4 GiB"))?;
                out.extend_from_slice(&slen.to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    Ok(())
}

/// Decode one [`KeyedRow`] from `bytes` starting at `*pos`. Advances `pos`
/// past the consumed bytes on success.
pub(crate) fn decode_one(bytes: &[u8], pos: &mut usize) -> Result<KeyedRow, SpillError> {
    let key = HilbertKey::new(read_u64_le(bytes, pos)?);
    let user_id = read_u64_le(bytes, pos)?;
    let row_fingerprint = read_u64_le(bytes, pos)?;
    let geom_bytes_estimate = read_u64_le(bytes, pos)?;
    let mut bbox = [0f32; 4];
    for slot in &mut bbox {
        *slot = f32::from_le_bytes(read_array::<4>(bytes, pos)?);
    }

    let geom_len = read_u32_le(bytes, pos)? as u64;
    if geom_len > MAX_FRAME_BYTES {
        return Err(SpillError::TooLarge {
            got: geom_len,
            max: MAX_FRAME_BYTES,
        });
    }
    let geom_len = usize::try_from(geom_len).map_err(|_| SpillError::Malformed("geom_len > usize"))?;
    let geom_bytes = take_slice(bytes, pos, geom_len)?;
    let mut features = decode_geometry_payload(geom_bytes)?;
    if features.len() != 1 {
        return Err(SpillError::Malformed("expected single-feature geometry payload"));
    }
    let mut feature = features.remove(0);
    // Override fields the geometry codec re-derives from coords. The header
    // carries the source-supplied user_id and the bbox the compiler used to
    // compute the Hilbert key, so the post-spill row is byte-identical to
    // its in-memory counterpart.
    feature.user_id = user_id;
    feature.bbox = bbox;

    let attr_count = read_u32_le(bytes, pos)? as usize;
    let mut attrs: Vec<(String, AttrValue)> = Vec::with_capacity(attr_count);
    for _ in 0..attr_count {
        let nlen = read_u32_le(bytes, pos)? as usize;
        let name_bytes = take_slice(bytes, pos, nlen)?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| SpillError::Malformed("attr name not utf8"))?
            .to_owned();
        let tag = read_u8(bytes, pos)?;
        let value = match tag {
            ATTR_NULL => AttrValue::Null,
            ATTR_BOOL => {
                let b = read_u8(bytes, pos)?;
                AttrValue::Bool(b != 0)
            }
            ATTR_INT => AttrValue::Int(i64::from_le_bytes(read_array::<8>(bytes, pos)?)),
            ATTR_FLOAT => AttrValue::Float(f64::from_bits(u64::from_le_bytes(read_array::<8>(bytes, pos)?))),
            ATTR_STRING => {
                let slen = read_u32_le(bytes, pos)? as usize;
                let s_bytes = take_slice(bytes, pos, slen)?;
                let s = std::str::from_utf8(s_bytes)
                    .map_err(|_| SpillError::Malformed("attr string not utf8"))?
                    .to_owned();
                AttrValue::String(s)
            }
            _ => return Err(SpillError::Malformed("unknown attr tag")),
        };
        attrs.push((name, value));
    }

    Ok(KeyedRow {
        feature,
        attrs: Arc::new(attrs),
        geom_bytes_estimate,
        key,
        row_fingerprint,
    })
}

#[inline]
fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, SpillError> {
    let arr = read_array::<1>(bytes, pos)?;
    Ok(arr[0])
}

#[inline]
fn read_u32_le(bytes: &[u8], pos: &mut usize) -> Result<u32, SpillError> {
    Ok(u32::from_le_bytes(read_array::<4>(bytes, pos)?))
}

#[inline]
fn read_u64_le(bytes: &[u8], pos: &mut usize) -> Result<u64, SpillError> {
    Ok(u64::from_le_bytes(read_array::<8>(bytes, pos)?))
}

#[inline]
fn read_array<const N: usize>(bytes: &[u8], pos: &mut usize) -> Result<[u8; N], SpillError> {
    let slice = take_slice(bytes, pos, N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

#[inline]
fn take_slice<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], SpillError> {
    let end = pos.checked_add(n).ok_or(SpillError::Malformed("offset overflow"))?;
    if end > bytes.len() {
        return Err(SpillError::Malformed("frame truncated"));
    }
    let s = &bytes[*pos..end];
    *pos = end;
    Ok(s)
}

#[inline]
fn bucket_for(key: HilbertKey) -> usize {
    let shift = 64 - u32::from(SPILL_BUCKET_BITS);
    (key.get() >> shift) as usize
}

/// Per-binding bucket spill. Owns one append-only temp file per Hilbert
/// prefix bucket; rows are routed by the top [`SPILL_BUCKET_BITS`] of their
/// Hilbert key. After scatter completes, the writer side is finalised into
/// a [`SpillReader`] which iterates buckets in Hilbert order, decodes each
/// bucket's rows, and applies the in-bucket tiebreak sort to feed the
/// downstream page sweep.
pub(crate) struct BucketSpill {
    binding_id: String,
    scratch_dir: PathBuf,
    budget_bytes: u64,
    total_bytes: u64,
    writers: Vec<Option<BucketWriter>>,
    bucket_row_counts: Vec<usize>,
    encode_buf: Vec<u8>,
}

struct BucketWriter {
    inner: BufWriter<NamedTempFile>,
    bytes_written: u64,
}

impl BucketSpill {
    /// Create an empty spill rooted at `scratch_dir`. Files are not created
    /// until the first row lands in the corresponding bucket.
    pub(crate) fn new(binding_id: String, scratch_dir: PathBuf, budget_bytes: u64) -> Self {
        let mut writers = Vec::with_capacity(SPILL_BUCKET_COUNT);
        for _ in 0..SPILL_BUCKET_COUNT {
            writers.push(None);
        }
        Self {
            binding_id,
            scratch_dir,
            budget_bytes,
            total_bytes: 0,
            writers,
            bucket_row_counts: vec![0; SPILL_BUCKET_COUNT],
            encode_buf: Vec::with_capacity(4096),
        }
    }

    /// Encode `row` and append it to its bucket file. Lazily creates the
    /// bucket file on first write. Returns
    /// [`SpillError::BudgetExceeded`] when the running total of all bucket
    /// file sizes would cross `budget_bytes`.
    pub(crate) fn append(&mut self, row: &KeyedRow) -> Result<(), SpillError> {
        let bucket = bucket_for(row.key);
        self.encode_buf.clear();
        encode_one(row, &mut self.encode_buf)?;
        let frame_len = self.encode_buf.len() as u64;
        let new_total = self.total_bytes.saturating_add(frame_len);
        if new_total > self.budget_bytes {
            return Err(SpillError::BudgetExceeded {
                binding: self.binding_id.clone(),
                observed_bytes: new_total,
                budget_bytes: self.budget_bytes,
            });
        }
        let slot = &mut self.writers[bucket];
        if slot.is_none() {
            let temp = NamedTempFile::new_in(&self.scratch_dir).map_err(|source| SpillError::Io {
                context: format!("create spill bucket file in {}", self.scratch_dir.display()),
                source,
            })?;
            *slot = Some(BucketWriter {
                inner: BufWriter::with_capacity(WRITE_BUF_BYTES, temp),
                bytes_written: 0,
            });
        }
        let writer = match slot {
            Some(w) => w,
            None => return Err(SpillError::Malformed("bucket writer slot vacant after insert")),
        };
        writer
            .inner
            .write_all(&self.encode_buf)
            .map_err(|source| SpillError::Io {
                context: format!("write spill bucket {bucket} for binding {}", self.binding_id),
                source,
            })?;
        writer.bytes_written = writer.bytes_written.saturating_add(frame_len);
        self.total_bytes = new_total;
        self.bucket_row_counts[bucket] = self.bucket_row_counts[bucket].saturating_add(1);
        Ok(())
    }

    /// Total bytes written across all bucket files so far. Test-only at
    /// present; threaded into metrics in a future cycle.
    #[cfg(test)]
    pub(crate) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Number of buckets that have at least one row.
    #[cfg(test)]
    pub(crate) fn populated_buckets(&self) -> usize {
        self.writers.iter().filter(|w| w.is_some()).count()
    }

    /// Flush every bucket writer and convert into a reader-side
    /// [`SpillReader`]. Empty buckets are dropped.
    pub(crate) fn into_reader(self) -> Result<SpillReader, SpillError> {
        let BucketSpill {
            binding_id,
            writers,
            bucket_row_counts,
            ..
        } = self;
        let mut buckets: Vec<Option<TempPath>> = Vec::with_capacity(SPILL_BUCKET_COUNT);
        for slot in writers {
            match slot {
                None => buckets.push(None),
                Some(BucketWriter { inner, .. }) => {
                    let temp = inner.into_inner().map_err(|e| SpillError::Io {
                        context: format!("flush spill writer for binding {binding_id}"),
                        source: e.into_error(),
                    })?;
                    buckets.push(Some(temp.into_temp_path()));
                }
            }
        }
        Ok(SpillReader {
            binding_id,
            buckets,
            bucket_row_counts,
        })
    }
}

/// Read-side handle: holds one [`TempPath`] per non-empty bucket. Walking
/// buckets in index order (top-`SPILL_BUCKET_BITS` of the Hilbert key)
/// yields rows in cross-bucket Hilbert order; the in-bucket tiebreak sort
/// inside [`SpillReader::take_bucket`] applies the full
/// `(key, user_id, row_fingerprint)` comparator.
pub(crate) struct SpillReader {
    binding_id: String,
    buckets: Vec<Option<TempPath>>,
    bucket_row_counts: Vec<usize>,
}

impl SpillReader {
    /// Decode and tiebreak-sort all rows in bucket `idx`. Returns an empty
    /// vec for unpopulated buckets.
    pub(crate) fn take_bucket(&self, idx: usize) -> Result<Vec<KeyedRow>, SpillError> {
        let Some(temp) = self.buckets.get(idx).and_then(|s| s.as_ref()) else {
            return Ok(Vec::new());
        };
        let rows = read_bucket_rows(temp.as_ref(), &self.binding_id)?;
        let mut rows = rows;
        rows.sort_by(|a, b| {
            a.key
                .cmp(&b.key)
                .then_with(|| a.feature.user_id.cmp(&b.feature.user_id))
                .then_with(|| a.row_fingerprint.cmp(&b.row_fingerprint))
        });
        Ok(rows)
    }

    /// Number of buckets (always [`SPILL_BUCKET_COUNT`]).
    pub(crate) fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Row count in bucket `idx`, 0 if the bucket is empty.
    pub(crate) fn bucket_rows(&self, idx: usize) -> usize {
        self.bucket_row_counts.get(idx).copied().unwrap_or(0)
    }
}

fn read_bucket_rows(path: &Path, binding_id: &str) -> Result<Vec<KeyedRow>, SpillError> {
    let file = File::open(path).map_err(|source| SpillError::Io {
        context: format!("open spill bucket {} for binding {binding_id}", path.display()),
        source,
    })?;
    let mut reader = BufReader::with_capacity(WRITE_BUF_BYTES, file);
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).map_err(|source| SpillError::Io {
        context: format!("read spill bucket {} for binding {binding_id}", path.display()),
        source,
    })?;
    let mut rows = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        let row = decode_one(&buf, &mut pos)?;
        rows.push(row);
    }
    Ok(rows)
}

/// Single linear spill file used as pre-key staging during the source
/// stream: rows arrive without their final Hilbert key (combined_bbox is
/// only known at end-of-stream), so they are appended as encoded
/// [`KeyedRow`] frames with `key = HilbertKey::min()`. After the stream
/// closes, the caller computes `combined_bbox`, reads the linear spill
/// back, and re-routes each row through [`BucketSpill`] with its proper
/// key.
pub(crate) struct LinearSpill {
    binding_id: String,
    file: BufWriter<NamedTempFile>,
    bytes_written: u64,
    encode_buf: Vec<u8>,
}

impl LinearSpill {
    /// Open a fresh linear spill in `scratch_dir`.
    pub(crate) fn new(binding_id: String, scratch_dir: &Path) -> Result<Self, SpillError> {
        let temp = NamedTempFile::new_in(scratch_dir).map_err(|source| SpillError::Io {
            context: format!("create linear spill in {}", scratch_dir.display()),
            source,
        })?;
        Ok(Self {
            binding_id,
            file: BufWriter::with_capacity(WRITE_BUF_BYTES, temp),
            bytes_written: 0,
            encode_buf: Vec::with_capacity(4096),
        })
    }

    /// Append one row in pre-key form. Caller is responsible for writing
    /// the row's `key` field as `HilbertKey::min()` (or any sentinel — the
    /// reader overwrites it once `combined_bbox` is known).
    pub(crate) fn append(&mut self, row: &KeyedRow) -> Result<(), SpillError> {
        self.encode_buf.clear();
        encode_one(row, &mut self.encode_buf)?;
        self.file.write_all(&self.encode_buf).map_err(|source| SpillError::Io {
            context: format!("write linear spill for binding {}", self.binding_id),
            source,
        })?;
        self.bytes_written = self.bytes_written.saturating_add(self.encode_buf.len() as u64);
        Ok(())
    }

    /// Bytes written to the spill file so far.
    pub(crate) fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Drain the spill, calling `f` once per row in stream order. Closes
    /// and removes the underlying temp file when finished.
    pub(crate) fn drain<F>(self, mut f: F) -> Result<(), SpillError>
    where
        F: FnMut(KeyedRow) -> Result<(), SpillError>,
    {
        let LinearSpill {
            binding_id,
            file,
            bytes_written: _,
            encode_buf: _,
        } = self;
        let temp = file.into_inner().map_err(|e| SpillError::Io {
            context: format!("flush linear spill for binding {binding_id}"),
            source: e.into_error(),
        })?;
        let path = temp.into_temp_path();
        let opened = File::open(&path).map_err(|source| SpillError::Io {
            context: format!("open linear spill {} for binding {binding_id}", path.display()),
            source,
        })?;
        let mut reader = BufReader::with_capacity(WRITE_BUF_BYTES, opened);
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map_err(|source| SpillError::Io {
            context: format!("read linear spill for binding {binding_id}"),
            source,
        })?;
        let mut pos = 0usize;
        while pos < buf.len() {
            let row = decode_one(&buf, &mut pos)?;
            f(row)?;
        }
        // path drops here, removing the file.
        drop(path);
        Ok(())
    }
}

/// Output of [`BootstrapAccumulator::finalize`]. Either an in-memory Vec
/// (no spill activated) or a [`SpillReader`] that walks bucket files in
/// Hilbert order. Both variants iterate rows in the same deterministic
/// `(key, user_id, row_fingerprint)` order; downstream consumers ignore
/// which side delivered the rows.
pub(crate) enum SortedRows {
    /// Rows fit in RAM; the Vec has already been bucketed-sorted and
    /// tiebreak-sorted in place.
    InMemory(Vec<KeyedRow>),
    /// Rows are stored in per-bucket spill files; iterate via
    /// [`SpillReader::take_bucket`] in increasing bucket index.
    Spilled(SpillReader),
}

/// Streaming row accumulator for the bootstrap path. Holds rows in RAM
/// until the configured threshold is crossed, then drains the in-memory
/// tail into a [`LinearSpill`] and routes subsequent rows there. After
/// the source stream closes the caller calls [`Self::finalize`] with the
/// observed `combined_bbox`; finalize computes per-row Hilbert keys and
/// either runs the in-memory bucketed sort or scatters into a
/// [`BucketSpill`] for disk-backed gather.
pub(crate) struct BootstrapAccumulator {
    binding_id: String,
    scratch_dir: PathBuf,
    scratch_budget_bytes: u64,
    spill_threshold_bytes: u64,
    in_memory: Vec<KeyedRow>,
    in_memory_bytes: u64,
    linear: Option<LinearSpill>,
}

impl BootstrapAccumulator {
    /// Create a new accumulator with no rows admitted yet.
    pub(crate) fn new(
        binding_id: String,
        scratch_dir: PathBuf,
        scratch_budget_bytes: u64,
        spill_threshold_bytes: u64,
    ) -> Self {
        Self {
            binding_id,
            scratch_dir,
            scratch_budget_bytes,
            spill_threshold_bytes,
            in_memory: Vec::new(),
            in_memory_bytes: 0,
            linear: None,
        }
    }

    /// Push one pre-key row (`row.key` should be [`HilbertKey::min`] until
    /// finalize assigns the real key). `est` is the approximate row byte
    /// estimate used for threshold accounting.
    pub(crate) fn push(&mut self, row: KeyedRow, est: u64) -> Result<(), SpillError> {
        if let Some(linear) = self.linear.as_mut() {
            // already spilling; route directly.
            check_budget(&self.binding_id, linear.bytes_written(), self.scratch_budget_bytes)?;
            linear.append(&row)?;
            check_budget(&self.binding_id, linear.bytes_written(), self.scratch_budget_bytes)?;
            return Ok(());
        }
        let projected = self.in_memory_bytes.saturating_add(est);
        if projected > self.spill_threshold_bytes {
            // crossing threshold: open linear spill, drain in_memory tail
            // into it, and append the new row there too.
            let mut linear = LinearSpill::new(self.binding_id.clone(), &self.scratch_dir)?;
            for staged in self.in_memory.drain(..) {
                linear.append(&staged)?;
                check_budget(&self.binding_id, linear.bytes_written(), self.scratch_budget_bytes)?;
            }
            linear.append(&row)?;
            check_budget(&self.binding_id, linear.bytes_written(), self.scratch_budget_bytes)?;
            self.in_memory_bytes = 0;
            self.linear = Some(linear);
            return Ok(());
        }
        self.in_memory_bytes = projected;
        self.in_memory.push(row);
        Ok(())
    }

    /// Drain the accumulator into a [`SortedRows`]. `assign_key` returns
    /// the proper Hilbert key for each row given `combined_bbox` semantics
    /// (the caller closes over `combined_bbox`).
    pub(crate) fn finalize<F>(self, mut assign_key: F) -> Result<SortedRows, SpillError>
    where
        F: FnMut(&KeyedRow) -> HilbertKey,
    {
        let BootstrapAccumulator {
            binding_id,
            scratch_dir,
            scratch_budget_bytes,
            spill_threshold_bytes: _,
            mut in_memory,
            in_memory_bytes: _,
            linear,
        } = self;

        match linear {
            None => {
                for r in in_memory.iter_mut() {
                    r.key = assign_key(r);
                }
                // bucketed_sort + tiebreak match the existing in-memory pipeline.
                crate::external_sort::bucketed_sort_in_place(
                    &mut in_memory,
                    crate::external_sort::ExternalSortConfig::DEFAULT.bucket_bits,
                    |r| r.key,
                );
                in_memory.sort_by(|a, b| {
                    a.key
                        .cmp(&b.key)
                        .then_with(|| a.feature.user_id.cmp(&b.feature.user_id))
                        .then_with(|| a.row_fingerprint.cmp(&b.row_fingerprint))
                });
                Ok(SortedRows::InMemory(in_memory))
            }
            Some(linear) => {
                let mut spill = BucketSpill::new(binding_id.clone(), scratch_dir, scratch_budget_bytes);
                linear.drain(|mut row| {
                    row.key = assign_key(&row);
                    spill.append(&row)
                })?;
                let reader = spill.into_reader()?;
                Ok(SortedRows::Spilled(reader))
            }
        }
    }
}

fn check_budget(binding: &str, bytes: u64, budget: u64) -> Result<(), SpillError> {
    if bytes > budget {
        return Err(SpillError::BudgetExceeded {
            binding: binding.to_owned(),
            observed_bytes: bytes,
            budget_bytes: budget,
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_artifact::{FeatureGeom, GeomKind};

    fn point_row(key: u64, user_id: u64, fp: u64) -> KeyedRow {
        KeyedRow {
            feature: FeatureGeom {
                user_id,
                bbox: [1.0, 2.0, 3.0, 4.0],
                geom: GeomKind::Point((10.0, 20.0)),
            },
            attrs: Arc::new(vec![
                ("name".into(), AttrValue::String("alpha".into())),
                ("count".into(), AttrValue::Int(-7)),
                ("ratio".into(), AttrValue::Float(1.5)),
                ("flag".into(), AttrValue::Bool(true)),
                ("opt".into(), AttrValue::Null),
            ]),
            geom_bytes_estimate: 42,
            key: HilbertKey::new(key),
            row_fingerprint: fp,
        }
    }

    fn polygon_row() -> KeyedRow {
        KeyedRow {
            feature: FeatureGeom {
                user_id: 99,
                bbox: [0.0, 0.0, 10.0, 10.0],
                geom: GeomKind::Polygon(vec![vec![
                    (0.0, 0.0),
                    (10.0, 0.0),
                    (10.0, 10.0),
                    (0.0, 10.0),
                    (0.0, 0.0),
                ]]),
            },
            attrs: Arc::new(vec![]),
            geom_bytes_estimate: 200,
            key: HilbertKey::new(0xDEAD_BEEF),
            row_fingerprint: 0xCAFE_BABE,
        }
    }

    #[test]
    fn roundtrip_point_with_mixed_attrs() {
        let row = point_row(0xAA, 7, 0x55);
        let mut buf = Vec::new();
        encode_one(&row, &mut buf).unwrap();
        let mut pos = 0usize;
        let decoded = decode_one(&buf, &mut pos).unwrap();
        assert_eq!(pos, buf.len());
        assert_eq!(decoded.key, row.key);
        assert_eq!(decoded.feature.user_id, row.feature.user_id);
        assert_eq!(decoded.feature.bbox, row.feature.bbox);
        assert_eq!(decoded.row_fingerprint, row.row_fingerprint);
        assert_eq!(decoded.geom_bytes_estimate, row.geom_bytes_estimate);
        match decoded.feature.geom {
            GeomKind::Point((x, y)) => {
                assert!((x - 10.0).abs() < 1e-6);
                assert!((y - 20.0).abs() < 1e-6);
            }
            _ => panic!("expected point"),
        }
        assert_eq!(decoded.attrs.len(), 5);
        assert_eq!(decoded.attrs[0].0, "name");
        assert!(matches!(decoded.attrs[0].1, AttrValue::String(ref s) if s == "alpha"));
        assert!(matches!(decoded.attrs[1].1, AttrValue::Int(-7)));
        assert!(matches!(decoded.attrs[2].1, AttrValue::Float(f) if (f - 1.5).abs() < 1e-12));
        assert!(matches!(decoded.attrs[3].1, AttrValue::Bool(true)));
        assert!(matches!(decoded.attrs[4].1, AttrValue::Null));
    }

    #[test]
    fn roundtrip_polygon_no_attrs() {
        let row = polygon_row();
        let mut buf = Vec::new();
        encode_one(&row, &mut buf).unwrap();
        let mut pos = 0usize;
        let decoded = decode_one(&buf, &mut pos).unwrap();
        assert_eq!(pos, buf.len());
        assert!(matches!(decoded.feature.geom, GeomKind::Polygon(_)));
        assert_eq!(decoded.attrs.len(), 0);
    }

    #[test]
    fn streams_concatenate() {
        let a = point_row(1, 1, 1);
        let b = polygon_row();
        let mut buf = Vec::new();
        encode_one(&a, &mut buf).unwrap();
        encode_one(&b, &mut buf).unwrap();
        let mut pos = 0usize;
        let da = decode_one(&buf, &mut pos).unwrap();
        let db = decode_one(&buf, &mut pos).unwrap();
        assert_eq!(pos, buf.len());
        assert_eq!(da.key, a.key);
        assert_eq!(db.key, b.key);
    }

    #[test]
    fn truncated_frame_errors() {
        let row = point_row(0, 0, 0);
        let mut buf = Vec::new();
        encode_one(&row, &mut buf).unwrap();
        buf.truncate(buf.len() - 4);
        let mut pos = 0usize;
        let err = decode_one(&buf, &mut pos);
        assert!(matches!(err, Err(SpillError::Malformed(_))));
    }

    #[test]
    fn unknown_attr_tag_errors() {
        let row = point_row(0, 0, 0);
        let mut buf = Vec::new();
        encode_one(&row, &mut buf).unwrap();
        // The attr-count u32 sits right after the geom payload; we have
        // 5 attrs and the first attr tag is the `name` tag for ATTR_STRING.
        // Walk to the first tag byte and corrupt it.
        let mut pos = 0usize;
        let _ = decode_one(&buf, &mut pos).unwrap();
        // re-decode with the attr_count's first attr tag corrupted.
        // simplest: scan for the ATTR_STRING tag (0x04) preceded by a 4-byte
        // u32 length of "name" (4) and replace.
        let needle = [4u8, 0, 0, 0, b'n', b'a', b'm', b'e', ATTR_STRING];
        let idx = buf.windows(needle.len()).position(|w| w == needle).unwrap();
        buf[idx + needle.len() - 1] = 0xFF;
        let mut pos = 0usize;
        let err = decode_one(&buf, &mut pos);
        assert!(matches!(err, Err(SpillError::Malformed(_))));
    }

    fn key_in_bucket(bucket: usize) -> HilbertKey {
        let shift = 64 - u32::from(SPILL_BUCKET_BITS);
        HilbertKey::new((bucket as u64) << shift)
    }

    #[test]
    fn bucket_for_partitions_keys_by_top_bits() {
        let shift = 64 - u32::from(SPILL_BUCKET_BITS);
        for b in [0, 1, 7, SPILL_BUCKET_COUNT / 2, SPILL_BUCKET_COUNT - 1] {
            // any key whose top bits are b lands in bucket b regardless of
            // the lower-order bits.
            let lo = HilbertKey::new(((b as u64) << shift) | 1);
            let hi = HilbertKey::new(((b as u64) << shift) | (1 << (shift - 1)) | 0xFF);
            assert_eq!(bucket_for(lo), b);
            assert_eq!(bucket_for(hi), b);
        }
    }

    #[test]
    fn bucket_spill_roundtrip_orders_within_bucket() {
        let scratch = tempfile::TempDir::new().unwrap();
        let mut spill = BucketSpill::new("binding-x".into(), scratch.path().to_path_buf(), 1 << 30);

        // three rows in bucket 0 (with deliberately scrambled tiebreak), one
        // in bucket SPILL_BUCKET_COUNT-1.
        let mut rows = vec![
            KeyedRow {
                feature: FeatureGeom {
                    user_id: 5,
                    bbox: [0.0, 0.0, 0.0, 0.0],
                    geom: GeomKind::Point((1.0, 1.0)),
                },
                attrs: Arc::new(vec![]),
                geom_bytes_estimate: 0,
                key: key_in_bucket(0),
                row_fingerprint: 99,
            },
            KeyedRow {
                feature: FeatureGeom {
                    user_id: 5,
                    bbox: [0.0, 0.0, 0.0, 0.0],
                    geom: GeomKind::Point((1.0, 1.0)),
                },
                attrs: Arc::new(vec![]),
                geom_bytes_estimate: 0,
                key: key_in_bucket(0),
                row_fingerprint: 1,
            },
            KeyedRow {
                feature: FeatureGeom {
                    user_id: 1,
                    bbox: [0.0, 0.0, 0.0, 0.0],
                    geom: GeomKind::Point((1.0, 1.0)),
                },
                attrs: Arc::new(vec![]),
                geom_bytes_estimate: 0,
                key: key_in_bucket(0),
                row_fingerprint: 50,
            },
            KeyedRow {
                feature: FeatureGeom {
                    user_id: 7,
                    bbox: [0.0, 0.0, 0.0, 0.0],
                    geom: GeomKind::Point((1.0, 1.0)),
                },
                attrs: Arc::new(vec![]),
                geom_bytes_estimate: 0,
                key: key_in_bucket(SPILL_BUCKET_COUNT - 1),
                row_fingerprint: 0,
            },
        ];
        for r in &rows {
            spill.append(r).unwrap();
        }
        assert_eq!(spill.populated_buckets(), 2);
        assert!(spill.total_bytes() > 0);

        let reader = spill.into_reader().unwrap();
        // bucket 0 yields three rows, sorted by (user_id, row_fingerprint)
        // since all keys are equal.
        let bucket0 = reader.take_bucket(0).unwrap();
        assert_eq!(bucket0.len(), 3);
        let tiebreak: Vec<(u64, u64)> = bucket0.iter().map(|r| (r.feature.user_id, r.row_fingerprint)).collect();
        assert_eq!(tiebreak, vec![(1, 50), (5, 1), (5, 99)]);
        let mid = reader.take_bucket(SPILL_BUCKET_COUNT / 2).unwrap();
        assert!(mid.is_empty());
        let last = reader.take_bucket(SPILL_BUCKET_COUNT - 1).unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].feature.user_id, 7);

        // sanity: dropping the reader removes the temp files.
        let temp_root = scratch.path().to_path_buf();
        drop(reader);
        let leftover: Vec<_> = std::fs::read_dir(&temp_root).unwrap().flatten().collect();
        assert!(leftover.is_empty(), "spill files leaked: {leftover:?}");
        // hold rows so any Arc<...> leaks would surface in miri-style runs.
        rows.clear();
    }

    #[test]
    fn bucket_spill_budget_exceeded() {
        let scratch = tempfile::TempDir::new().unwrap();
        let mut spill = BucketSpill::new("tight".into(), scratch.path().to_path_buf(), 64);
        let row = point_row(0, 0, 0);
        // first row alone exceeds 64 bytes (header + geom + attrs > 64).
        let err = spill.append(&row).unwrap_err();
        match err {
            SpillError::BudgetExceeded {
                binding,
                observed_bytes,
                budget_bytes,
            } => {
                assert_eq!(binding, "tight");
                assert!(observed_bytes > budget_bytes);
                assert_eq!(budget_bytes, 64);
            }
            other => panic!("expected BudgetExceeded, got {other:?}"),
        }
    }
}
