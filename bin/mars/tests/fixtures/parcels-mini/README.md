# parcels-mini

Tiny synthetic fixture for the image-diff harness (`tests/image_diff_harness.rs`).

## Contents

- `seed.sql` - one PostGIS table `mars_diff.parcels` with seven polygons in three
  classes (`park`, `road`, `water`), all in `EPSG:25832`, fitting cell (0,0).
- `service.yaml` - minimal MARS service config with three style classes. Three
  placeholders (`{{DSN}}`, `{{STORE}}`, `{{CACHE}}`) are substituted by the
  harness at runtime.
- `goldens/` - reference PNGs the harness compares against, one per case in the
  matrix. Filenames match `Case::name` in the harness.

## The matrix

Each `Case` declares a render plan, a per-channel pixel tolerance, and a
maximum differing-pixel ratio. Cases share one container, one compile, and one
runtime - adding a case is one entry and one regenerated golden.

Current cases:

| name                     | bbox (25832)            | dims    | tolerance | max ratio |
|--------------------------|-------------------------|---------|-----------|-----------|
| `parcels-cell-0-0`       | (0,0)-(1023,1023)       | 512×512 | 2         | 0.005     |
| `parcels-quadrant-sw`    | (0,0)-(500,500)         | 256×256 | 2         | 0.005     |
| `parcels-quadrant-ne`    | (500,500)-(1000,1000)   | 256×256 | 2         | 0.005     |

## Regenerating goldens

The harness is gated on the `integration` cargo feature (it spins up a postgis
testcontainer). To regenerate every golden in one pass:

```sh
MARS_GOLDEN_REGENERATE=1 cargo test -p mars --features integration \
    --test image_diff_harness -- --nocapture
```

This writes one PNG per case to `goldens/` instead of comparing. Then run again
without the env var to verify the comparison passes, and commit the updated
PNG(s).

Tighten per-case tolerances in `image_diff_harness.rs` only after re-running
across platforms; tiny-skia anti-alias output is not bit-identical between
minor versions.
