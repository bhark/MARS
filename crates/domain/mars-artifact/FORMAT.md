# MARS artifact format v2

## File layout
- header: 8B magic "MARS\0\0\0\0", u32 LE format_version (2), u32 LE flags (0)
- sections, each: u16 LE kind, u16 LE flags (bit 0 = compressed; v2 always 0), u64 LE length, length payload bytes
- footer: FlatBuffers (planus) blob describing artifact kind, section index (kind, file_offset, length, uncompressed_length), bbox, feature_count, source_artifact_ref for layer artifacts
- u32 LE footer_length, 8B trailer "MARS\0\0\0\0"

All multi-byte integers are little-endian.

## Section kinds (existing enum - extend if needed)
- 0x02 GeometryPayload
- 0x03 Attributes
- 0x04 LabelCandidates
- 0x05 ClassAssignment
- 0x06 StyleRefs
- 0x07 SpatialIndex
- 0x08 ImageResources

### Forward-compatibility policy
Section kinds are additive. Adding a new `SectionKind` variant does NOT
require a `format_version` bump: the footer stores kinds as raw `u16`, and
readers iterate the footer's section index without enum-coercing the kinds.
A reader that does not know the new kind silently skips it; lookups by
typed `SectionKind` (`reader.section(...)`) target only kinds the reader's
enum names. `format_version` is reserved for wire-layout breakage (header,
section framing, footer schema).

## Substrate primary key
The per-page primary key for joining sidecars (attributes / class / label) to
geometry is `feature_idx`: the positional slot index of the feature in the
GeometryPayload, `0..n`. Source-supplied identifiers (`user_id`) live on the
geometry-index entry as data, not as a key, and are explicitly allowed to
repeat - for example when a source row is exploded into per-part features by
a simplification pipeline. Determinism is owned by the compiler: it sorts by
`(hilbert_key, user_id, row_fingerprint)` before encoding.

## Geometry payload v2
Endian: little-endian. All deltas zigzag-varint. Coordinates quantized to integer millimetres ((x * 1000.0).round() as i64).

Per feature, in compiler-supplied slot order:
- u64 user_id (non-key data; may repeat)
- [f32; 4] bbox in canonical CRS units (NOT mm)
- u8 geom_type: 1=Point, 2=LineString, 3=Polygon, 4=MultiPoint, 5=MultiLineString, 6=MultiPolygon (matches WKB)
- u32 coord_offset
- u32 coord_len

The packed coord array follows the feature index. For each feature's coord block:
- Point: one absolute (x_mm, y_mm) i64 pair (varint i64 zigzag, but Point is a single absolute pair without delta)
- LineString: varint vertex_count; first vertex absolute (i64 x_mm, i64 y_mm); subsequent (dx, dy) zigzag-varints
- Polygon: varint ring_count; outer first then holes; per ring same as LineString
- Multi*: varint part_count; per part the singular layout
- Empty: vertex_count = 0 / ring_count = 0 / part_count = 0 - permitted; rendered as no-op

Determinism: the encoder writes features in caller-supplied order (the
substrate primary key is the position itself). Two writes with identical
input must produce byte-identical artifacts.

## attributes section (source artifacts)
Header: `[magic "MARSATTR"][u32 version = 2][u32 count][u32 dir_offset]`.
Rows region: `count × [u32 row_len][row_bytes]`.
Directory region (at `dir_offset`, sorted ascending by feature_idx):
`count × [u32 feature_idx][u32 byte_offset]` - slot is the per-page primary
key; user_id is not used here.

### per-row attribute block
Little-endian, lengths as `u32`:

```
block  := count:u32, entry*
entry  := name_len:u32, name:utf8, tag:u8, payload
tag    := 0 Null | 1 Bool | 2 Int | 3 Float | 4 String
payload:
  Null    -> (none)
  Bool    -> u8 (0 | 1)
  Int     -> i64 LE
  Float   -> f64 LE (IEEE 754 bits)
  String  -> u32 len, utf8 bytes
```

A single row block is bounded at 64 KiB.

## class_assignment section (layer artifacts)
`u32 count`, then `count × [u32 feature_idx][u16 class_index]` sorted
ascending by feature_idx. Sparse: only slots that match a class appear.

## label_candidates section (layer artifacts)
See `label_candidates.rs` for the wire layout. Each entry carries a flags
byte; bit 1 (`HAS_SLOT`) marks slot-bearing labels (`u32 feature_idx`
follows). Slotless entries carry pruned-feature labels - features whose
geometry was filtered out at this level under the Independent survival
policy. The codec requires slotted entries to be ascending by feature_idx
and to precede any slotless entries.

## style_refs section (layer artifacts)
u32 count, then `count` entries each as (u32 length, length UTF-8 bytes) - style_id strings, indexed by class_index.

## image_resources section (image artifacts)
Bundles raster bitmap assets referenced by `FillPaint::Image { name }` styles so the runtime renderer can resolve `name -> bytes` without out-of-band coordination.

Wire format: u32 count, then `count` entries each as `(u16 name_len, name_len UTF-8 bytes, u32 image_byte_len, image_byte_len bytes)`. Names are non-empty, unique, and byte-sorted ascending so readers can `binary_search` by name in `O(log n)`. Image payload is the encoded image (PNG / JPEG / WebP); the decoder downstream sniffs the format from the bytes.

## Content hash
ContentHash = BLAKE3 of the entire file bytes. Used as the store key. The footer carries no self-hash.
