# Changelog

All notable changes to MARS are recorded in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project will adopt [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it starts tagging releases.

## [Unreleased]

### Changed

- **Manifest format bumped from v3 to v4** (breaking on-disk change).
  `LevelMetadata.hilbert_range_table` widens from
  `Vec<(HilbertKey, HilbertKey)>` to `Vec<(HilbertKey, HilbertKey, PageId)>`
  so the change-feed path reads the persisted `PageId` directly instead of
  reconstructing one from the table position. Rebalance allocates fresh
  page ids and the prior reconstruction landed dirties on the wrong page.
  Existing v3 manifests are no longer readable; bootstrap a fresh manifest
  after upgrade.
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
