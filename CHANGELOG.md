# Changelog

All notable changes to loadbearer-fleet are documented in this file.

The release workflow extracts the section for a tag verbatim as that release's
notes, so each one has to stand on its own.

## 0.3.1 - Thu, 10 Sep 2026

**Security fix. Upgrade if anything other than you can write to your
collection folder** — which, in the deployment this is built for, means every
machine in the estate.

Nothing about the analysis, the index, the dashboard or the configuration
changed, so upgrading is replacing the binary. No index rebuild, no config
edit.

- **A result document is now refused above 16 MB, before it is read.** There
  was no limit on either ingest path: a whole file went into memory, and a
  gzipped one was decompressed with no bound. gzip reaches about 1030:1, so a
  398 KB `.json.gz` on the collection folder took peak memory to **523 MB**,
  and roughly 15 MB on the share would have exhausted 16 GB. The document was
  rejected afterwards — for having no `schema` field — which is exactly too
  late to be any use.

  The startup scan pays the same cost, so one file could have stopped the
  dashboard coming up at all. That is the failure mode this project describes
  as the one that hides every other one: a dashboard that never loads, or an
  index that never refreshes, looks like a quiet estate.

  Now checked twice — on the file's size on disk *before* it is read, and on
  what it decompresses to. The same file peaks at 43 MB and is reported as a
  compression bomb, with the rest of the scan carrying on. A real result is
  about 40 KB, so the limit leaves roughly 400× headroom and no legitimate
  document comes near it. Both the plain and gzipped paths were re-checked
  against real documents.
- **Half-finished sign-ins are capped at 1024.** `/auth/login` needs no
  session, and each call held a state, a nonce and a PKCE verifier for fifteen
  minutes with nothing bounding the total. Past the cap a *new* sign-in is
  refused with a 503 rather than an existing one being dropped — evicting to
  make room would have turned a memory bound into a way to deny somebody
  access halfway through signing in.
- **The post-sign-in redirect rejects control characters**, so `?next=` cannot
  smuggle anything past the existing `//` and `://` checks. Cosmetic in
  practice — axum returns a 500 rather than doing anything with it — but the
  check belongs there.

