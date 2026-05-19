# mars-fuzz

cargo-fuzz harnesses for MARS. Excluded from the workspace; targets must be
run with nightly + `cargo-fuzz` installed.

## one-time setup

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## running a target

```sh
# from the repo root
cargo +nightly fuzz run fuzz_target_artifact_reader
cargo +nightly fuzz run fuzz_target_expr_parser
cargo +nightly fuzz run fuzz_target_text_template
```

Corpus and crash artifacts land under `fuzz/corpus/<target>/` and
`fuzz/artifacts/<target>/` respectively; both directories are gitignored.

## targets

- `fuzz_target_artifact_reader` - feeds arbitrary byte sequences to
  `mars_artifact::ArtifactReader::open`. Any panic is a bug.
- `fuzz_target_expr_parser` - feeds arbitrary UTF-8 strings to
  `mars_expr::parse`. Any panic is a bug.
- `fuzz_target_text_template` - feeds arbitrary UTF-8 strings to
  `mars_expr::parse_template` (the `${expr}` interpolation tokenizer used by
  the text template engine). Any panic is a bug.
