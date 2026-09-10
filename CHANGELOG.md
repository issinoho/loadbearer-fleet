# Changelog

All notable changes to loadbearer-fleet are documented in this file.

The release workflow extracts the section for a tag verbatim as that release's
notes, so each one has to stand on its own.

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
