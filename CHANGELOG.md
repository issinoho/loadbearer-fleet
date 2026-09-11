# Changelog

All notable changes to loadbearer-fleet are documented in this file.

The release workflow extracts the section for a tag verbatim as that release's
notes, so each one has to stand on its own.

## 0.5.2 - Fri, 11 Sep 2026

Three fixes from one session on a real host, all of them the tool being
confidently unhelpful about *which index* it was looking at. **One behaviour
change**, noted below, matters if you script `report`, `status` or `backup`.

### Fixed

- **A command that only reads no longer invents an index.** SQLite's `open`
  creates the file, so `loadbearer-fleet forget <machine>` without `--config`
  made a `fleet-index.db` in the current directory and reported that no
  machine matched — while the service's index held that machine the whole
  time. `report`, `status`, `backup` and `forget` now refuse on a missing
  index, naming the path they looked at and pointing at `--config`. `scan` and
  `serve` still create one; populating is their job.
- **`forget` says how much the index holds.** "No machine in the index
  matches" is a true statement about an empty index and a useless one: the
  reader's question is whether they are looking at the right database. The
  message now names the index file and the number of machines in it, and an
  index with none gets its own wording.
- **The dashboard notices a change made by another process.** The fleet-wide
  analysis is cached in memory and was rebuilt only by a scan, so a `forget`
  run from a terminal left the machine on screen until the next scan tick —
  up to fifteen minutes, which reads as `forget` not having worked. It now
  checks SQLite's `data_version`, which by design does not move for the
  serving process's own writes, so it answers exactly the right question: has
  somebody else committed? One pragma against an already-open connection per
  request, and the rebuild happens only when something changed.

  **Rescan folder** was the workaround, and still works — it rebuilds the same
  cache.

### Changed

- **`report`, `status`, `backup` and `forget` now exit non-zero when the index
  does not exist**, where they used to succeed against an empty one they had
  just created. Anything scripted around "empty snapshot means empty fleet"
  should be checked: an empty snapshot from a database the command invented a
  moment earlier reads as *the fleet is empty* when the truth is *you are
  looking at the wrong file*. To get the old shape, `scan` first — which is
  what creates the index in the first place.

## 0.5.1 - Fri, 11 Sep 2026

Everything in this release came out of deploying 0.5.0 to a real host and
watching where the time went. No behaviour changes: it is three things the
tool already knew and did not say, and one piece of documentation that was
actively dangerous.

### Fixed

- **`service preflight` gave advice that would have made things worse.** A
  relative `index` resolves beside the config file — so with the config in
  `/etc/loadbearer-fleet`, the index lands there too, and the report said
  `/etc/loadbearer-fleet is owned by 0:0 … sudo chown loadbearer-fleet: <dir>`.
  Following that hands the service write access to its own configuration. It
  now recognises the case and names the *setting* instead: which keys resolved
  there, why a relative path does that, and to give them absolute paths under
  `/var/lib`. All the offending keys are reported in one failure, rather than
  one per run as each is fixed.
- **The log file the configuration names does not exist.** `[log] file` is a
  stem — the rotation appends the UTC date — so `tail -f` on the configured
  path fails with `No such file or directory`, which is a poor way to find out
  how rotation works. The binary now announces the file it is actually writing
  when it starts, on **stderr**, so `journalctl -u loadbearer-fleet` answers
  the question for a service; and `service preflight` reports the dated name
  rather than the stem. Stdout is untouched, so piping `report --json` is
  unaffected — and there is now a test that runs the binary *with* a log file
  configured to hold that line, which the existing ones structurally could
  not.
- **Advice with a placeholder in it.** The generic unwritable-directory
  failure said `sudo chown loadbearer-fleet: <dir>` whatever the account and
  whatever the directory. It names both now, so the line can be pasted.
- **`check-auth` listed only the group claim** under what it cannot know from
  outside a real sign-in. `name`, `preferred_username` and `email` follow the
  same rule, and a provider that sends them to userinfo only — Authelia's
  default — leaves the dashboard labelling your session with a UUID. It says
  so before the sign-in rather than after.

### Documentation

- **The unit is no longer installed with `service unit | sudo tee`.** `tee`
  truncates its target before the command on the left has produced anything,
  so a generate that *refuses* — a missing `[log] file`, a config path it will
  not accept — leaves a zero-length unit file behind. systemd reads that as
  **masked**, so the next `systemctl start` answers
  `Unit loadbearer-fleet.service is masked` and the unit that was there is
  gone. The README and both places in the runbook now write a temporary file
  and `&&` the install, so a refusal changes nothing, and there is a
  Troubleshooting entry under the message you would actually search for.
