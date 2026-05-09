# Changelog

All notable changes to MARS are recorded in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project will adopt [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it starts tagging releases.

## [Unreleased]

### Changed

- Pass-1 page boundary cutting now sizes rows by
  `octet_length(ST_AsBinary(geom))` (raw WKB bytes server-side) instead of an
  estimate derived from decoded and simplified geometry size. The new metric is
  cheaper to compute and avoids hydrating geometries during pass 1.

### Operator notes

- The pass-1 sizing change above shifts page boundaries even on otherwise
  unchanged source data. The first bootstrap after upgrading therefore
  republishes most pages for one cycle. The shift is one-shot and
  self-correcting: subsequent bootstraps and incremental cycles are unaffected.
  Plan accordingly when scheduling the upgrade.

### Known limits

- Bootstrap pass-1 still buffers row summaries in memory, bounded by
  `compile.plan_budget_bytes`. A fixed-record spill primitive is designed but
  not yet implemented; until it lands, very large bootstraps that exceed the
  configured budget surface as `BootstrapPlanTooLarge` and require either
  raising the budget or partitioning the source.
