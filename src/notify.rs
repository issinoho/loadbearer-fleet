//! Outbound notifications: one JSON POST to a URL the operator configures,
//! when a scan finds a new critical finding and, if asked, as a digest.
//!
//! ## Off unless configured
//!
//! With no `[notify] webhook_url` nothing here is built, and the process opens
//! no connection it did not open before. Generic on purpose: one documented
//! payload, no Teams or Slack card format. A receiver that wants one builds it
//! from this; the `text` field is there for one that only wants a line.
//!
//! ## What counts as news
//!
//! After each rebuild of the analysis, the **live critical** findings in the
//! new snapshot — fleet-wide, unscoped, with triage applied exactly as
//! `/metrics` applies it — are compared with the findings that were critical
//! in the snapshot it replaced, *before* triage. A finding is "the same
//! finding" if it is the same rule on the same machine, as everywhere else.
//! What is in the first and not the second is sent, all of it in one POST.
//!
//! Comparing against the underlying snapshot rather than the live one is what
//! keeps triage out of it. A snooze running out, or an acknowledgement being
//! cleared, makes a finding live again without making it new, and is not
//! sent. An acknowledgement of a *warning* that lapses because the finding has
//! become critical is sent, because the finding was not critical before. A
//! suppressed or acknowledged finding is never live, so never sent. Changing
//! triage rebuilds nothing, so it can never send anything by itself.
//!
//! **An empty baseline sends nothing.** When the snapshot being replaced holds
//! no machines at all — the first scan ever, or an index rebuilt after an
//! upgrade — every critical finding in the estate would be "new". The new
//! snapshot becomes the baseline instead, and the log says so. The baseline at
//! startup is the index as it stood, so the startup scan still finds what
//! arrived in the folder while the server was down.
//!
//! The baseline is held in memory and nothing is recorded about what was
//! sent. Under the rules above that loses nothing a scan can see: a restart
//! starts from the index, which holds what the last run had seen. What it can
//! miss is a finding that was already in the index when the process started —
//! put there by a `scan` from a terminal while the server was down — which
//! the digest exists to catch.
//!
//! ## Never on the scan path
//!
//! Raising an event serialises it and puts it on a bounded queue; a task of
//! its own sends it. The scan, and the request that triggered it, neither wait
//! for the receiver nor fail because of it. When the queue is full the *new*
//! event is dropped, logged and counted: what is already queued is older news
//! that has waited its turn, and the digest is the backstop.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use openidconnect::reqwest;
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, Time, UtcOffset, Weekday};
use tokio::sync::mpsc;

use crate::analytics::{BySeverity, Changes, Flag, QuietCounts, Severity, Snapshot};
use crate::config::{Config, DigestSchedule};

/// The header a signed request carries: `sha256=<lowercase hex>`.
pub const SIGNATURE_HEADER: &str = "X-Loadbearer-Signature";

/// Events waiting to be sent. A scan raises at most one, and a digest one a
/// day, so a full queue means the receiver has been failing for a while.
const QUEUE: usize = 16;

/// Tries per event, the first included.
const ATTEMPTS: u32 = 3;

/// For the whole of one try: connecting, sending and the answer.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Waited before the second try, twice this before the third.
const BACKOFF: Duration = Duration::from_secs(2);

/// The most findings one payload lists. The rest are counted in `more`, so a
/// receiver relaying to a chat channel is not handed a thousand lines.
pub const MAX_FINDINGS: usize = 50;

/// How late a digest may still go. A timer that wakes well after its minute —
/// a host resumed from sleep — skips it rather than sending yesterday's
/// summary at an hour nobody chose.
const LATE: time::Duration = time::Duration::minutes(5);

/// What a request is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Critical,
    Digest,
    Test,
}

impl Event {
    /// As it appears in the payload's `event` field and in the metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "findings.critical",
            Self::Digest => "digest",
            Self::Test => "test",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Sent,
    Failed,
    Dropped,
}

const OUTCOMES: [(Outcome, &str); 3] = [
    (Outcome::Sent, "sent"),
    (Outcome::Failed, "failed"),
    (Outcome::Dropped, "dropped"),
];

/// How each event fared, for `/metrics`. Shared with the sending task, so
/// plain atomics rather than the runtime lock.
#[derive(Debug, Default)]
pub struct Counters {
    /// `[critical, digest][sent, failed, dropped]`. A test event is only ever
    /// sent from the command line, by a process with no metrics.
    cells: [[AtomicU64; 3]; 2],
}

impl Counters {
    fn add(&self, event: Event, outcome: Outcome) {
        let row = match event {
            Event::Critical => 0,
            Event::Digest => 1,
            Event::Test => return,
        };
        self.cells[row][outcome as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// `(event, result, count)` for every combination, zeros included, so an
    /// alert on `failed` has a series to watch before the first failure.
    pub fn rows(&self) -> Vec<(&'static str, &'static str, u64)> {
        let mut out = Vec::with_capacity(6);
        for (row, event) in [Event::Critical, Event::Digest].into_iter().enumerate() {
            for (outcome, name) in OUTCOMES {
                out.push((
                    event.as_str(),
                    name,
                    self.cells[row][outcome as usize].load(Ordering::Relaxed),
                ));
            }
        }
        out
    }
}

/// One event, ready to send: the body is fixed when the event is raised, so
/// every try sends, and signs, the same bytes.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub event: Event,
    pub body: Vec<u8>,
}

/// One finding as the payload carries it.
#[derive(Debug, Serialize)]
struct Finding {
    machine_key: String,
    hostname: Option<String>,
    code: &'static str,
    severity: Severity,
    headline: String,
    detail: String,
    /// The machine's page on the dashboard.
    url: String,
}

/// Every payload: the same four fields first, then what the event adds.
#[derive(Debug, Serialize)]
struct Envelope<'a> {
    event: &'static str,
    sent_at: String,
    server: &'a str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<DigestSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    findings: Option<Vec<Finding>>,
    /// Findings left out of `findings` by the cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    more: Option<usize>,
}

