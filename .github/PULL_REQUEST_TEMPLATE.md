<!--
Subject of this PR's commits should be conventional:
  <type>(<scope>): <short imperative>
See CONTRIBUTING.md for the full rule set.
-->

## What

<!-- one paragraph: what does this change, and why now -->

## How

<!-- key implementation notes: new types, ports lifted, adapters touched -->

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked --all-targets`
- [ ] `./scripts/check-hexagonal-architecture.sh`
- [ ] integration (`MARS_INTEGRATION=1 cargo test -p mars --features integration` or `./scripts/run-integration.sh`) where relevant

## Notes for reviewers

<!-- anything reviewers should pay extra attention to: tradeoffs, follow-ups, gotchas -->
