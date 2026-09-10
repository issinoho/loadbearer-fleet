## What changed and why

<!-- A short description. Link a related issue if there is one (e.g. "Fixes #123"). -->

## How was this tested?

<!--
- The four checks, or just "all pass locally" if nothing unusual.
- If this touches the analysis: which rule, and what the fixture proves. Tests
  here build their fixtures from real result documents on purpose.
- If this touches authorization: say so explicitly. Those tests go through the
  real router with a real session cookie, because a unit test of the filter
  proves the projection is right and only a request proves it is the one a
  request gets.
- If this touches a chart or a view: `node scripts/check-ui.mjs`, and whether
  you looked at the page. Both visual bugs found so far were invisible to the
  code and obvious on screen.
- If this touches the service or the config: which platform you ran it on.
  `cfg(windows)` and `cfg(not(windows))` mean each build only compiles one of
  the two.
-->

## Checklist

- [ ] `cargo fmt --all --check` is clean
- [ ] `cargo clippy --all-targets --locked -- -D warnings` is clean
- [ ] `cargo test --locked` passes
- [ ] `node scripts/check-ui.mjs` passes, if anything under `assets/` changed
- [ ] Still builds on the MSRV (1.88) if you touched dependencies
- [ ] Docs updated if this changes user-facing behaviour — and if it changes
      something the README asserts, the README changed too

<!--
No need to touch the version in Cargo.toml or CHANGELOG.md — that is a
maintainer step at release time.
-->
