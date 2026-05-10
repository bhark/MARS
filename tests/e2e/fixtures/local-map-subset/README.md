# local map subset fixture

This fixture contract is the public e2e data boundary. The harness expects a
gzip-compressed SQL dump at `target/e2e-fixtures/local-map-subset.sql.gz` unless
`scripts/run-k3d-e2e.sh --fixture PATH` is used.

The dump must create the `e2e_source` schema and these tables:

| table | geometry | id | attributes |
| --- | --- | --- | --- |
| `land` | `geom` polygon/multipolygon, EPSG:25832 | `id` | none |
| `water` | `geom` polygon/multipolygon, EPSG:25832 | `id` | none |
| `settlements` | `geom` polygon/multipolygon, EPSG:25832 | `id` | none |
| `roads` | `geom` line/multiline, EPSG:25832 | `id` | `kind` |
| `buildings` | `geom` polygon/multipolygon, EPSG:25832 | `id` | `kind`, `status` |
| `waterways` | `geom` line/multiline, EPSG:25832 | `id` | `width_class` |

Rows should intersect the default test bbox `850000,6090000,895000,6145000` in
EPSG:25832. Fixture generation is intentionally outside the public harness.
