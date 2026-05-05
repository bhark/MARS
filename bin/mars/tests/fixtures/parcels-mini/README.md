# parcels-mini

Tiny synthetic fixture for the image-diff harness (`tests/image_diff_harness.rs`).

## Contents

- `seed.sql` — one PostGIS table `mars_diff.parcels` with seven polygons in three
  classes (`park`, `road`, `water`), all in `EPSG:25832`, fitting cell (0,0).
- `service.yaml` — minimal MARS service config with three style classes. Three
  placeholders (`{{DSN}}`, `{{STORE}}`, `{{CACHE}}`) are substituted by the
  harness at runtime.
- `goldens/` — reference PNGs the harness compares against.

## Regenerating goldens

The harness is gated on the `e2e` cargo feature (it spins up a postgis
testcontainer). To regenerate the golden:

```sh
MARS_GOLDEN_REGENERATE=1 cargo test -p mars --features e2e \
    --test image_diff_harness -- --nocapture
```

This writes the rendered PNG to `goldens/` instead of comparing. Then run again
without the env var to verify the comparison passes, and commit the updated
PNG(s).

Tighten the tolerances in `image_diff_harness.rs` only after re-running across
platforms; tiny-skia anti-alias output is not bit-identical between minor
versions.
