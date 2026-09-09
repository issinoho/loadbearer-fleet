# loadbearer-fleet

A fleet dashboard over collected [loadbearer](https://github.com/issinoho/loadbearer)
results: point it at the folder your deployment tool drops `.json` result files
into, and it indexes them and reports across the estate — executive summary,
grouping, drilldown, and the outliers and red flags worth acting on.

**Status: early but usable.** Ingest, cohort analytics, the red-flag engine, the
web dashboard and single sign-on all work. Out of the box it runs
unauthenticated on loopback; point it at your identity provider and it does OIDC
with roles and per-site scoping. See [Roadmap](#roadmap).

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
loadbearer-fleet report                            # summary, cohorts and red flags
loadbearer-fleet report --all                      # include informational flags
loadbearer-fleet report --json                     # the whole snapshot, as the UI sees it
loadbearer-fleet serve \\fileserver\loadbearer     # the dashboard, on http://127.0.0.1:8787
loadbearer-fleet init-config > fleet.toml          # a starter config, placeholders included
loadbearer-fleet serve --config fleet.toml         # with single sign-on
loadbearer-fleet check-auth --config fleet.toml    # check the identity provider settings
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
- [x] Cohort analytics — compare a machine against its own peer group, and against
      its own history, rather than against the reference baseline
- [x] Red flags: low grades, cohort outliers, regressions, weak components, thermal
      limits, forced runs, partial runs, RAM-backed disk targets, battery wear,
      stale results, weak identity
- [x] Web UI: executive summary, cohort explorer, machine table, machine drilldown
      with history — see [The dashboard](#the-dashboard)
- [x] Entra ID / OIDC SSO with PKCE, roles from group claims, tag-scoped
      authorization — see [Sign-in](#sign-in)
- [ ] Service packaging, config file, metrics

## Analysis

### Peers first, then history, then the baseline

The reference baseline answers "is this machine any good". It is the wrong tool
for "is this machine broken": loadbearer's own baseline header puts ±10–20%
uncertainty on the anchors, and some of them rest on three machines. A 15%
shortfall against the baseline could be the baseline.

So the primary comparison is **a machine against its peers**, and the secondary
one is **a machine against its own history** — which is stronger still, because
the hardware is held constant. A machine 15% down on forty identical machines in
the same estate, or 15% down on its own last four runs, is the machine.

A cohort is not "machines with the same CPU" but machines with the same CPU
*measured the same way*: `build_isa`, `duration_preset`, `profile` and `baseline`
each change the number without anything changing about the machine, so each is
part of the cohort key. Installed RAM deliberately is not — two machines with the
same CPU and different DIMM configurations really do differ, and surfacing the
single-channel one is the point rather than something to excuse.

Cohorts use **median and median absolute deviation**, not mean and standard
deviation, because the statistic has to survive the thing it is looking for. Six
machines at 1000 and two at 720 have a mean of 930 and a standard deviation of
118, which puts the two bad ones 1.8 sigma out — invisible to a 3.5-sigma rule,
because each one's damage is partly absorbed into the spread the other is
measured against. The median stays at 1000 and names both. On a fleet of clones
MAD collapses towards zero, so the dispersion is floored at a fraction of the
median; that floor is usually what binds, and it puts the effective trigger at
around a 15% shortfall.

### Three queues, not one wall of red

Findings are separated by what you would do about them, because mixing them
means an estate owner reads a thermal caveat as a failing asset:

| Queue | Means | Examples |
| --- | --- | --- |
| **machine** | a hardware decision — repair, replace, reassign | low grade, cohort outlier, regression, one weak component, worn battery |
| **measurement** | the number can't be trusted until collection is fixed | thermally limited, run on battery, partial run, RAM-backed disk target, unstable CPU timings |
| **coverage** | about the data we hold, not about the machine | stale result, attributed by hostname only, no peer group |

Every threshold lives in one struct with its reasoning attached, so tuning the
engine is a config change rather than a code read.

## The dashboard

```
loadbearer-fleet serve \\fileserver\loadbearer
```

Scans the folder, then serves four views on `127.0.0.1:8787`: an executive
**overview** (how many machines need attention, grade distribution, findings by
queue, score spread, and the ranked list of what to look at), a **cohort
explorer** (each machine against the median of the machines like it), a sortable
**machine table**, and a **drilldown** per machine with its score history, its
components, its findings in full, and every subtest of its latest run.

`POST /api/rescan` — the Rescan button — re-reads the folder. `GET /api/snapshot`
returns exactly what the dashboard draws, so any view can be scripted or
diffed; `GET /api/machine/{key}` is the drilldown payload.

### No build step, no CDN, one binary

The HTML, CSS and JavaScript are compiled into the executable, and the charts
are hand-rolled SVG. There is no npm, no bundler, and nothing is fetched from
the internet at runtime — which is what makes it work on an air-gapped
management network, and what lets the server send `default-src 'self'` and mean
it.

### The charts

Built to the Claude Code `dataviz` skill's specs, and the colour work was
computed rather than eyeballed: the diverging blue/red pair passes all six
checks in both light and dark mode. Grades get a single hue rather than an
ordinal ramp, because six steps cannot clear the ramp's adjacent-lightness gate
inside the range the surface allows — and the axis already carries the order.
Every chart has a table view, so no value is ever reachable only by hovering,
and hover and keyboard focus show the same thing.

`node scripts/check-ui.mjs` renders every chart and every view against a
minimal DOM and asserts the geometry: no NaN reaching an SVG attribute, nothing
painted outside its own box, bars capped at 24px, markers carrying their surface
ring, hit targets big enough to hit, and axis labels that fit their band. It
needs Node; the server does not, so it is deliberately outside `cargo test`.

## Sign-in

```
loadbearer-fleet init-config > fleet.toml     # then fill in the placeholders
loadbearer-fleet check-auth --config fleet.toml
loadbearer-fleet serve --config fleet.toml
```

OpenID Connect authorization code flow with PKCE. Register the app as a public
client — the flow needs no client secret, and a secret in a config file on a
management server is a secret in every backup of that server — with the redirect
URI `<public_url>/auth/callback`.

Roles come from group claims. On Entra, emit the `groups` claim from the app
registration's **Token configuration** (it is a token setting, not a scope), and
map group **object IDs** to roles:

```toml
[[auth.grants]]
group = "<object-id-of-your-fleet-admins-group>"
role = "admin"     # read the dashboard, and trigger a rescan

[[auth.grants]]
group = "<object-id-of-the-glasgow-desktop-team>"
role = "viewer"    # read only
tags = { site = "glasgow" }   # and only the machines tagged this way
```

The most privileged matching grant wins; scopes are the union of the matching
grants at that role, so somebody in two site groups sees both sites. Someone who
authenticates but matches no grant is refused rather than shown an empty
dashboard — authenticating is not the same as being authorized.

**Tag scoping is enforced in the data layer.** A viewer's scope is pushed into
the same filter every request is projected through, and it is read from the
session, never from the query string — so a scoped viewer who edits the URL to
another site gets an empty result, never that site. A machine outside your scope
returns 404 rather than 403: whether it exists isn't something you're entitled to
learn. The Rescan button is hidden from viewers as a courtesy; the server refuses
the request whether it was hidden or not.

Two things the config refuses at load time rather than at first sign-in: any
value still holding a starter placeholder, named individually; and a multi-tenant
issuer (`/common/`, `/organizations/`, `/consumers/`), which cannot work because
those endpoints advertise a templated `{tenantid}` issuer that never matches the
URL it was fetched from. Use your own tenant's issuer.

Sessions are opaque random tokens in an `HttpOnly`, `SameSite=Lax` cookie, held
server-side and keyed by the token's SHA-256 so a memory dump doesn't hand
anyone a live session. They live in memory, so a restart signs everyone out;
there are no refresh tokens stored anywhere.

### It will not put the estate on the wire in the clear

The index holds the hostname and firmware serial of every machine you own, so a
non-loopback bind is refused unless **both** sign-in is configured and
`public_url` is https. `--allow-remote` overrides it and logs a warning. The two
refusals say different things because they are different mistakes: without
sign-in, anyone who can reach the port gets the whole estate; without TLS, the
session cookie crosses the network in the clear and whoever copies it is signed
in as that user.

The dashboard does not terminate TLS. Put a reverse proxy in front of it and set
`public_url` to the proxy's address.

## Licence

MIT.
