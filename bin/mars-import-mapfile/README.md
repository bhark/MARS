# mars-import-mapfile

Translates a MapServer mapfile to a MARS YAML service config.

The translator is **opinionated** and covers the subset of mapfile
constructs the forvaltning2 diff harness fixture exercises. Anything
outside that subset is logged as a warning and either skipped or emitted
as a `# TODO: hand-translate` placeholder.

Pure synchronous binary; no `tokio`, no DB.

## Usage

```sh
cargo run -p mars-import-mapfile --locked -- \
  [--include-layer NAME ...] \
  [--output PATH] \
  [--strict] \
  <mapfile>
```

### Flags

- `--include-layer NAME` (repeatable): keep only LAYERs whose `NAME`
  matches one of these values (case-insensitive). Defaults to all
  layers. Use this to narrow a large upstream mapfile down to the
  subset under harness coverage.
- `--output PATH`: write to `PATH` instead of stdout.
- `--strict`: exit with code 2 if any warning was emitted. Useful for
  CI gating: only known unsupported constructs (METADATA, OUTPUTFORMAT,
  PROJECTION, FONTSET, LEGEND) should pass.

### Coverage

| Mapfile construct | Behaviour |
|---|---|
| `MAP { ... }` | scanned recursively |
| `INCLUDE` | resolved during scan; cycles detected |
| `LAYER NAME / TITLE / TYPE / DATA` | mapped to MARS layer fields |
| `LAYER MINSCALEDENOM / MAXSCALEDENOM` | mapped to source `max_denom_exclusive` |
| `LAYER PROCESSING "ITEMS=..."` | source `id_column` (heuristic: `ogc_fid` / `id` / `*_fid`) |
| `SCALETOKEN VALUES { ... }` | expanded to denom-keyed sources, one per bucket |
| `CLASS NAME / TITLE / EXPRESSION / STYLE` | mapped to MARS class with `when:` and `style: ref` |
| `STYLE COLOR / OUTLINECOLOR / WIDTH / OUTLINEWIDTH / PATTERN` | mapped to MARS Style; multi-pass STYLE collapses with a warning |
| `LABEL` | mapped to MARS label (font/colour/size subset) |
| `METADATA / LEGEND / OUTPUTFORMAT / PROJECTION / FONTSET / SYMBOL / FEATURE / JOIN / COMPOSITE / CLUSTER / GRID / VALIDATION` | warned, skipped |

### Expression operators

`mars-expr` AST target. Supported: `=`, `<>`, `IN`, `NOT IN`, `AND`,
`OR`, `NOT`, attribute `[name]` quoting, single/double-quoted string
literals, integer/float literals, mapfile bareword strings (`12-`,
`2.5-12`). Anything else (regex, math, function calls) emits a typed
`Unsupported` error and a `# TODO: hand-translate` placeholder.

CLASSITEM-driven CLASS NAME / EXPRESSION (where the class matches
against a layer-level CLASSITEM rather than naming a column) is **not**
modelled; expressions of this shape come through as bare literals
(e.g. `when: "'12-'"`) and need hand reconciliation.

## Operator runbook: forvaltning2 fixture

The `bin/mars/tests/fixtures/forvaltning2/service.yaml` is a curated
subset of the upstream mapfile. Operator workflow when the upstream
moves:

1. Update the ref pointer in `mapfile-source.md`.
2. Run the regeneration script:
   ```sh
   MARS_FORVALTNING2_MAPFILE=/path/to/wms.map \
     ./scripts/regenerate-forvaltning2-fixture.sh
   ```
   This invokes the importer with the six `--include-layer` flags
   matching the harness layers (Landpolygon, Soe, Byomraade, Vejmidte,
   Bygning, Vandloebsmidte) and prints a unified diff against the
   committed fixture.
3. Reconcile the diff against
   `bin/mars/tests/fixtures/forvaltning2/HANDSTRIP.md`. New deltas
   inside the strip set are expected; deltas outside it indicate
   either an upstream schema change (table/column rename, new class)
   or an importer bug.
4. Re-run the diff capture against MapServer to validate the parity
   budget still holds; only then commit the updated fixture.

The mapfile lives in a separate, operator-local repository and is not
vendored here; this CI check is operator-side, not GitHub Actions. The
script requires `MARS_FORVALTNING2_MAPFILE` (or `--mapfile PATH`).

## Architecture

- `scanner.rs` - line-based tokeniser; honours quoted strings, strips
  comments outside strings, recursively resolves INCLUDE.
- `expression.rs` - mapfile EXPRESSION lexer/parser, lowering to
  `mars-expr` AST.
- `emitter.rs` - YAML rendering. Bands are derived from the union of
  source `max_denom_exclusive` breakpoints across all kept layers.
- `main.rs` - CLI, walk, and per-construct mapping.

This binary is a composition root and may freely depend on adapter and
support crates; it does not link `tokio` or any I/O port.
