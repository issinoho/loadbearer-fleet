# loadbearer-fleet

A fleet dashboard over collected [loadbearer](https://github.com/issinoho/loadbearer)
results: point it at the folder your deployment tool drops `.json` result files
into, and it indexes them and reports across the estate — executive summary,
grouping, drilldown, and the outliers and red flags worth acting on.

**Status: early.** The index and schema reader work; the analytics, web UI and
authentication are not built yet. See [Roadmap](#roadmap).

## How it fits together

```
  PDQ / Intune / Ansible          this
  runs loadbearer on each   ->   \\share\loadbearer\*.json   ->   index   ->   dashboard
  machine, writes --output          (source of truth)          (derived)
```

Three decisions shape everything else:

**The folder is the source of truth.** The index is a derived read model and
holds nothing that isn't in the files. Delete it and it rebuilds — which is why
its schema can change freely, why there is nothing to migrate, and why it isn't
something you need to back up.

**It reads the schema, not loadbearer's code.** loadbearer's `VERSIONING.md`
makes the `schema`-tagged JSON a stability contract and says in the same breath
that its internal Rust API is not one. So this parses the document. The two
projects release independently and the contract between them is the thing
that's actually promised.

**Data quality is a first-class input, not an afterthought.** loadbearer records
when a run shouldn't be taken at face value — a thermally limited machine, a
measurement taken on battery, a `--target-dir` that turned out to be a RAM disk,
a component a security policy blocked. Ranking an estate while ignoring those
would produce confident nonsense, so they're indexed alongside the numbers and
surfaced.

## Usage

```
loadbearer-fleet scan \\fileserver\loadbearer      # index a folder (or re-index it)
loadbearer-fleet status                            # what's in the index
```

Rescanning is cheap and idempotent: every run is keyed by the SHA-256 of the
document it came from, so an unchanged file is a no-op and a file the collector
renamed on copy doesn't become a second run.

## Identity, and why grouping is trustworthy

Runs are attributed to a machine by the best identifier the result carries:

| | survives a reimage | notes |
| --- | --- | --- |
| `serial` | yes | what asset, warranty and lease records key on |
| `smbios_uuid` | yes | absent on some VMs |
| `machine_id` | **no** | always readable; resets on reimage |
| `hostname` | no | renameable and reissued — a last resort |

This matters in practice and not just in theory: on Windows all four are
readable by any user, but on Linux the DMI serial and UUID are root-only, so an
unprivileged sweep there keys on `machine_id`. In testing against seven real
machines, three keyed on `serial` and four on `machine_id`. The dashboard
reports which, because "four of your machines are keyed on an identifier that
resets when they're reimaged" is something an estate owner should know.

## Roadmap

- [x] Schema reader, tolerant of older files and forward-compatible with new fields
- [x] Folder scan into a SQLite index, idempotent by content, full history retained
- [ ] Cohort analytics — compare a machine against its own peer group rather than
      the reference baseline, which sidesteps that baseline's ±10–20% uncertainty
- [ ] Red flags: failing grades, thermal limits, partial runs, battery wear,
      stale results, capped working sets
- [ ] Web UI: executive summary, cohort explorer, machine drilldown with history
- [ ] Entra ID / OIDC SSO, roles from group claims, tag-scoped authorization
- [ ] Service packaging, config file, metrics

## Licence

MIT.