#[derive(Debug, Serialize)]
struct DigestSummary {
    period: &'static str,
    machines: usize,
    /// Machines with a live critical or warning finding.
    machines_flagged: usize,
    live: BySeverity,
    quieted: QuietCounts,
    /// Weekly only, and null when nothing had been measured a week earlier.
    changes: Option<DigestChanges>,
}

/// `Timeline::changes`, counted: judged by the rules, as "What changed" is,
/// so triage plays no part.
#[derive(Debug, Serialize)]
struct DigestChanges {
    since: String,
    new_findings: BySeverity,
    new_machine_findings: BySeverity,
    resolved: BySeverity,
    worsened: usize,
    eased: usize,
    machines_added: usize,
}

/// HMAC-SHA256 (RFC 2104) over `sha2`, rather than a crate for fifteen
/// lines. Checked against RFC 4231's vectors in the tests.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// The signature header's value for this body.
pub fn signature(secret: &[u8], body: &[u8]) -> String {
    format!("sha256={}", hex(&hmac_sha256(secret, body)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What JavaScript's `encodeURIComponent` does, because the dashboard decodes
/// a machine's route with `decodeURIComponent` and the link has to survive it.
fn encode_component(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => (b as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// A machine's page, as the dashboard routes to it: `#/machine/<key>`.
pub fn machine_url(server: &str, machine_key: &str) -> String {
    format!(
        "{}/#/machine/{}",
        server.trim_end_matches('/'),
        encode_component(machine_key)
    )
}

fn name(f: &Flag) -> &str {
    f.hostname.as_deref().unwrap_or(&f.machine_key)
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

fn findings(server: &str, flags: &[&Flag]) -> (Vec<Finding>, usize) {
    let listed = flags
        .iter()
        .take(MAX_FINDINGS)
        .map(|f| Finding {
            machine_key: f.machine_key.clone(),
            hostname: f.hostname.clone(),
            code: f.code,
            severity: f.severity,
            headline: f.headline.clone(),
            detail: f.detail.clone(),
            url: machine_url(server, &f.machine_key),
        })
        .collect();
    (listed, flags.len().saturating_sub(MAX_FINDINGS))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn body(envelope: &Envelope) -> Vec<u8> {
    // Nothing in these types can fail to serialise.
    serde_json::to_vec(envelope).unwrap_or_default()
}

/// The critical findings in `after` that are news against `before`, or `None`
/// when `before` is an empty baseline. See the module notes for the rule.
///
/// `before` is the replaced snapshot as computed, without triage; `after` is
/// the new one with triage applied, so a quieted finding is not live.
pub fn new_criticals<'a>(before: &Snapshot, after: &'a Snapshot) -> Option<Vec<&'a Flag>> {
    if before.machines.is_empty() {
        return None;
    }
    let was: HashSet<(&str, &str)> = before
        .flags
        .iter()
        .filter(|f| f.severity == Severity::Critical)
        .map(|f| (f.machine_key.as_str(), f.code))
        .collect();
    // One per finding: `component_weak` can raise the rule more than once on
    // a machine, and it is one finding everywhere else.
    let mut listed = HashSet::new();
    Some(
        after
            .flags
            .iter()
            .filter(|f| f.severity == Severity::Critical && f.is_live())
            .filter(|f| {
                let key = (f.machine_key.as_str(), f.code);
                !was.contains(&key) && listed.insert(key)
            })
            .collect(),
    )
}

/// `findings.critical`: the new critical findings from one rebuild.
pub fn critical_event(server: &str, flags: &[&Flag], now: OffsetDateTime) -> Outgoing {
    let machines: HashSet<&str> = flags.iter().map(|f| f.machine_key.as_str()).collect();
    let text = match flags {
        [one] => format!(
            "loadbearer-fleet: new critical finding on {}: {}",
            name(one),
            one.headline
        ),
        [first, rest @ ..] => format!(
            "loadbearer-fleet: {} on {}\n{}: {} (and {} more)",
            plural(flags.len(), "new critical finding", "new critical findings"),
            plural(machines.len(), "machine", "machines"),
            name(first),
            first.headline,
            rest.len()
        ),
        [] => "loadbearer-fleet: no new critical findings".to_string(),
    };
    let (listed, more) = findings(server, flags);
    Outgoing {
        event: Event::Critical,
        body: body(&Envelope {
            event: Event::Critical.as_str(),
            sent_at: rfc3339(now),
            server,
            text,
            summary: None,
            findings: Some(listed),
            more: Some(more),
        }),
    }
}

/// `digest`: the fleet as an unscoped Overview shows it. `live` is the
/// snapshot with triage applied; `changes` is the week's, for a weekly digest.
pub fn digest_event(
    server: &str,
    period: DigestSchedule,
    live: &Snapshot,
    changes: Option<&Changes>,
    now: OffsetDateTime,
) -> Outgoing {
    let s = &live.summary;
    let critical: Vec<&Flag> = live
        .flags
        .iter()
        .filter(|f| f.severity == Severity::Critical && f.is_live())
        .collect();
    let changes = changes
        .filter(|c| !c.baseline_empty)
        .map(|c| DigestChanges {
            since: c.from.clone(),
            new_findings: c.counts.new_findings,
            new_machine_findings: c.counts.new_machine_findings,
            resolved: c.counts.resolved,
            worsened: c.counts.worsened,
            eased: c.counts.eased,
            machines_added: c.counts.machines_added,
        });

    let mut text = format!(
        "loadbearer-fleet {} digest: {} critical, {} warning across {} of {}",
        period.as_str(),
        s.critical,
        s.warnings,
        s.machines_flagged,
        plural(s.machines, "machine", "machines"),
    );
    if s.quieted.total() > 0 {
        text.push_str(&format!("; {} quieted", s.quieted.total()));
    }
    if let Some(c) = &changes {
        let new = c.new_findings.critical + c.new_findings.warning + c.new_findings.info;
        let resolved = c.resolved.critical + c.resolved.warning + c.resolved.info;
        text.push_str(&format!(
            "\nSince {}: {new} new, {resolved} resolved, {} worse",
            c.since, c.worsened
        ));
    }

    let (listed, more) = findings(server, &critical);
    Outgoing {
        event: Event::Digest,
        body: body(&Envelope {
            event: Event::Digest.as_str(),
            sent_at: rfc3339(now),
            server,
            text,
            summary: Some(DigestSummary {
                period: period.as_str(),
                machines: s.machines,
                machines_flagged: s.machines_flagged,
                live: BySeverity {
                    critical: s.critical,
                    warning: s.warnings,
                    info: s.info,
                },
                quieted: s.quieted,
                changes,
            }),
            findings: Some(listed),
            more: Some(more),
        }),
    }
}

/// `test`: nothing about the fleet, only that the wiring works.
pub fn test_event(server: &str, now: OffsetDateTime) -> Outgoing {
    Outgoing {
        event: Event::Test,
        body: body(&Envelope {
            event: Event::Test.as_str(),
            sent_at: rfc3339(now),
            server,
            text: format!(
                "loadbearer-fleet: test notification from {server}. If you can read this, the \
                 webhook works."
            ),
            summary: None,
            findings: None,
            more: None,
        }),
    }
}

/// The next time a digest is due, strictly after `now`: `at` (UTC) on the
/// next day, or the next Monday for a weekly one. `None` when it is off.
pub fn next_slot(
    now: OffsetDateTime,
    schedule: DigestSchedule,
    at: Time,
) -> Option<OffsetDateTime> {
    if schedule == DigestSchedule::Off {
        return None;
    }
    let now = now.to_offset(UtcOffset::UTC);
    let mut day = now.date();
    // Eight days covers "today has passed and the next Monday is a week out".
    for _ in 0..8 {
        let slot = day.with_time(at).assume_utc();
        if slot > now && (schedule == DigestSchedule::Daily || day.weekday() == Weekday::Monday) {
            return Some(slot);
        }
        day = day.next_day()?;
    }
    None
}

/// Whether a digest whose slot was `slot` may still go at `now`. See `LATE`.
pub fn still_due(slot: OffsetDateTime, now: OffsetDateTime) -> bool {
    now - slot <= LATE
}

/// Why a delivery did not land.
#[derive(Debug)]
pub enum Failure {
    /// The receiver answered, and not with a 2xx.
    Status(reqwest::StatusCode),
    /// No answer: could not connect, timed out, or the connection broke.
    Transport(String),
}

impl Failure {
    /// A receiver that is down or overloaded may not be in a second; one that
    /// refused the request will refuse it again.
    fn worth_retrying(&self) -> bool {
        match self {
            Self::Status(s) => s.is_server_error() || *s == reqwest::StatusCode::TOO_MANY_REQUESTS,
            Self::Transport(_) => true,
        }
    }

    pub fn status(&self) -> Option<reqwest::StatusCode> {
        match self {
            Self::Status(s) => Some(*s),
            Self::Transport(_) => None,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(s) => write!(f, "answered {s}"),
            Self::Transport(why) => write!(f, "{why}"),
        }
    }
}

/// An HTTP error with the URL taken out, and its causes appended: reqwest's
/// own message names the URL, and a webhook URL is often a credential.
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut text = e.to_string();
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        text.push_str(": ");
        text.push_str(&c.to_string());
        cause = c.source();
    }
    text
}

/// Where events go, and the client that takes them there.
pub struct Target {
    url: reqwest::Url,
    secret: Option<Vec<u8>>,
    client: reqwest::Client,
    backoff: Duration,
    /// Scheme and host only, for the log. Never the path or the query, which
    /// is where a webhook URL keeps its token.
    label: String,
}

impl Target {
    /// `None` when no webhook is configured.
    pub fn new(config: &Config) -> Result<Option<Self>> {
        let Some(raw) = &config.notify.webhook_url else {
            return Ok(None);
        };
        let url = crate::config::webhook_url(raw)?;
        let mut builder = reqwest::ClientBuilder::new()
            // A receiver that redirects is not the receiver that was
            // configured, and following one would let it point this
            // somewhere inside the network: the same SSRF reasoning as the
            // identity provider's client.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .user_agent(concat!("loadbearer-fleet/", env!("CARGO_PKG_VERSION")));
        if let Some(path) = &config.auth.ca_bundle {
            for cert in crate::auth::ca_bundle(path)? {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder
            .build()
            .context("building the HTTP client for the webhook")?;
        let label = format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or_default(),
            url.port().map(|p| format!(":{p}")).unwrap_or_default()
        );
        Ok(Some(Self {
            url,
            secret: (!config.notify.secret.is_empty())
                .then(|| config.notify.secret.as_bytes().to_vec()),
            client,
            backoff: BACKOFF,
            label,
        }))
    }

    /// Scheme and host, safe to log and to print.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Send one event, trying up to three times on no answer, a 5xx or a 429
    /// and not at all again on any other answer. Logs the outcome.
    pub async fn deliver(&self, out: &Outgoing) -> Result<reqwest::StatusCode, Failure> {
        let event = out.event.as_str();
        let mut attempt = 0;
        let failure = loop {
            attempt += 1;
            let mut request = self
                .client
                .post(self.url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(out.body.clone());
            if let Some(secret) = &self.secret {
                request = request.header(SIGNATURE_HEADER, signature(secret, &out.body));
            }
            let failure = match request.send().await {
                Ok(response) if response.status().is_success() => {
                    let status = response.status();
                    tracing::info!(
                        event,
                        status = status.as_u16(),
                        target = %self.label,
                        attempt,
                        "notification sent"
                    );
                    return Ok(status);
                }
                Ok(response) => Failure::Status(response.status()),
                Err(e) => Failure::Transport(describe(e)),
            };
            if attempt >= ATTEMPTS || !failure.worth_retrying() {
                break failure;
            }
            tracing::debug!(event, target = %self.label, attempt, error = %failure, "retrying a notification");
            tokio::time::sleep(self.backoff * attempt).await;
        };
        tracing::warn!(
            event,
            status = failure.status().map(|s| s.as_u16()),
            target = %self.label,
            attempts = attempt,
            error = %failure,
            "notification not delivered"
        );
        Err(failure)
    }
}

/// Raises events and hands them to the task that sends them. Present only
/// when a webhook is configured.
pub struct Notifier {
    target: Arc<Target>,
    /// `public_url`: the payload's `server`, and what machine links start with.
    server: String,
    critical: bool,
    digest: Option<(DigestSchedule, Time)>,
    queue: mpsc::Sender<Outgoing>,
    /// Taken by `start`. Events raised before then — by the startup scan,
    /// which runs before there is a runtime to send from — wait here.
    waiting: Mutex<Option<mpsc::Receiver<Outgoing>>>,
    counters: Arc<Counters>,
    #[cfg(test)]
    raised: AtomicU64,
}

impl Notifier {
    /// `None` when no webhook is configured. Fails on a CA bundle that cannot
    /// be read, rather than starting with a webhook that can never connect.
    pub fn new(config: &Config, counters: Arc<Counters>) -> Result<Option<Self>> {
        let Some(target) = Target::new(config)? else {
            return Ok(None);
        };
        let (queue, waiting) = mpsc::channel(QUEUE);
        let digest = match (config.notify.digest, config.notify.digest_time()) {
            (DigestSchedule::Off, _) | (_, None) => None,
            (schedule, Some(at)) => Some((schedule, at)),
        };
        Ok(Some(Self {
            target: Arc::new(target),
            server: config.server.public_url.trim_end_matches('/').to_string(),
            critical: config.notify.critical,
            digest,
            queue,
            waiting: Mutex::new(Some(waiting)),
            counters,
            #[cfg(test)]
            raised: AtomicU64::new(0),
        }))
    }

    pub fn critical(&self) -> bool {
        self.critical
    }

    pub fn digest(&self) -> Option<(DigestSchedule, Time)> {
        self.digest
    }

    pub fn target(&self) -> &str {
        self.target.label()
    }

    /// Start sending. Needs a runtime; does nothing the second time.
    pub fn start(&self) {
        let Some(mut waiting) = self.waiting.lock().expect("notify queue lock").take() else {
            return;
        };
        let target = Arc::clone(&self.target);
        let counters = Arc::clone(&self.counters);
        tokio::spawn(async move {
            // One at a time, in order. A receiver that is down holds the
            // queue for at most three timeouts per event, and the queue's
            // bound is what keeps that from growing.
            while let Some(out) = waiting.recv().await {
                let outcome = match target.deliver(&out).await {
                    Ok(_) => Outcome::Sent,
                    Err(_) => Outcome::Failed,
                };
                counters.add(out.event, outcome);
            }
        });
    }

    /// After a rebuild: send what is newly critical and live, if anything is.
    /// `before` is the snapshot replaced, as computed; `after` is the new one
    /// projected with triage.
    pub fn after_rebuild(&self, before: &Snapshot, after: &Snapshot, now: OffsetDateTime) {
        if !self.critical {
            return;
        }
        match new_criticals(before, after) {
            None => {
                if !after.machines.is_empty() {
                    tracing::info!(
                        machines = after.machines.len(),
                        critical = after.summary.critical,
                        "no fleet to compare against yet: taking this scan as the baseline for \
                         critical notifications, and sending nothing for it"
                    );
                }
            }
            Some(new) if new.is_empty() => {}
            Some(new) => self.raise(critical_event(&self.server, &new, now)),
        }
    }

    /// Raise the digest for `period`. See `digest_event`.
    pub fn raise_digest(
        &self,
        period: DigestSchedule,
        live: &Snapshot,
        changes: Option<&Changes>,
        now: OffsetDateTime,
    ) {
        self.raise(digest_event(&self.server, period, live, changes, now));
    }

    fn raise(&self, out: Outgoing) {
        #[cfg(test)]
        self.raised.fetch_add(1, Ordering::Relaxed);
        let event = out.event;
        if let Err(e) = self.queue.try_send(out) {
            self.counters.add(event, Outcome::Dropped);
            let why = match e {
                mpsc::error::TrySendError::Full(_) => {
                    "the queue is full: the receiver has been slow or down for a while"
                }
                mpsc::error::TrySendError::Closed(_) => "nothing is sending any more",
            };
            tracing::warn!(event = event.as_str(), reason = why, "notification dropped");
        }
    }

    /// How many events have been raised, sent or not.
    #[cfg(test)]
    pub fn raised(&self) -> u64 {
        self.raised.load(Ordering::Relaxed)
    }
}

/// `notify test`: send a `test` event to the configured webhook and return
/// where it went and what it answered. The same client, the same signing
/// and the same retries as a real event.
pub async fn send_test(config: &Config) -> Result<(String, reqwest::StatusCode)> {
    let Some(target) = Target::new(config)? else {
        anyhow::bail!(
            "no [notify] webhook_url is configured, so there is nothing to send a test to. Pass \
             --config with the file that sets it"
        );
    };
    let out = test_event(
        config.server.public_url.trim_end_matches('/'),
        OffsetDateTime::now_utc(),
    );
    match target.deliver(&out).await {
        Ok(status) => Ok((target.label().to_string(), status)),
        Err(failure) => anyhow::bail!(
            "the webhook at {} did not accept the test event: {failure}",
            target.label()
        ),
    }
}

/// A receiver on 127.0.0.1, for the tests here and in `web`: it records what
/// arrives and answers with the statuses it is given, in order, then 204.
#[cfg(test)]
pub(crate) mod receiver {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;

    #[derive(Debug, Clone)]
    pub struct Received {
        pub headers: HeaderMap,
        pub body: Vec<u8>,
    }

    impl Received {
        pub fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).expect("a JSON body")
        }

        pub fn header(&self, name: &str) -> Option<String> {
            Some(self.headers.get(name)?.to_str().ok()?.to_string())
        }
    }

    struct Script {
        seen: Mutex<Vec<Received>>,
        answers: Mutex<VecDeque<u16>>,
        delay: Duration,
    }

    pub struct Receiver {
        /// Where to point `webhook_url`.
        pub url: String,
        script: Arc<Script>,
    }

    async fn take(State(s): State<Arc<Script>>, headers: HeaderMap, body: Bytes) -> StatusCode {
        // Recorded before the delay, so a test can see that a request arrived
        // while the receiver is still sitting on it.
        s.seen.lock().expect("seen").push(Received {
            headers,
            body: body.to_vec(),
        });
        if !s.delay.is_zero() {
            tokio::time::sleep(s.delay).await;
        }
        let code = s
            .answers
            .lock()
            .expect("answers")
            .pop_front()
            .unwrap_or(204);
        StatusCode::from_u16(code).expect("a status")
    }

    pub async fn start(answers: &[u16], delay: Duration) -> Receiver {
        let script = Arc::new(Script {
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(answers.iter().copied().collect()),
            delay,
        });
        let app = Router::new()
            .route("/hook", post(take))
            .with_state(Arc::clone(&script));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Receiver {
            // A query string, as a real webhook URL often has, so the tests
            // can check it never reaches the log.
            url: format!("http://{addr}/hook?token=do-not-log-me"),
            script,
        }
    }

    impl Receiver {
        pub fn seen(&self) -> Vec<Received> {
            self.script.seen.lock().expect("seen").clone()
        }

        /// Wait until at least `n` requests have arrived, and return them.
        pub async fn wait_for(&self, n: usize) -> Vec<Received> {
            for _ in 0..1000 {
                let seen = self.seen();
                if seen.len() >= n {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("{n} request(s) never arrived; {} did", self.seen().len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::receiver;
    use super::*;
    use crate::analytics::{FlagKind, Tuning};
    use crate::index::Index;
    use std::path::Path;
    use time::macros::{datetime, time};

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// RFC 4231, test cases 1 to 4, 6 and 7. Case 5 is a truncated output,
    /// which nothing here produces.
    #[test]
    fn hmac_matches_rfc_4231() {
        let long_key = vec![0xaa; 131];
        let key_25: Vec<u8> = (1..=25).collect();
        let cases: [(&[u8], &[u8], &str); 6] = [
            (
                &[0x0b; 20],
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe",
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                &[0xaa; 20],
                &[0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                &key_25,
                &[0xcd; 50],
                "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
            ),
            (
                &long_key,
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
            (
                &long_key,
                b"This is a test using a larger than block-size key and a larger than \
                  block-size data. The key needs to be hashed before being used by the HMAC \
                  algorithm.",
                "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
            ),
        ];
        for (i, (key, data, want)) in cases.iter().enumerate() {
            assert_eq!(
                hmac_sha256(key, data).to_vec(),
                unhex(want),
                "RFC 4231 case {}",
                if i < 4 { i + 1 } else { i + 2 }
            );
        }
        assert_eq!(
            signature(b"Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_daily_digest_is_due_at_the_next_occurrence_of_its_time() {
        let at = time!(08:00);
        let daily = |now| next_slot(now, DigestSchedule::Daily, at);
        // Before the time today: today.
        assert_eq!(
            daily(datetime!(2026-11-12 07:59:59 UTC)),
            Some(datetime!(2026-11-12 08:00 UTC))
        );
        // At it exactly: it has just gone, so tomorrow. Asked again straight
        // after sending, a slot must not come round twice.
        assert_eq!(
            daily(datetime!(2026-11-12 08:00 UTC)),
            Some(datetime!(2026-11-13 08:00 UTC))
        );
        // Across midnight, a month end and a year end.
        assert_eq!(
            daily(datetime!(2026-11-30 23:59 UTC)),
            Some(datetime!(2026-12-01 08:00 UTC))
        );
        assert_eq!(
            daily(datetime!(2026-12-31 09:00 UTC)),
            Some(datetime!(2027-01-01 08:00 UTC))
        );
        // UTC whatever offset the clock was read in: 09:30 at +02:00 is 07:30
        // UTC, so 08:00 UTC today is still ahead.
        assert_eq!(
            daily(datetime!(2026-11-12 09:30 +02:00)),
            Some(datetime!(2026-11-12 08:00 UTC))
        );
        // Midnight itself.
        assert_eq!(
            next_slot(
                datetime!(2026-11-12 23:00 UTC),
                DigestSchedule::Daily,
                time!(00:00)
            ),
            Some(datetime!(2026-11-13 00:00 UTC))
        );
        assert_eq!(
            next_slot(datetime!(2026-11-12 07:00 UTC), DigestSchedule::Off, at),
            None
        );
    }

    #[test]
    fn a_weekly_digest_is_due_on_the_next_monday() {
        let at = time!(08:00);
        let weekly = |now| next_slot(now, DigestSchedule::Weekly, at);
        // 2026-11-12 is a Thursday; 2026-11-16 the next Monday.
        assert_eq!(
            weekly(datetime!(2026-11-12 12:00 UTC)),
            Some(datetime!(2026-11-16 08:00 UTC))
        );
        // Monday before the time: today. Monday after it: a week on.
        assert_eq!(
            weekly(datetime!(2026-11-16 07:00 UTC)),
            Some(datetime!(2026-11-16 08:00 UTC))
        );
        assert_eq!(
            weekly(datetime!(2026-11-16 08:00 UTC)),
            Some(datetime!(2026-11-23 08:00 UTC))
        );
        // Sunday night into Monday, across a month and a year.
        assert_eq!(
            weekly(datetime!(2026-11-29 23:59 UTC)),
            Some(datetime!(2026-11-30 08:00 UTC))
        );
        assert_eq!(
            weekly(datetime!(2026-12-29 10:00 UTC)),
            Some(datetime!(2027-01-04 08:00 UTC))
        );
        for slot in [
            weekly(datetime!(2026-11-12 12:00 UTC)),
            weekly(datetime!(2027-02-27 12:00 UTC)),
        ] {
            assert_eq!(slot.expect("a slot").weekday(), Weekday::Monday);
        }
    }

    /// A timer that wakes long after its minute skips the digest rather than
    /// sending it at an hour nobody chose.
    #[test]
    fn a_digest_woken_late_is_skipped_not_sent() {
        let slot = datetime!(2026-11-16 08:00 UTC);
        assert!(still_due(slot, datetime!(2026-11-16 08:00:01 UTC)));
        assert!(still_due(slot, datetime!(2026-11-16 08:05 UTC)));
        assert!(!still_due(slot, datetime!(2026-11-16 08:05:01 UTC)));
        assert!(!still_due(slot, datetime!(2026-11-16 19:00 UTC)));
    }

    #[test]
    fn a_machine_link_is_the_route_the_dashboard_decodes() {
        assert_eq!(
            machine_url("https://fleet.example.com/", "uuid:1234-ab"),
            "https://fleet.example.com/#/machine/uuid%3A1234-ab"
        );
        // What decodeURIComponent would need: a slash, a space and a
        // non-ASCII hostname all escaped, byte by byte.
        assert_eq!(encode_component("a/b c\u{e9}"), "a%2Fb%20c%C3%A9");
        assert_eq!(encode_component("A-z_0.9!~*'()"), "A-z_0.9!~*'()");
    }

    fn flag(key: &str, host: &str, code: &'static str) -> Flag {
        Flag {
            severity: Severity::Critical,
            kind: FlagKind::Machine,
            code,
            machine_key: key.to_string(),
            hostname: Some(host.to_string()),
            headline: format!("{code} headline"),
            detail: format!("{code} detail"),
            triage: None,
        }
    }

    fn keys(v: &serde_json::Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().expect("an object").keys().cloned().collect();
        k.sort();
        k
    }

    /// The payload is an interface somebody else's code reads, so its field
    /// names are pinned here: renaming one breaks a receiver silently.
    #[test]
    fn the_critical_payload_has_the_documented_shape() {
        let now = datetime!(2026-11-12 09:15 UTC);
        let a = flag("uuid:a", "PC-01", "drive_failing");
        let b = flag("uuid:a", "PC-01", "disk_nearly_full");
        let c = flag("uuid:c", "PC-03", "drive_failing");
        let out = critical_event("https://fleet.example.com", &[&a, &b, &c], now);
        assert_eq!(out.event, Event::Critical);
        let v: serde_json::Value = serde_json::from_slice(&out.body).expect("JSON");
        assert_eq!(
            keys(&v),
            ["event", "findings", "more", "sent_at", "server", "text"]
        );
        assert_eq!(v["event"], "findings.critical");
        assert_eq!(v["sent_at"], "2026-11-12T09:15:00Z");
        assert_eq!(v["server"], "https://fleet.example.com");
        assert_eq!(v["more"], 0);
        assert_eq!(
            v["text"],
            "loadbearer-fleet: 3 new critical findings on 2 machines\n\
             PC-01: drive_failing headline (and 2 more)"
        );
        let f = &v["findings"][0];
        assert_eq!(
            keys(f),
            [
                "code",
                "detail",
                "headline",
                "hostname",
                "machine_key",
                "severity",
                "url"
            ]
        );
        assert_eq!(f["machine_key"], "uuid:a");
        assert_eq!(f["hostname"], "PC-01");
        assert_eq!(f["code"], "drive_failing");
        assert_eq!(f["severity"], "critical");
        assert_eq!(f["url"], "https://fleet.example.com/#/machine/uuid%3Aa");

        let one = critical_event("https://fleet.example.com", &[&c], now);
        let v: serde_json::Value = serde_json::from_slice(&one.body).expect("JSON");
        assert_eq!(
            v["text"],
            "loadbearer-fleet: new critical finding on PC-03: drive_failing headline"
        );
    }

    #[test]
    fn a_long_list_is_capped_and_the_rest_counted() {
        let many: Vec<Flag> = (0..MAX_FINDINGS + 7)
            .map(|i| flag(&format!("k{i}"), &format!("PC-{i}"), "drive_failing"))
            .collect();
        let refs: Vec<&Flag> = many.iter().collect();
        let out = critical_event("https://f", &refs, datetime!(2026-11-12 09:15 UTC));
        let v: serde_json::Value = serde_json::from_slice(&out.body).expect("JSON");
        assert_eq!(
            v["findings"].as_array().expect("a list").len(),
            MAX_FINDINGS
        );
        assert_eq!(v["more"], 7);
    }

    #[test]
    fn the_test_payload_says_nothing_about_the_fleet() {
        let out = test_event("https://fleet.example.com", datetime!(2026-11-12 09:15 UTC));
        let v: serde_json::Value = serde_json::from_slice(&out.body).expect("JSON");
        assert_eq!(keys(&v), ["event", "sent_at", "server", "text"]);
        assert_eq!(v["event"], "test");
    }

    fn fleet() -> Snapshot {
        let mut index = Index::open_in_memory().unwrap();
        for f in [
            "win-modern.json",
            "linux-throttled.json",
            "low-end.json",
            "legacy-1.2.4.json",
        ] {
            index
                .ingest_file(
                    &Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures")
                        .join(f),
                )
                .unwrap();
        }
        crate::analytics::snapshot(
            index.conn(),
            &Tuning::default(),
            datetime!(2026-09-12 12:00 UTC),
        )
        .unwrap()
    }

    /// A fleet whose every machine opted in to `--collect`, several without
    /// Secure Boot: neither payload says a word of it. A webhook goes to a
    /// chat channel, which is exactly where a list of unprotected machines
    /// should not end up.
    #[test]
    fn no_payload_carries_the_opt_in_facts() {
        use crate::schema::samples;
        let mut index = Index::open_in_memory().unwrap();
        for (f, enrich) in [
            (
                "win-modern.json",
                samples::optin_windows as fn(&mut serde_json::Value),
            ),
            ("linux-throttled.json", samples::optin_linux_luks),
            ("low-end.json", samples::optin_legacy_bios),
            ("legacy-1.2.4.json", samples::optin_linux_none_detected),
        ] {
            let mut doc = samples::fixture(f);
            enrich(&mut doc);
            index.ingest_text(&doc.to_string(), Path::new(f)).unwrap();
        }
        let snap = crate::analytics::snapshot(
            index.conn(),
            &Tuning::default(),
            datetime!(2026-09-12 12:00 UTC),
        )
        .unwrap();
        assert!(snap.machines.iter().any(|m| m.secure_boot == Some(false)));
        let all: Vec<&Flag> = snap.flags.iter().collect();
        for out in [
            digest_event(
                "https://fleet.example.com",
                DigestSchedule::Daily,
                &snap,
                None,
                datetime!(2026-11-12 08:00 UTC),
            ),
            critical_event(
                "https://fleet.example.com",
                &all,
                datetime!(2026-11-12 08:00 UTC),
            ),
        ] {
            let text = String::from_utf8(out.body).expect("UTF-8").to_lowercase();
            for word in [
                "secure",
                "tpm",
                "encrypt",
                "firmware_mode",
                "uefi",
                "elevat",
                "uptime",
                "boot",
            ] {
                assert!(!text.contains(word), "{word} in a {:?} payload", out.event);
            }
        }
    }

    /// The digest, from a real snapshot: its counts are the summary's, and its
    /// field names are pinned like the critical event's.
    #[test]
    fn the_digest_payload_counts_what_the_overview_shows() {
        let snap = fleet();
        let out = digest_event(
            "https://fleet.example.com",
            DigestSchedule::Daily,
            &snap,
            None,
            datetime!(2026-11-12 08:00 UTC),
        );
        assert_eq!(out.event, Event::Digest);
        let v: serde_json::Value = serde_json::from_slice(&out.body).expect("JSON");
        assert_eq!(
            keys(&v),
            [
                "event", "findings", "more", "sent_at", "server", "summary", "text"
            ]
        );
        assert_eq!(v["event"], "digest");
        let s = &v["summary"];
        assert_eq!(
            keys(s),
            [
                "changes",
                "live",
                "machines",
                "machines_flagged",
                "period",
                "quieted"
            ]
        );
        assert_eq!(s["period"], "daily");
        assert_eq!(s["machines"], snap.summary.machines);
        assert_eq!(s["machines_flagged"], snap.summary.machines_flagged);
        assert_eq!(s["live"]["warning"], snap.summary.warnings);
        assert_eq!(s["live"]["critical"], snap.summary.critical);
        assert_eq!(
            keys(&s["quieted"]),
            ["acknowledged", "snoozed", "suppressed"]
        );
        assert!(s["changes"].is_null(), "a daily digest has no comparison");
        assert_eq!(
            v["findings"].as_array().expect("a list").len(),
            snap.flags
                .iter()
                .filter(|f| f.severity == Severity::Critical)
                .count()
        );
        let text = v["text"].as_str().expect("text");
        assert!(
            text.starts_with(&format!(
                "loadbearer-fleet daily digest: {} critical, {} warning across {} of {} machines",
                snap.summary.critical,
                snap.summary.warnings,
                snap.summary.machines_flagged,
                snap.summary.machines
            )),
            "{text}"
        );
    }

    fn config_for(url: &str, secret: &str) -> Config {
        let mut config = Config::default();
        config.notify.webhook_url = Some(url.to_string());
        config.notify.secret = secret.to_string();
        config.validate().expect("a valid config");
        config
    }

    fn quick(config: &Config) -> Target {
        let mut target = Target::new(config).expect("built").expect("configured");
        target.backoff = Duration::from_millis(10);
        target
    }

    #[test]
    fn nothing_is_built_without_a_webhook() {
        let config = Config::default();
        assert!(Target::new(&config).expect("no error").is_none());
        assert!(
            Notifier::new(&config, Arc::default())
                .expect("no error")
                .is_none()
        );
    }

    /// The receiver can check the request is ours: the signature is the
    /// HMAC of exactly the bytes that arrived.
    #[tokio::test]
    async fn a_signed_request_verifies_against_its_body() {
        let rx = receiver::start(&[], Duration::ZERO).await;
        let target = quick(&config_for(&rx.url, "a-shared-secret"));
        let out = test_event("https://fleet.example.com", OffsetDateTime::now_utc());
        let status = target.deliver(&out).await.expect("delivered");
        assert_eq!(status, reqwest::StatusCode::NO_CONTENT);

        let got = rx.wait_for(1).await;
        let req = &got[0];
        assert_eq!(req.body, out.body, "the bytes raised are the bytes sent");
        assert_eq!(
            req.header(SIGNATURE_HEADER),
            Some(signature(b"a-shared-secret", &req.body))
        );
        assert_eq!(
            req.header("content-type").as_deref(),
            Some("application/json")
        );
        assert_eq!(
            req.header("user-agent"),
            Some(format!("loadbearer-fleet/{}", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(req.json()["event"], "test");
    }

    #[tokio::test]
    async fn without_a_secret_there_is_no_signature() {
        let rx = receiver::start(&[], Duration::ZERO).await;
        let target = quick(&config_for(&rx.url, ""));
        target
            .deliver(&test_event("https://f", OffsetDateTime::now_utc()))
            .await
            .expect("delivered");
        assert_eq!(rx.wait_for(1).await[0].header(SIGNATURE_HEADER), None);
    }

    #[tokio::test]
    async fn a_server_error_is_retried_and_then_succeeds() {
        let rx = receiver::start(&[500, 503], Duration::ZERO).await;
        let target = quick(&config_for(&rx.url, "s"));
        let status = target
            .deliver(&test_event("https://f", OffsetDateTime::now_utc()))
            .await
            .expect("the third try lands");
        assert_eq!(status, reqwest::StatusCode::NO_CONTENT);
        let got = rx.seen();
        assert_eq!(got.len(), 3);
        assert!(
            got.iter().all(|r| r.body == got[0].body),
            "every try is the same request"
        );
    }

    #[tokio::test]
    async fn retries_stop_at_three() {
        let rx = receiver::start(&[500, 429, 502, 200], Duration::ZERO).await;
        let target = quick(&config_for(&rx.url, ""));
        let failure = target
            .deliver(&test_event("https://f", OffsetDateTime::now_utc()))
            .await
            .expect_err("three tries, all refused");
        assert_eq!(failure.status(), Some(reqwest::StatusCode::BAD_GATEWAY));
        assert_eq!(rx.seen().len(), 3);
    }

    /// A 4xx other than 429 is the receiver saying no, and will say it again.
    #[tokio::test]
    async fn a_client_error_is_not_retried() {
        let rx = receiver::start(&[400], Duration::ZERO).await;
        let target = quick(&config_for(&rx.url, ""));
        let failure = target
            .deliver(&test_event("https://f", OffsetDateTime::now_utc()))
            .await
            .expect_err("refused");
        assert_eq!(failure.status(), Some(reqwest::StatusCode::BAD_REQUEST));
        assert_eq!(rx.seen().len(), 1, "no second try");
    }

    /// A redirect is not followed: the receiver configured is the only one.
    #[tokio::test]
    async fn a_redirect_is_a_failure_not_a_hop() {
        let rx = receiver::start(&[307], Duration::ZERO).await;
        let target = quick(&config_for(&rx.url, ""));
        let failure = target
            .deliver(&test_event("https://f", OffsetDateTime::now_utc()))
            .await
            .expect_err("not followed");
        assert_eq!(
            failure.status(),
            Some(reqwest::StatusCode::TEMPORARY_REDIRECT)
        );
        assert_eq!(rx.seen().len(), 1);
    }

    /// Nothing listening: three tries, then a failure that names neither the
    /// path nor the query, where a webhook URL keeps its token.
    #[tokio::test]
    async fn no_answer_is_retried_and_the_url_stays_out_of_the_error() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = free.local_addr().expect("address").port();
        drop(free);
        let target = quick(&config_for(
            &format!("http://127.0.0.1:{port}/hook?token=do-not-log-me"),
            "",
        ));
        assert_eq!(target.label(), format!("http://127.0.0.1:{port}"));
        let failure = target
            .deliver(&test_event("https://f", OffsetDateTime::now_utc()))
            .await
            .expect_err("nothing there");
        assert!(failure.status().is_none());
        let text = failure.to_string();
        assert!(!text.contains("do-not-log-me"), "{text}");
        assert!(!text.contains("/hook"), "{text}");
    }

    /// The queue is bounded, and what does not fit is counted rather than
    /// waited for.
    #[test]
    fn a_full_queue_drops_the_new_event_and_counts_it() {
        let counters = Arc::new(Counters::default());
        let notifier = Notifier::new(
            &config_for("https://hooks.example.com/x", ""),
            Arc::clone(&counters),
        )
        .expect("built")
        .expect("configured");
        // Never started, so nothing drains the queue.
        let snap = fleet();
        for _ in 0..QUEUE + 2 {
            notifier.raise_digest(
                DigestSchedule::Daily,
                &snap,
                None,
                OffsetDateTime::now_utc(),
            );
        }
        let rows = counters.rows();
        let count = |event: &str, result: &str| {
            rows.iter()
                .find(|(e, r, _)| *e == event && *r == result)
                .map(|(_, _, n)| *n)
                .expect("a row")
        };
        assert_eq!(count("digest", "dropped"), 2);
        assert_eq!(count("digest", "sent"), 0);
        assert_eq!(count("findings.critical", "dropped"), 0);
        assert_eq!(rows.len(), 6, "every combination, zeros included");
    }

    /// An acknowledged critical is not news; nor is a critical that already
    /// was one. Built on flags directly because the rule is about keys and
    /// severities, and the rebuild tests in `web` drive it with real
    /// documents.
    #[test]
    fn only_live_criticals_that_were_not_critical_are_new() {
        let mut before = fleet();
        let mut after = before.clone();
        let key = before.machines[0].key.clone();
        let mut old = flag(&key, "H", "disk_nearly_full");
        old.severity = Severity::Warning;
        before.flags.push(old);
        before.flags.push(flag(&key, "H", "drive_failing"));
        after.flags.push(flag(&key, "H", "disk_nearly_full"));
        after.flags.push(flag(&key, "H", "drive_failing"));
        let mut quiet = flag(&key, "H", "drive_worn");
        quiet.triage = Some(crate::state::Triage {
            state: crate::state::TriageState::Suppressed,
            by: "x".into(),
            at: "2026-11-12T00:00:00Z".into(),
            note: None,
            until: None,
            severity: None,
            rule: Some(1),
            reach: None,
            tags: None,
        });
        after.flags.push(quiet);
        let new = new_criticals(&before, &after).expect("a baseline");
        let codes: Vec<&str> = new.iter().map(|f| f.code).collect();
        assert_eq!(codes, ["disk_nearly_full"], "escalated only");

        before.machines.clear();
        assert!(
            new_criticals(&before, &after).is_none(),
            "an empty baseline"
        );
    }
}
