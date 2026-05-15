# osm

Fixture for the OSM parity scenario (`tests/parity/tests/osm.rs`). Verifies
MARS renders an OpenStreetMap-derived Liechtenstein subset within tolerance of
an external reference renderer's output.

## Contents

- `seed.sql` - prelude executed before the dump is restored. Installs PostGIS
  in the `public` schema.
- `restore.sh` - shell-side restore for the postgres init container. Strips the
  redundant `CREATE SCHEMA public;` from the dump (the fresh init container
  already owns one), then pipes the gunzipped dump into psql.
- `service.yaml` - MARS service config. Single broad scale band; per-layer
  `scale:` windows reproduce the reference renderer's `MAXSCALEDENOM` gates.
  Placeholders `{{DSN}}` / `{{STORE}}` / `{{CACHE}}` substituted at runtime.
- `goldens/*.png` (and `*.jpg`) - one reference image per case in the matrix.
  Filenames match the `Case::name` in `tests/parity/tests/osm.rs`.

## Hosting

The fixture is a `pg_dump`-format snapshot of a Liechtenstein OSM extract
loaded via `osm2pgsql` (~9-10 MiB compressed). It is published as an asset
on a GitHub Release of `bhark/MARS`; the canonical URL and sha256 are pinned
in `manifest.sha256` beside this file.

`scripts/run-parity.sh` and `tests/parity/scripts/fetch-fixture.sh` both
read that manifest and stage the dump at:

```
target/parity-fixtures/osm-parity.sql.gz
```

Set `MARS_PARITY_FIXTURE_URL` to override the source (forks / mirrors /
air-gapped dev). Set `MARS_PARITY_FIXTURE_PATH` to skip the fetch and use a
local dump in-place. Maintainers cut new fixture versions via
`scripts/release-fixtures.sh`.

The asset contains data (C) OpenStreetMap contributors and is distributed
under the Open Database License (ODbL). See the GitHub Release notes for
the full attribution text.

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
differing-pixel ratio. See `tests/parity/tests/osm.rs` for the full matrix.

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
