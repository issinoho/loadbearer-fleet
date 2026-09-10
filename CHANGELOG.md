# Changelog

All notable changes to loadbearer-fleet are documented in this file.

The release workflow extracts the section for a tag verbatim as that release's
notes, so each one has to stand on its own.

## Unreleased

- **`forget <machine>`** removes a machine from the index and deletes the
  documents archived for it. A scan only ever adds, so deleting a result file
  from the collection folder left the run indexed and the machine on the
  dashboard, still counting towards the fleet total and its cohort median —
  which is the same property that lets the index keep history an overwriting
  collector has discarded, seen from the other side. Takes a hostname or a
  machine key, refuses to guess when a reissued hostname matches two machines,
  and has `--dry-run`. It deliberately does not touch the collection folder;
  it reports which files are still there, because while they are the next scan
  indexes the machine straight back.

- **`backup <file>`** takes a consistent snapshot of the index while the
  dashboard is serving, via SQLite's `VACUUM INTO` — copying a live database
  with `cp` is a torn read, and nobody stops a dashboard for a backup agent.
  Refuses to overwrite; restore is putting the file back where `index` points.
- **`archive_dir`**, optional, keeps every document indexed: gzipped,
  content-addressed, about 8 KB a run. It exists for the collection pattern
  where each machine overwrites one file, which leaves the index as the only
  record of earlier runs — with this set, that record is in files, so the index
  is genuinely derived again and moving servers is a copy. Archived before
  indexed, so "in the index" implies "kept".
- **`scan` reads `.json.gz` as well as `.json`**, which is what makes importing
  an archive just a scan, and lets a collection share be gzipped.
- Moving between instances, consolidating two of them, backup and restore are
  therefore all file copies plus a scan. There is deliberately no export format:
  the interchange format is `loadbearer.result/1`, which loadbearer already
  promises to keep stable.

- **An index this build can't read is now kept, not dropped.** A change to the
  index format renames the existing file to
  `fleet-index.superseded-v1-<when>.db` and rebuilds a fresh one from the
  collection folder. It used to drop the tables in place, which is only
  harmless if the folder keeps a file per run: the index holds a row per run,
  so a collector that overwrites one file per machine leaves the index as the
  sole record of everything earlier. Renaming costs the same rescan and leaves
  the history on disk. A rename that fails stops startup with an explanation
  rather than falling back to deleting.
- **Axis labels can no longer repeat.** A chart whose tallest bar was 2
  machines drew ticks on halves and printed `0, 1, 1, 2, 2` through an integer
  formatter — the normal case on a small estate. Counting axes now take a
  minimum step of 1, and `scripts/check-ui.mjs` fails on a duplicated label.
- A favicon and README branding, and upgrade instructions.

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
