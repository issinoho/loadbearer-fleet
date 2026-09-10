# Security Policy

## Supported versions

There are no maintained release branches — only the **latest release** is
supported. Please upgrade before reporting anything.

## Reporting a vulnerability

Please **don't** open a public issue for a security report.

The preferred way is GitHub's own
[private vulnerability reporting](https://github.com/issinoho/loadbearer-fleet/security/advisories/new):
open the [Security tab](https://github.com/issinoho/loadbearer-fleet/security)
and click "Report a vulnerability". That reaches only the maintainer and keeps
the conversation off the public tracker until there's a fix.

If you'd rather not use that, email **iain@issinoho.com**.

This is a solo-maintained project, so there's no formal SLA — but a genuine
security report gets priority over everything else. Expect an initial response
within a few days.

## What this is, in security terms

Unlike [loadbearer](https://github.com/issinoho/loadbearer), which is a local
command-line tool, this is a **long-running HTTP server that holds an inventory
of an entire estate** — hostnames, firmware serials, asset tags, CPU models,
disk models and capacities, for every machine that has ever reported. Treat the
index and the archive as you would any other asset database.

It:

- serves HTTP on **`127.0.0.1:8787` by default**, and [refuses a wider
  bind](README.md#it-will-not-put-the-estate-on-the-wire-in-the-clear) unless
  sign-in is configured *and* `public_url` is https, or `--allow-remote` is
  passed explicitly;
- **does not terminate TLS.** Put a reverse proxy in front of it;
- **reads** the collection folder and never writes to it; **writes** the index,
  and the document archive if `archive_dir` is set;
- **treats the collection folder as untrusted input**, because anything that
  can write there controls it — in the documented deployment, every machine in
  the estate. A document is refused above **16 MB**, on its size on disk before
  it is read and again on what it decompresses to, so a `.json.gz` cannot turn
  a few megabytes on the share into gigabytes in memory. A rejected file is
  reported and the rest of the scan continues;
- **caps sign-ins in flight** at 1024, since `/auth/login` needs no session and
  each call holds a state, a nonce and a PKCE verifier for fifteen minutes.
  Past the cap a *new* sign-in is refused with a 503 rather than an existing
  one being evicted, so a flood cannot break a sign-in somebody is halfway
  through;
- signs users in with OpenID Connect (authorization code flow with PKCE, no
  client secret required), holding sessions **in memory** keyed by the SHA-256
  of an opaque random token, in an `HttpOnly`, `SameSite=Lax` cookie marked
  `Secure` when `public_url` is https;
- serves every asset from its own binary — no CDN, no npm, nothing fetched at
  runtime — which is what lets it send `default-src 'self'` and mean it;
- makes **no outbound connections** except to the identity provider's discovery,
  JWKS and token endpoints during a sign-in, and only when `auth.mode = "oidc"`.
  Those requests **do not follow redirects** — following one from a
  discovery URL would make this an SSRF primitive — and they verify the
  provider's certificate against a **built-in root set**, not the machine's
  trust store. There is no option to skip verification; `auth.ca_bundle` adds
  a private CA to the built-in roots for a self-hosted provider, and is the
  only way to do it.

It stores no credentials. `client_secret` exists in the config for providers
that require one, but the intended shape is a public client with PKCE, so the
field is normally empty.

## Before you report: known, by-design behaviour

- **`auth.mode = "none"` makes everyone who can reach the port an
  administrator.** That is the default, which is why a non-loopback bind is
  refused without `--allow-remote`, and why the dashboard header says "Local
  access" rather than implying somebody signed in.
- **A tag-scoped viewer sees fleet-wide cohort statistics.** Scoping restricts
  which *machines* a viewer can see, and it is enforced in the data layer rather
  than the UI. But cohort medians and percentiles are deliberately computed
  across the whole index, because a peer group that shrank to the current filter
  would make the same machine's shortfall change depending on who was looking.
  So a viewer scoped to one site can infer aggregate performance of hardware
  they cannot otherwise see. If that matters in your environment, run separate
  instances over separate folders rather than relying on tag scoping.
- **`/metrics` carries no machine labels**, by design — but it does reveal fleet
  size, grade distribution and how much of the estate is failing. It is off
  unless `[metrics] enabled = true`, requires a bearer token when one is set,
  and refuses to be enabled without a token on a non-loopback bind.
- **Error responses include the error message.** The readers are the people
  running the estate, and a blank 500 wastes their afternoon; but a message can
  name a path or a config field.
- **The Windows binary is unsigned.** CI cannot reach the code-signing
  certificate. Verify it against `SHA256SUMS` and the build-provenance
  attestation — `gh attestation verify <file> --repo issinoho/loadbearer-fleet`
  — rather than by publisher.
- **Result documents embed a machine inventory**, so the collection folder, the
  index, the archive and a `backup` snapshot are all machine fingerprints. See
  [Removing a machine](README.md#removing-a-machine--deleting-its-file-is-not-enough):
  deleting a result file is *not* enough to remove it from the dashboard.

If you're unsure whether something is a genuine vulnerability or one of the
above, report it anyway — that's a reasonable thing to ask.

## What has and hasn't been verified

Being straight about this, because "it has tests" is not the same as "it has
been audited":

- **Covered by tests**, driven through the real router with real session
  cookies: that an anonymous caller gets nothing, that a forged cookie is not a
  session, that a viewer cannot rescan, that a tag-scoped viewer cannot widen
  their scope through the query string, that a machine outside their scope is
  indistinguishable from one that doesn't exist, that both halves of the
  sign-in CSRF defence are required, that the post-sign-in redirect cannot
  leave the site, and that sessions expire and are stored only as hashes.
  Also, from the input side: that a real compression bomb — built in the test,
  not mocked — is refused *before* it is decompressed, that an oversized plain
  document is refused on its size alone, and that half-finished sign-ins cannot
  grow without limit. Each of those three was watched failing with its guard
  removed before being kept.
- **Not verified end to end:** the authorization-code exchange and ID-token
  verification, which need a live identity provider. Those rest on the
  [`openidconnect`](https://crates.io/crates/openidconnect) crate. Discovery,
  issuer validation and TLS *were* checked against a real Entra endpoint.
- **No penetration test, and no third-party review.**

## Other measures already in place

- [Dependabot](https://github.com/issinoho/loadbearer-fleet/security/dependabot)
  for both GitHub Actions and Cargo (see `.github/dependabot.yml`).
- CI runs `clippy -D warnings` and the test suite on Linux and Windows, and
  checks the declared MSRV on both.
- TLS is [rustls](https://github.com/rustls/rustls) — there is no OpenSSL in
  the dependency tree.
