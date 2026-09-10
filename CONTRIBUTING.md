# Contributing to loadbearer-fleet

Thanks for considering a contribution. This is a solo-maintained project, so
response times are best-effort — but bug reports, small fixes and well-scoped
features are genuinely welcome.

This file is about contributing to the *code*. For how to run it, see the
[README](README.md). For the tool that produces the results this reads, see
[loadbearer](https://github.com/issinoho/loadbearer).

By participating you're expected to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Getting started

```
git clone https://github.com/issinoho/loadbearer-fleet.git
cd loadbearer-fleet
cargo test
cargo run -- --index dev.db scan tests/fixtures
cargo run -- --index dev.db serve tests/fixtures
```

Then open <http://127.0.0.1:8787>. `tests/fixtures` holds four real result
documents with the identifiers replaced, which is enough to exercise every
view — though not enough for a comparable cohort, since a peer group needs four
machines of the *same* hardware.

Rust 1.88 or newer, plus a C compiler for the bundled SQLite
(`build-essential`, or the MSVC build tools). Node is needed only for the
dashboard check below.

## The checks

Every push and pull request runs, on **Linux and Windows**:

```
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --locked && node scripts/check-ui.mjs
```

Plus `cargo check --all-targets --locked` on Rust **1.88**, the declared MSRV,
also on both platforms.

All of it must pass. Some notes on why it's shaped like that:

- **Both platforms matter for more than the tests.** The Windows service
  integration is behind `cfg(windows)` and the fallbacks that replace it are
  behind `cfg(not(windows))`, so either build only ever compiles one of the
  two. Whichever platform you skip is the one that breaks.
- **The MSRV job is separate and uses `check`, not `clippy`,** because clippy's
  lint set moves between releases and a new lint would fail it for a reason
  unrelated to whether 1.88 compiles.
- **`--locked` is deliberate.** If you change dependencies, commit the updated
  `Cargo.lock`.
- **`node scripts/check-ui.mjs`** renders every chart and every view against a
  minimal DOM and asserts the geometry — no `NaN` reaching an SVG attribute
  (which silently drops a mark rather than erroring), nothing painted outside
  its own box, bars within spec, markers carrying their surface ring, hit
  targets big enough to hit, axis labels that fit their band and never repeat.
  It builds its snapshot by running the binary over the fixtures, so it can't
  drift from the API.

Match the style of the surrounding code — comment density, naming, idiom.
Comments should explain *why*, especially for a platform quirk or a deliberate
trade-off, not restate what the code already says.

## Project layout

| Path | What |
| --- | --- |
| `src/schema.rs` | reads `loadbearer.result/1`. Tolerant by design — unknown fields ignored, almost everything optional, open enumerations left as strings |
| `src/index.rs` | the SQLite index, ingest, the document archive, `backup`, `forget` |
| `src/analytics.rs` | cohorts, the red-flag engine, the executive summary, the filter projection |
| `src/web.rs` | the HTTP layer, the snapshot cache, authorization applied as part of the projection |
| `src/auth.rs` | OIDC sign-in, sessions, roles and tag scopes |
| `src/config.rs` | the config file, and the validation that refuses placeholders |
| `src/service.rs` | Windows service integration, and the systemd unit generator |
| `src/metrics.rs` | the Prometheus exposition |
| `assets/` | the dashboard — HTML, CSS, hand-rolled SVG charts, compiled into the binary with `include_str!` |
| `scripts/check-ui.mjs` | the chart and view geometry check |

## Things this project has opinions about

Worth reading before a change that touches them, because they're decisions
rather than accidents:

- **The folder is the source of truth; the index is derived.** With the
  qualification in `src/index.rs`'s module doc — a collector that overwrites
  one file per machine leaves the index holding history the folder no longer
  has, which is why `archive_dir` exists and why a superseded index is renamed
  rather than dropped.
- **The schema is the contract, not loadbearer's Rust API.** Don't link
  loadbearer as a dependency; parse the document. A new optional field is
  explicitly not a breaking change, so the reader must tolerate one.
- **Data quality is a first-class input.** loadbearer records when a run
  shouldn't be read at face value; ranking an estate while ignoring that
  produces confident nonsense.
- **Findings go in three queues** — the machine, the measurement, the data we
  hold — separated by what you'd do about them.
- **Authorization is enforced in the data layer**, never in the UI, and a
  caller's scope comes from their session and never from the query string.
- **Charts follow the Claude Code `dataviz` skill's specs**, and the colour work
  is *computed* rather than eyeballed — the palette is validated with that
  skill's script, and the reasoning is in the header comment of
  `assets/app.css`. Every chart has a table view, so no value is ever reachable
  only by hovering.

## Testing conventions

- Unit tests live in each module's `#[cfg(test)] mod tests`.
- **Analytics tests build their fixtures by mutating a real result document and
  pushing it through the real ingest path**, so they cover the schema reader and
  the SQL as well as the rules. Don't hand-write a synthetic result — a
  document claiming "scored 700, graded S" lets a rule pass here that would
  fail in the field.
- **Authorization is tested through the real router** with a real session
  cookie. A unit test of the filter proves the projection is right; only a
  request proves the projection is the one a request gets.
- `snapshot()` and the metrics renderer take the clock as a parameter, so
  anything about staleness or age is tested rather than tiptoed around.
- If you touch a chart or a view, run `node scripts/check-ui.mjs` — and if you
  can, look at the page. Both of the visual bugs found so far were invisible to
  the code and obvious on screen.

## Submitting a change

1. Fork and branch off `main`.
2. Make the change, with tests where the conventions above call for them.
3. Run all four checks locally.
4. Open a PR describing what changed and why, and reference any related issue
   (`Fixes #123`).

Leave `Cargo.toml`'s version and `CHANGELOG.md` alone — that happens at release
time, and the steps are in
[Releases](README.md#releases).

**If you add or change a command, a flag or a configuration key, don't write it
up by hand.** `src/reference.rs` builds the reference from the clap definition
and from `Config::starter()`, so the doc comment *is* the documentation. Two
tests hold that line: one fails if any command or argument has no help text,
and one fails if a configuration key never reaches `init-config` — which is
what the reference embeds. The wiki page is regenerated from the binary at
release time:

```
loadbearer-fleet reference > Command-Line-and-Configuration.md
```

## Reporting bugs

Open a [GitHub issue](https://github.com/issinoho/loadbearer-fleet/issues/new/choose).
For anything about a number being wrong, the most useful thing you can attach
is the **result document** — with identifiers replaced if you'd rather — since
that is what the analysis actually reads.

For a security report, see [SECURITY.md](SECURITY.md) instead. Don't open a
public issue.
