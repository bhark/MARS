# MARS artifact format v1

## File layout
- header: 8B magic "MARS\0\0\0\0", u32 LE format_version (1), u32 LE flags (0)
- sections, each: u16 LE kind, u16 LE flags (bit 0 = compressed; v1 always 0), u64 LE length, length payload bytes
- footer: FlatBuffers (planus) blob describing artifact kind, section index (kind, file_offset, length, uncompressed_length), bbox, feature_count, source_artifact_ref for layer artifacts
- u32 LE footer_length, 8B trailer "MARS\0\0\0\0"

All multi-byte integers are little-endian.

## Section kinds (existing enum — extend if needed)
- 0x01 GeometryIndex (deferred to Phase 1)
- 0x02 GeometryPayload
- 0x03 Attributes (deferred to Phase 1)
- 0x04 LabelCandidates (deferred to Phase 2)
- 0x05 ClassAssignment
- 0x06 StyleRefs

## Geometry payload v1
Endian: little-endian. All deltas zigzag-varint. Coordinates quantized to integer millimetres ((x * 1000.0).round() as i64).

Per feature, in ascending feature_id order:
- u64 feature_id
- [f32; 4] bbox in canonical CRS units (NOT mm)
- u8 geom_type: 1=Point, 2=LineString, 3=Polygon, 4=MultiPoint, 5=MultiLineString, 6=MultiPolygon (matches WKB)
- u32 coord_block_offset
- u32 coord_block_length

The packed coord array follows the feature index. For each feature's coord block:
- Point: one absolute (x_mm, y_mm) i64 pair (varint i64 zigzag, but Point is a single absolute pair without delta)
- LineString: varint vertex_count; first vertex absolute (i64 x_mm, i64 y_mm); subsequent (dx, dy) zigzag-varints
- Polygon: varint ring_count; outer first then holes; per ring same as LineString
- Multi*: varint part_count; per part the singular layout
- Empty: vertex_count = 0 / ring_count = 0 / part_count = 0 — permitted; rendered as no-op

Determinism: features must be written in ascending feature_id order. Two writes with identical input must produce byte-identical artifacts.

## class_assignment section (layer artifacts)
[(u64 feature_id, u16 class_index)] sorted ascending by feature_id.

## style_refs section (layer artifacts)
u32 count, then `count` entries each as (u32 length, length UTF-8 bytes) — style_id strings, indexed by class_index.

## Content hash
ContentHash = BLAKE3 of the entire file bytes. Used as the store key. The footer carries no self-hash.
