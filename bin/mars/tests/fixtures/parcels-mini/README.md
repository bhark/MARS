# parcels-mini

Tiny synthetic fixture consumed by `tests/perf_harness.rs` for host-side
latency / throughput / GFI measurements.

## Contents

- `seed.sql` - one PostGIS table `mars_diff.parcels` with seven polygons in three
  classes (`park`, `road`, `water`), all in `EPSG:25832`, fitting cell (0,0).
- `service.yaml` - minimal MARS service config with three style classes. Three
  placeholders (`{{DSN}}`, `{{STORE}}`, `{{CACHE}}`) are substituted by the
  harness at runtime.
