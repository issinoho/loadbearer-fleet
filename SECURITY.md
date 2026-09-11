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
- **reads** the collection folder, and never writes to it **unless
  `[server] upload_dir` is set** — which is off by default, and which has to
  name a directory *inside* the collection folder. It also **writes** the
  index, and the document archive if `archive_dir` is set. Nothing here ever
  deletes from the collection folder;
- **accepts submitted results only through one route.** `POST /api/upload`,
  from a signed-in `contributor` or an ingest token. Five things are checked
  before a byte is kept, and the first two — who the caller is, and whether
  they are inside their per-credential rate limit — are answered **before the
  request body is read at all**, so an unauthenticated or over-limit caller
  cannot make the server buffer a 16 MB document. Then: that an `upload_dir`
  exists, that the caller may upload, that the body is `application/json`
  (which no cross-origin form can send, on top of `SameSite=Lax`), and that
  the document parses and falls within their tag scope. The stored filename is
  built from the *document* and sanitised, never from anything the request
  chose, and the file is **created** rather than opened — so a symlink planted
  in the upload directory is an error rather than a way to have the service
  write somewhere it did not choose;
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

It stores no user credentials — no passwords, and no tokens from the identity
provider are kept after a sign-in. `client_secret` exists in the config for
providers that require one, but the intended shape is a public client with
PKCE, so the field is normally empty.

Two bearer credentials **do** live in the configuration file in plain text when
they are used: `[metrics] token`, and each `[[ingest.tokens]]` entry. They are
compared by digesting both sides, so neither its length nor a prefix leaks
through timing, but the file itself is the store — treat it as one. `640
root:<service account>` is what the runbook installs it as, and revoking is
deleting the entry and restarting.

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
- **A contributor can fabricate a result.** Tag scoping on submission is
  containment, not prevention: the tags are checked against what the uploader
  sent, so a contributor scoped to `site=glasgow` can invent a machine tagged
  `site=glasgow`. What it stops is them reaching Edinburgh. An *unscoped*
  contributor is unconstrained, because an empty scope means the whole fleet
  everywhere else in this tool. Give the ability to submit to the people you
  would give the collection folder's write permission to.
- **An ingest token may write and may not read.** It is accepted on
  `/api/upload` and nowhere else — every reading endpoint requires a session
  and never looks at an `Authorization` header — so a token leaking out of a
  deployment script leaks a write path rather than the estate. It is also,
  deliberately, always a `contributor` and never more.
- **The rate limit counts submissions, not bytes.** At the default of 120 a
  minute, a credential submitting *maximum-sized* documents rather than the
  usual 40 KB ones can still write faster than the "40 KB at a time" framing
  suggests. Lower it, or set a filesystem quota, if the upload directory
  shares a volume with something that matters.
- **The upload rate limit is a safety valve, not DoS protection.** It bounds
  what one credential can write, which is the runaway-script and leaked-token
  case. Absorbing a flood is the reverse proxy's job; unauthenticated requests
  never reach the limiter, because they are refused before there is a
  principal to count them against.
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
  On the write path: that uploading is refused with no `upload_dir`, for the
  wrong role, for the wrong content type and for a document outside the
  caller's scope; that an unknown ingest token is refused *without* falling
  back to whatever session sat beside it; that a token is refused on every
  reading endpoint; that a metrics token is not an ingest token and vice
  versa; that a hostile hostname cannot escape the upload directory; and that
  a credential over its rate limit is refused before the document is parsed or
  written. The token fall-through and the rate limit were each watched failing
  with the guard removed before being kept.
  Also, from the input side: that a real compression bomb — built in the test,
  not mocked — is refused *before* it is decompressed, that an oversized plain
  document is refused on its size alone, and that half-finished sign-ins cannot
  grow without limit. Each of those three was watched failing with its guard
  removed before being kept.
- **Exercised against a live provider**, on 2026-09-11, against a self-hosted
  **Authelia 4.39.25**: discovery, issuer validation, TLS against the public
  certificate, the authorization request with PKCE `S256`, the
  authorization-code exchange as a public client with no secret, ID-token
  verification, and a group claim read out of that token and mapped to a role.
  A full sign-in reached the dashboard with the role its grant specified.

  That closes what this file previously listed as unverified. It was one run
  through one provider, so read it as "the path has been walked" rather than
  "the path has been tested" — there is no automated coverage of the exchange,
  because there is no provider to run it against in CI. Discovery, issuer
  validation and TLS were separately checked against a real Entra endpoint.
- **No penetration test, and no third-party review.**

## Other measures already in place

- [Dependabot](https://github.com/issinoho/loadbearer-fleet/security/dependabot)
  for both GitHub Actions and Cargo (see `.github/dependabot.yml`).
- CI runs `clippy -D warnings` and the test suite on Linux and Windows, and
  checks the declared MSRV on both.
- TLS is [rustls](https://github.com/rustls/rustls) — there is no OpenSSL in
  the dependency tree.
