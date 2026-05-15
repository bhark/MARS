# e2e fixture (osm)

The kind-based e2e suite shares the parity OSM dump rather than carrying a
second public dataset. The dump is a `pg_dump` of `osm2pgsql`'s
`planet_osm_*` tables built from the Liechtenstein OpenStreetMap extract;
sha + URL are pinned in `tests/parity/fixtures/osm/manifest.sha256` and the
asset itself is published as a GitHub Release of `bhark/MARS` (data
(C) OpenStreetMap contributors, ODbL licensed - see the Release notes).

The e2e harness:

1. Fetches `osm-parity.sql.gz` to `target/e2e-fixtures/` via
   `tests/e2e/scripts/fetch-fixture.sh` (reads the parity manifest;
   override with `MARS_E2E_FIXTURE_URL` for forks/mirrors,
   `MARS_E2E_FIXTURE_PATH` to bypass the fetch entirely).
2. Restores the dump into the in-cluster postgis.
3. Runs `derive-e2e.sql` (this directory) to materialise the `e2e_source`
   schema on top of `planet_osm_*`, reprojecting geometries to EPSG:25832.
4. Runs `assert-fixture.sql` and `create-replication.sql`, then layers the
   synthetic POI + pattern_zone tables from `tests/e2e/sql/`.

## Schema (produced by `derive-e2e.sql`)

| table | geometry | id | attributes |
| --- | --- | --- | --- |
| `land` | `geom` polygon/multipolygon, EPSG:25832 | `id` | none |
| `water` | `geom` polygon/multipolygon, EPSG:25832 | `id` | none |
| `settlements` | `geom` polygon/multipolygon, EPSG:25832 | `id` | none |
| `roads` | `geom` line/multiline, EPSG:25832 | `id` | `kind` (major/minor) |
| `buildings` | `geom` polygon/multipolygon, EPSG:25832 | `id` | `kind` (raw OSM `building` tag), `status` (synthetic; `temporary` for `id % 50 = 0`, else `permanent`) |
| `waterways` | `geom` line/multiline, EPSG:25832 | `id` | `width_class` (`wide` for river/canal, else `narrow`) |

`status` is synthetic because OSM has no equivalent attribute; the modulus
keeps the `status='temporary'` class filter matching at least some rows for
any reasonable OSM extract size.

## Default test bbox

`[536000, 5210000, 548000, 5235000]` in EPSG:25832 (the populated extent of
the Liechtenstein extract). The full data extent is
`[535000, 5201000, 551000, 5264000]` and is what `service.yaml` declares as
the cell-grid extent.
