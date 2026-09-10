# Changelog

All notable changes to loadbearer-fleet are documented in this file.

The release workflow extracts the section for a tag verbatim as that release's
notes, so each one has to stand on its own.

## Unreleased

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