- **Moving a by-hand setup into place** is written up, because it is how most
  people will arrive: an unpacked tarball in a home directory, a config beside
  it, results wherever was convenient. Four things move and the order matters
  — binary before the unit is generated, config out of `$HOME` with its paths
  made absolute, the index carried across rather than rebuilt, and the
  collection folder kept out of the service's own data directory.
- **Three Authelia corrections**, all from a live 4.39.25: the consent
  duration is `1M` and not `'1 month'` (which fails as a YAML error, not as a
  bad value); `docker compose up -d` does not pick up an edit to a
  bind-mounted config and `restart` is the command; and `validate-config` line
  numbers are post-template-expansion, so a 75-line file can report an error
  at line 110.
- The Linux download in the README is split across variables, because a URL
  long enough to wrap pastes as two commands — the first of which downloads a
  truncated path and saves nine bytes of "Not Found" under a plausible name.

## 0.5.0 - Fri, 11 Sep 2026

Two commands that now do the diagnosis rather than describe it, a dashboard
that works on a phone, and a Sign out button that works at all. **If you run
this behind an identity provider, two of the fixes matter to you**, and one of
them needs a change on the provider as well.

### New

- **`service preflight` checks what the generated unit will depend on, before
  you install it.** The hardened unit refuses to start unless what it names
  already exists, and systemd reports those refusals as `217/USER` and
  `226/NAMESPACE` — codes that say nothing about the cause, arriving while you
  are reading journal output rather than a sentence. So it checks, while the
  answer is still readable: an absolute `--config`, `[log] file` set, the
  binary not under a home directory (it becomes `ExecStart`, and
  `ProtectHome=yes` would hide it), no config path under one either, the
  service account existing, every `ReadWritePaths` directory existing *and*
  writable by that account, the collection folder present, and the bind
  address actually free. Each failure carries its own fix.

  Two deliberate calls: writability is judged from ownership and mode rather
  than by attempting a write as the target account, which would mean being that
  account — a heuristic, and it says so. And a missing collection folder is a
  note rather than a failure, because the startup scan reports it and carries
  on.

### Changed

- **`check-auth` sends the authorization request a real sign-in sends, and
  reads the answer.** It used to prove discovery and then print your settings
  back at you. It now settles in one command what took three rounds of `curl`:
  whether the client is registered, whether the redirect URI matches byte for
  byte, and whether the scopes and PKCE are accepted. Checked against a live
  provider along with each failure it claims to diagnose — an unknown
  `client_id`, an unregistered redirect URI, and a scope the client is not
  allowed. It exits non-zero, so it can gate a deploy rather than being
  something somebody has to read carefully. What it cannot know it lists,
  because the group claim only appears in a real ID token.
