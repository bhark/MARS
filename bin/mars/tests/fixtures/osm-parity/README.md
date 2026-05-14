# osm-parity

Image-diff fixture for `tests/osm_parity_harness.rs`. Verifies MARS renders an
OpenStreetMap-derived Liechtenstein subset within tolerance of a reference
renderer's output.

## Contents

- `seed.sql` - prelude executed before the dump is restored. Installs PostGIS
  in the `public` schema.
- `restore.sh` - shell-side restore for the postgres init container. Strips the
  redundant `CREATE SCHEMA public;` from the dump (the fresh init container
  already owns one), then pipes the gunzipped dump into psql.
- `service.yaml` - MARS service config. Single broad scale band; per-layer
  `scale:` windows reproduce the reference renderer's `MAXSCALEDENOM` gates.
  Placeholders `{{DSN}}` / `{{STORE}}` / `{{CACHE}}` substituted at runtime.
- `goldens/*.png` (and `*.jpg`) - one reference image per case in the harness
  matrix. Filenames match the `Case::name` in `osm_parity_harness.rs`.

## Required fixture (not committed)

The fixture is a `pg_dump`-format snapshot of a Liechtenstein OSM extract
loaded via `osm2pgsql`. The harness expects to find it at:

```
target/parity-fixtures/osm-parity.sql.gz
```

The dump is 9-10 MiB compressed and is intentionally out of git. Operators
producing fresh goldens should reproduce the snapshot offline; the goldens in
this directory are the canonical reference and do not need regenerating in the
common case.

## Goldens

Goldens were captured one-shot from MapServer rendering the same Liechtenstein
OSM extract that backs the test fixture. MapServer is intentionally stable, so
frozen one-shot goldens are an appropriate reference for parity testing -
there is no automated regeneration workflow in this repo. If a future change
demands new goldens, capture them from any reference WMS that matches the
layer definitions in `service.yaml`.

## Cases

The harness drives a sequential matrix against a single shared runtime. Each
case declares a render plan, a per-channel pixel tolerance, and a max
differing-pixel ratio. See `osm_parity_harness.rs` for the full matrix.

The matrix spans:

- Three zoom tiers: overview (denom ~110k), mid (~13k), detail (~2.6k).
- Three anchors: Vaduz (urban), Schaan (village), eastern rural.
- One reprojection: EPSG:3857 native vs EPSG:25832 (UTM 32N) reprojected.
- One size variant: 512x512 vs 1024x1024.
- One JPEG variant: PNG vs JPEG (relaxed tolerance for chroma drift).
- Four layer-isolation cases: roads-only, landuse-only, buildings-only,
  boundary-only. Bisects geometric parity per layer.
- Empty-bbox sanity: an off-extent bbox; both renderers should emit the
  background page colour.