All three came with a test that was watched failing with its guard removed.
[SECURITY.md](https://github.com/issinoho/loadbearer-fleet/blob/main/SECURITY.md)
states both limits.

Found by auditing the codebase rather than from a report, and everything else
examined held up: authorization was re-verified live against an
OIDC-configured instance, and there is no string-built SQL, no HTML-injection
sink in the dashboard, and no `unsafe` anywhere in the crate.

## 0.3.0 - Thu, 10 Sep 2026

Answering "which build am I looking at", and making the documentation something
the binary generates rather than something anyone has to remember to update.
Nothing about the analysis, the index or the dashboard's numbers changed, so
upgrading is replacing the binary.

### It says which build it is

- **The version and the commit now appear everywhere they are asked for.**
  `--version` gives the long form, the first line of every run's log gives it
  structured, `report --json` and `/api/snapshot` carry it, `/metrics` labels
  `build_info` with it, and the dashboard shows it in the footer:

  ```
  loadbearer-fleet 0.3.0 (a1b2c3d4e 2026-09-10, x86_64-pc-windows-msvc, release)
  ```

  The version alone could never answer the question — every commit between two
  releases reports the same one, so a bug report naming `0.2.1` could be the
  release or a local build of anything on the way to this one. A `build.rs`
  stamps the commit, with `-dirty` when the tree was not clean.
- **The log says it on the first line of every run**, before any work, and
  carries the pid. A service log rotates daily and outlives upgrades, so
  without this a line from six weeks ago cannot be attributed to the build
  that wrote it, and a restart is indistinguishable from a reload. It is on
  every command, not just `serve`, so a `scan` from a scheduled task says it
  too.
- **The dashboard takes it from the snapshot**, not from the page, so it names
  the build that actually answered — including right after an upgrade, when a
  browser may still be holding cached HTML from the old one.
- `build_info` **gains a `build` label** alongside `version`. If you have a
  recording rule on that metric, it now has one more label; `version` is still
  a bare semver so anything comparing versions is unaffected.

### `reference`, and two settings that were never written down

- **`loadbearer-fleet reference`** prints the whole command-line and
  configuration reference as Markdown, generated from the definitions in the
  binary you are holding. It is also the
  [wiki page](https://github.com/issinoho/loadbearer-fleet/wiki/Command-Line-and-Configuration),
  which means the reference cannot describe a flag that does not exist. Writing
  those tables by hand would have been a second copy of `--help` and
  `init-config` with nothing checking the three agreed.
- **`init-config` now emits `scan_interval_minutes` and `extra_scopes`.** Both
  worked, and both appeared in no generated artefact at all —
  `scan_interval_minutes` being the setting this project's own README calls the
  difference between a service and a command. The starter file is what the
  release archive ships as `loadbearer-fleet.example.toml`, so they were
  missing there too.
- **Two `--allow-remote` flags were undocumented** — on `service run` and
  `service install`, which is to say on the flag that opts you out of a
  security refusal. Both now have help text.

Those three were not found by reading. Two tests came with the generator: one
fails if any command or argument has no help text, the other if a configuration
key never reaches `init-config`, compared against a serialised
`Config::default()` — which is every field there is, by construction. They
failed on their first run and named all four gaps.

### Documentation

- **The collection folder wants one file per run, and the README now says so
  rather than filing it under a caveat.** With a collector that overwrites one
  file per machine, the index holds history the folder does not, and "the
  folder is the source of truth, the index is derived" stops being true. The
  fix belongs at the gather step, not the write step: loadbearer's
  `--skip-if-newer-than` reads the mtime of its `--output` file and nothing
  else, so timestamping *that* path silently stops the gate ever firing and
  re-benchmarks the estate every sweep. See
  [Keep every run](https://github.com/issinoho/loadbearer/wiki/Fleet-Deployment#keep-every-run-not-just-the-latest).
- **The wiki now covers what gets looked up rather than read**: the generated
  reference, the [findings
  catalogue](https://github.com/issinoho/loadbearer-fleet/wiki/Findings) with
  all fourteen rules and every threshold, the [HTTP
  API](https://github.com/issinoho/loadbearer-fleet/wiki/HTTP-API),
  [metrics](https://github.com/issinoho/loadbearer-fleet/wiki/Metrics), and
  [troubleshooting](https://github.com/issinoho/loadbearer-fleet/wiki/Troubleshooting).
- The metrics page carries one thing worth acting on if you alert on this
  service: the four `scan_*` detail metrics are **absent** until a scan has
  succeeded, so a rule written only on their age stays silent through exactly
  the outage it was written for. Use `absent(...) or time() - ... > 3600`.

## 0.2.1 - Thu, 10 Sep 2026

One fix, and the setup instructions that walked you into a wall. Nothing about
the analysis, the dashboard or the index changed, so upgrading is replacing the
binary.

- **A placeholder left in a path setting is refused now**, as the identity
  settings already were. `init-config` writes
  `file = 'PUT-THE-PATH-FOR-THE-LOG-FILE-HERE'` and nothing checked it, so an
  unedited config created a *directory* with that name and logged into it
  contentedly. `collection_dir` and `archive_dir` are checked too. These fail
  in a worse way than the identity settings do — they don't fail: a placeholder
  tenant at least produces an error from the provider, however unhelpful, while
  an unreachable collection folder is reported and survived by design. Both
  leave you with something that looks configured. The checks run *before* the
  `auth.mode = "none"` shortcut rather than after it, which is the whole point:
  sign-in is the last thing anyone sets up, so the unauthenticated config is
  exactly the one that reaches a placeholder path.
- **The service setup in the README told you to run a command that fails.**
  `init-config > C:\ProgramData\loadbearer-fleet\fleet.toml` cannot work on a
  machine that hasn't been set up, because `>` does not create the folder — it
  fails with "Could not find a part of the path". It creates the folder first
  now, and writes with `Set-Content -Encoding utf8` rather than `>`, because
  Windows PowerShell 5.1 redirects as UTF-16 and the config is read as UTF-8,
  which gets you `stream did not contain valid UTF-8` at a point where a
  corrupt file is the more natural suspicion. The Linux path had the same
  missing directory plus the `sudo` redirect trap, where the shell opens the
  file as you before `sudo` gets a say.
- The repository now meets GitHub's community standards — a security policy
  with the [threat model spelled
  out](https://github.com/issinoho/loadbearer-fleet/blob/main/SECURITY.md),
  contributing guide, code of conduct and issue forms.

## 0.2.0 - Thu, 10 Sep 2026

Everything an estate needs in order to move, back up and forget — plus one
chart bug that had been on screen since the charts were written.

### Moving, backup and removal

- **`backup <file>`** takes a consistent snapshot of the index while the
  dashboard is still serving, using SQLite's `VACUUM INTO`. A plain copy of a
  live database is a torn read, and nobody stops a dashboard nightly so a
  backup agent can have it. Refuses to overwrite; restoring is putting the file
  back where `index` points.
- **`archive_dir`** (optional) keeps every document indexed: gzipped,
  content-addressed, about 8 KB a run against 40 KB raw. It exists for the
  collection pattern where each machine overwrites one file, which otherwise
  leaves the index as the sole record of earlier runs. With it set, that record
  is in files again, so the index is genuinely derived and moving servers is a
  copy rather than a migration. A run is archived *before* it is indexed, so
  "in the index" implies "kept".
- **`scan` reads `.json.gz` as well as `.json`**, which is what makes importing
  an archive just a scan — and lets a collection share be gzipped for a fifth
  of the space.
- **`forget <machine>`** removes a machine from the index and deletes the
  documents archived for it. A scan only ever adds, so deleting a result file
  left the run indexed and the machine still counting towards the fleet total
  and its cohort median. Takes a hostname or a machine key, has `--dry-run`,
  and lists both candidates rather than guessing when a reissued hostname
  matches two machines. It does not touch the collection folder — it reports
  which files are still there, because while they are, the next scan indexes
  the machine straight back.
- **An index this build cannot read is now kept rather than dropped.** A change
  to the index format renames the file to
  `fleet-index.superseded-v1-<when>.db` and rebuilds a fresh one. Dropping the
  tables was only harmless if the collection folder keeps a file per run; a
  rename costs the same rescan and leaves the history on disk. A rename that
  fails stops startup with an explanation rather than falling back to deleting.

There is deliberately no export format and no import command: the interchange
format is `loadbearer.result/1`, which loadbearer already promises to keep
stable, so moving between servers and consolidating two of them are both file
copies plus a scan.

### Fixed

- **A chart axis could print the same label twice.** With a tallest bar of two
  machines the ticks landed on halves and the integer formatter rendered
  `0, 1, 1, 2, 2` — and one or two machines in a grade bucket is the normal
  case on a small estate, so it was there from the start. Counting axes now
  take a minimum step of one, and `scripts/check-ui.mjs` fails on a duplicated
  label so it cannot come back.

### Documentation

- A getting-started walkthrough for a clean start on Windows and Linux, with
  every command run rather than written from memory.
- A favicon, a README banner and a screenshot of the overview.
- Sections on upgrading, on backup and restore, and on what removing a machine
  actually takes.

## 0.1.0 - Thu, 10 Sep 2026

First release. A fleet dashboard over a folder of collected
[loadbearer](https://github.com/issinoho/loadbearer) results.

- **Reads the folder, not loadbearer's code.** The `loadbearer.result/1` schema
  is loadbearer's documented stability contract and its Rust API explicitly
  isn't, so this parses the document. Unknown fields are ignored, almost
  everything is optional, and grade/confidence values this build has never seen
  degrade to "unrecognised" rather than failing the file — a dashboard that
  won't load a fleet because one machine runs a newer client would be worse
  than useless. A schema *major* it doesn't know is refused rather than guessed.
- **Ingest is idempotent by content.** Every run is keyed by the SHA-256 of the
  document, so a share can be rescanned constantly and a file the collector
  renamed on copy doesn't become a second run. A file that won't parse is
  reported and skipped, never fatal.
- **Compares a machine with its peers, and with its own past**, rather than
  with the reference baseline — whose anchors carry ±10–20% uncertainty. A
  cohort is machines with the same CPU *measured the same way*: instruction set,
  preset, profile and baseline are all part of the key. Median and MAD, not
  mean and standard deviation, so two degraded machines can't hide each other.
- **Findings split into three queues** by what you would do about them: the
  machine, the measurement, or the data we hold. Mixed into one list, a thermal
  caveat reads as a failing asset.
- **A web dashboard**: executive overview, cohort explorer, machine table and a
  per-machine drilldown with score history, components, findings and every
  subtest of the latest run. Hand-rolled SVG, no build step, nothing fetched
  from the internet at runtime — so it works on an air-gapped management
  network. Every chart has a table view.
- **Single sign-on**: OpenID Connect authorization code flow with PKCE, roles
  from group claims, and tag-scoped access enforced in the data layer rather
  than the UI. Somebody who authenticates but matches no grant is refused, not
  shown an empty fleet.
- **Runs as a service** on Windows (service controller integration, restart
  policy) and Linux (a hardened systemd unit it prints for you), rescanning on
  a timer, with Prometheus metrics for the things worth alerting on —
  `scan_last_success_timestamp_seconds` above all, because collection stopping
  is the failure that hides every other one.
- **Refuses to put the estate on the wire in the clear.** A non-loopback bind
  needs both sign-in and https, or an explicit `--allow-remote`.