- **Three runbooks in the wiki**, and a README that links to them instead of
  restating them: [deploying as a
  service](https://github.com/issinoho/loadbearer-fleet/wiki/Deploying-as-a-Service),
  [adding an identity
  provider](https://github.com/issinoho/loadbearer-fleet/wiki/Adding-an-Identity-Provider),
  and [Authelia](https://github.com/issinoho/loadbearer-fleet/wiki/Identity-Provider-Authelia)
  specifically, which has been run end to end. Ordered steps that each end in
  a command, rather than prose with the sequence left to infer, plus a table
  mapping every `FAIL` line `check-auth` can print to the one setting that
  fixes it.
- **The dashboard is checked in a real browser.** `scripts/check-mobile.mjs`
  starts the binary and lays the page out in headless Chrome at 390, 320, 768
  and 1440 pixels wide, failing if anything scrolls sideways, is painted past
  the right edge, clips instead of scrolling, or is too small to tap. It found
  a 320px fault on its first CI run that this machine's font metrics had
  hidden.

### Fixed

- **Sign out did nothing.** It ended the session correctly and then redirected
  to `/`, which needs a session — so the browser was sent on to `/auth/login`,
  the identity provider still had a session of its own, handed back a fresh
  one, and you arrived back on the dashboard. The session really had ended;
  none of that was visible. It now lands on `/auth/signed-out`, a page that
  needs no session, starts none, and explains the part that surprises people:
  this ends the dashboard's session and deliberately not the provider's, so
  signing back in may not ask for a password.
- **The banner labelled people with a UUID.** The display name came from
  `name`, then `preferred_username`, then the subject — and this reads the ID
  token and never calls userinfo, so on a provider that sends those claims to
  userinfo only, which is Authelia's default, the subject was all that was
  left. `email` is now tried before giving up, and giving up logs a warning
  naming the fix. **The real fix is on the provider:** put `name`,
  `preferred_username` and `email` in the ID token — the Authelia page has the
  exact block, and now also sets `consent_mode`, so you are not asked to
  consent on every sign-in. Nothing was broken meanwhile: the role and the
  scopes come from the group claim, not from the name.
- **The dashboard was unusable on a phone.** The topbar was a single
  non-wrapping row, so at 390px it laid out 685px of content: the last tab
  clipped, Rescan and Sign out and the theme toggle sat off-screen entirely,
  and the whole page dragged sideways. It is now two rows at phone widths —
  the mark and a full-width tab strip, with the actions beneath — with the
  filters in two columns, a 40px floor on every control wherever the pointer
  is coarse, and a tab strip that scrolls inside itself rather than widening
  the page. The `indexed N run(s) · read …` line moves to the footer beside
  the build: the same class of answer, and it was what made the header wrap.
- **A long value in the machine drilldown dragged the page sideways on a 320px
  screen.** A grid column that would not shrink below its content, so a CPU
  model took the document 17px wider than the window.

## 0.4.1 - Fri, 11 Sep 2026

Four fixes to running as a service on Linux, all found by following this
project's own instructions on a real host rather than reading them. If you run
it under systemd with `archive_dir` set, the first one matters to you.

- **`ReadWritePaths` omitted `archive_dir`.** The generated unit named the
  index's directory and the log's, and `ProtectSystem=strict` makes everything
  else read-only — so with an archive anywhere outside those two, every archive
  write was refused at runtime. It fails in the worst available shape: the
  service starts, the dashboard works, and the symptom is an archive that is
  mysteriously empty, noticed months later. **Regenerate your unit after
  upgrading** — `service unit` writes the correct `ReadWritePaths` now, but an
  already-installed unit keeps the old one.
- **`service unit` refuses a configuration whose paths are under a home
  directory.** The unit sets `ProtectHome=yes`, which makes `/home`, `/root`
  and `/run/user` *invisible* to the service rather than merely unreadable — so
  an index or log there fails as though it had never been created. That is
  knowable when the unit is printed, so it is refused then, naming each setting
  and its path, rather than left to systemd to report obscurely.
- **`service unit` no longer creates the log directory just to print.** It set
  logging up first, so previewing a unit as an ordinary user produced
  `creating the log directory /var/log/loadbearer-fleet` instead of a unit, and
  the only way to read one was as root. `init-config` and `reference` already
  skipped logging for that reason; this was the third command of the kind and
  the only one not treated as one.
- **The README's Linux service path was incomplete**, which is how the three
  above came to light. Following it exactly failed: nothing created the
  `loadbearer-fleet` account the unit names — systemd reports `217/USER` —
  nothing created the directories `ReadWritePaths` needs, which is
  `226/NAMESPACE` and says nothing about the cause, and `ExecStart` is whichever
  binary printed the unit, so generating it from an unpacked tarball in a home
  directory baked in a path `ProtectHome` then hid. Windows and Linux now have
  their own headings, and the Linux one is the whole sequence.

Both new guards were watched failing with the fix removed. The `ProtectHome`
one needed a Linux build to demonstrate at all: on Windows `/home/...` is not
an absolute path, so it is resolved beside the config file before the check
ever sees it.

## 0.4.0 - Thu, 10 Sep 2026

Sign-in against a **self-hosted** identity provider — Authelia, Keycloak,
Authentik — now works where it previously could not. Minor rather than patch
because there is a new configuration setting; nothing changed about the
analysis, the index or the dashboard, so upgrading is replacing the binary.

### `auth.ca_bundle`

**A provider behind an internal certificate authority was unreachable, and
there was nothing you could do about it.** The HTTP client verifies against a
built-in root set and deliberately does not read the machine's trust store, so
installing your CA on the server changed nothing and no setting existed to
point at it. Discovery simply failed.

```toml
[auth]
ca_bundle = '/etc/loadbearer-fleet/internal-ca.pem'
```

A PEM file of extra authorities, **added** to the built-in roots rather than
replacing them — so moving the provider onto a publicly-trusted certificate
later needs no change here, and a publicly-trusted one (Let's Encrypt via
DNS-01 works for an internal-only hostname) needs no setting at all. There is
still no option to skip verification, and there won't be.

An empty PEM file is refused rather than accepted: it would leave the provider
untrusted while looking configured, which is the worst of both. Both failures
name the setting *and* the file, because somebody who has already installed the
CA on the server will not believe the problem is here.

### `check-auth` says what it will look for

It reported the client type and redirect URI; it now also reports the scopes it
will request and where its certificate trust comes from:

```
discovery succeeded for https://auth.example.internal
  client_id:    loadbearer-fleet
  client type:  public (PKCE, no secret)
  redirect URI: https://fleet.example.internal/auth/callback
  groups claim: groups
  extra scopes: groups
  CA trust:     built-in roots + /etc/loadbearer-fleet/internal-ca.pem
  grants:       2
```

And it now states the requirement that catches people out: **this reads the ID
token and never calls the userinfo endpoint**, so a `groups` claim that only
appears at userinfo is invisible — you sign in successfully and match no grant.
On Entra that means a token-configuration change and no scope; on a self-hosted
provider it usually means the opposite, `extra_scopes = ["groups"]`, plus
whatever that provider calls the setting for which claims go in the ID token.

There is a new
[Self-hosted providers](https://github.com/issinoho/loadbearer-fleet#self-hosted-providers--authelia-keycloak-authentik)
section covering both, and
[SECURITY.md](https://github.com/issinoho/loadbearer-fleet/blob/main/SECURITY.md)
now records how the provider's certificate is verified and that discovery does
not follow redirects — following one from a discovery URL would make this an
SSRF primitive.

### Also

- **A guard this project shipped in 0.3.0 was weaker than its own description.**
  The test that proves `init-config` documents every configuration key passed
  with a key missing, because TOML has no null: the serializer omits a `None`,
  so every `Option` setting was invisible to it — `collection_dir`,
  `archive_dir` and `log.file` among them. It now builds a configuration with
  every field written out and no `..Default::default()`, so the *compiler*
  enforces completeness and a new setting will not build until the test names
  it. The 0.3.0 notes and `CONTRIBUTING.md` both called it "every field there
  is, by construction"; that was true only of fields with values.

## 0.3.2 - Thu, 10 Sep 2026

Fixes a regression this project shipped in 0.3.0, and answers a question an IT
department will ask on day one: which machines belong to whom.

### `report --json` is pipeable again

**If you script against `report --json`, this is the release you want.** In
0.3.0 and 0.3.1 it did not parse:

```
$ loadbearer-fleet report --json | jq .summary
parse error: Invalid numeric literal at line 1, column 5
```

The build-identity line added in 0.3.0 was written to **stdout**, which is
where `tracing_subscriber` puts a console log by default, so it arrived ahead
of the JSON with ANSI colour codes attached. The README describes that command
as "the whole snapshot, as the UI sees it" and the wiki says it can be
scripted or diffed; neither was true for two releases.

**Logging now goes to stderr** when no `[log] file` is configured, which is
where a command-line tool's diagnostics belong. Two consequences worth
knowing:

- A configured log file is unaffected — that path never touched stdout.
- If you were capturing console output by redirecting **stdout**, redirect
  stderr instead (`2>`), or set `[log] file` and stop depending on the
  console.

`tests/stdout_is_data.rs` now runs the real binary and parses what comes back
for `report --json`, `init-config` and `reference`, and fails on a stray
escape code. Nothing guarded those three pipeable outputs before, which is how
this got through twice.

### Group machines by whatever you label them with

- **Every tag key is now its own sortable column** in the machine table.
  Previously they were joined into one unsortable cell — and joining could not
  have sorted usefully anyway, since `ring=canary site=glasgow owner=jsmith`
  sorts by whichever key comes first alphabetically. Ask for the table by owner
  and you got it by ring.
- **Search matches tag values.** It covered hostname, serial, asset tag and CPU
  model — every identifier except the one you put there yourself. Tag *keys* are
  deliberately still not matched: searching `site` should not return every
  machine that has one.

Nothing to configure for either. A `--tag owner=jsmith` becomes a filter, a
column and a search term on its own, and one person owning several machines is
just a repeated value. Tags remain outside the cohort key, so labelling an
estate never disturbs a peer group.

### Documentation

- **[Labelling machines, and a word about
  owners](https://github.com/issinoho/loadbearer-fleet#labelling-machines-and-a-word-about-owners)**
  — tagging an owner works, but that value then lives in every result document,
  the index, `archive_dir` and every backup, and it is frozen at the moment of
  the run. Where the assignment already lives in Intune or an asset system,
  joining on `serial` or `asset_tag` costs less and stays current.
- **Where the collector fits, and why Intune is different.** `loadbearer`
  spawns no process and makes no network call, so something always has to move
  its output to the folder this reads. PDQ and Ansible have a step for that;
  Intune has none, so the script does the copy itself — which makes the run
  context and the share's permissions the thing to get right. Covered in
  [What sits in front of this](https://github.com/issinoho/loadbearer-fleet/wiki),
  in [Troubleshooting](https://github.com/issinoho/loadbearer-fleet/wiki/Troubleshooting)
  for when nothing arrives, and written up in full — marked untested — in
  loadbearer's
  [Fleet Deployment](https://github.com/issinoho/loadbearer/wiki/Fleet-Deployment#intune-and-anything-else-with-no-pull-file-step).

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
