//! The HTTP layer: a JSON API over the index, plus the dashboard that reads it.
//!
//! Four decisions shape this module.
//!
//! **The snapshot is computed once and projected per request.** Cohort
//! statistics, red flags and the executive summary come out of one pass over
//! the index, held in memory, and each request narrows that result rather than
//! re-querying. A fleet view is read constantly and written rarely — a sweep
//! lands, then a room full of people look at it — so recomputing per request
//! would be paying for the write pattern on every read. `POST /api/rescan` is
//! what invalidates it.
//!
//! **Authorization is part of that projection.** The caller's tag scope is
//! pushed into the same `Filter` the viewer's own filter goes through, so there
//! is exactly one place where "which machines does this request see" is
//! decided. See `auth`.
//!
//! **The assets are compiled in and nothing is fetched from the internet.**
//! No CDN, no npm, no build step: one binary, and it works on an air-gapped
//! management network where a script tag pointing at a public CDN would simply
//! fail. That also lets the response carry `default-src 'self'` honestly.
//!
//! **It refuses to put the estate on the wire in the clear.** A non-loopback
//! bind is declined unless either sign-in is configured and `public_url` is
//! https, or the operator has said `--allow-remote` in as many words.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::analytics::{self, Cohort, Detail, Filter, Flag, MachineView, Snapshot, Thresholds};
use crate::auth::{self, Authenticator, Caller};
use crate::config::{AuthMode, Config, Metrics as MetricsConfig};
use crate::index::{Index, ScanReport};
use crate::metrics::{self, Runtime, ScanStamp};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");
// An SVG rather than an .ico so it is text, and compiles in with everything
// else instead of needing `include_bytes!` and a binary in the repository.
const LOGO_SVG: &str = include_str!("../assets/logo-mark.svg");

pub struct AppState {
    index: Mutex<Index>,
    /// The fleet-wide analysis. Replaced wholesale on rescan; readers never
    /// block on a scan because they hold the previous `Arc` until it is.
    cache: RwLock<Arc<Snapshot>>,
    thresholds: Thresholds,
    /// The collection folder, if this instance was told about one. Absent means
    /// the index is read-only from here and `rescan` has nothing to walk.
    root: Option<PathBuf>,
    authenticator: Authenticator,
    metrics_config: MetricsConfig,
    /// Counters that outlive any one snapshot, for the metrics endpoint.
    runtime: Mutex<Runtime>,
}

impl AppState {
    pub fn new(index: Index, config: &Config, thresholds: Thresholds) -> Result<Arc<Self>> {
        let snap = analytics::snapshot(index.conn(), &thresholds, OffsetDateTime::now_utc())?;
        Ok(Arc::new(Self {
            index: Mutex::new(index),
            cache: RwLock::new(Arc::new(snap)),
            thresholds,
            root: config.server.collection_dir.clone(),
            authenticator: Authenticator::new(config),
            metrics_config: config.metrics.clone(),
            runtime: Mutex::new(Runtime::new()),
        }))
    }

    pub fn auth(&self) -> &Authenticator {
        &self.authenticator
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.cache.read().expect("cache lock"))
    }

    fn recompute(&self) -> Result<()> {
        let idx = self.index.lock().expect("index lock");
        let snap = analytics::snapshot(idx.conn(), &self.thresholds, OffsetDateTime::now_utc())?;
        *self.cache.write().expect("cache lock") = Arc::new(snap);
        Ok(())
    }

    /// Read the folder and rebuild the analysis. Blocking, and called from both
    /// the button and the timer so there is one implementation of "refresh".
    pub(crate) fn rescan(&self) -> Result<ScanReport> {
        let Some(root) = self.root.clone() else {
            anyhow::bail!(
                "this instance was started without a collection folder, so there is nothing \
                 to rescan"
            );
        };
        let mut runtime = self.runtime.lock().expect("runtime lock");
        runtime.scans_total += 1;
        drop(runtime);

        let report = {
            let mut idx = self.index.lock().expect("index lock");
            match idx.scan(&root) {
                Ok(report) => report,
                Err(e) => {
                    // A scan that could not run at all — an unreachable share,
                    // typically — as opposed to a file that would not parse,
                    // which `scan` reports and skips.
                    self.runtime
                        .lock()
                        .expect("runtime lock")
                        .scan_failures_total += 1;
                    return Err(e);
                }
            }
        };
        self.recompute()?;

        let mut runtime = self.runtime.lock().expect("runtime lock");
        runtime.last_scan = Some(ScanStamp {
            at: OffsetDateTime::now_utc(),
            seen: report.seen,
            ingested: report.ingested,
            rejected: report.rejected.len(),
        });
        Ok(report)
    }
}

/// Anything that goes wrong serving a request. The full chain goes to the log;
/// the response carries the summary, because the only readers are the people
/// running the estate and a blank 500 wastes their afternoon.
struct AppError {
    status: StatusCode,
    source: anyhow::Error,
}

impl AppError {
    /// A key that isn't in the fleet is the caller asking for something that
    /// doesn't exist, not the server failing — a stale bookmark shouldn't read
    /// as an outage. It is also the answer for a machine outside the caller's
    /// scope: whether that machine exists is not something they are entitled to
    /// learn.
    fn not_found(what: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            source: anyhow::anyhow!("{what}"),
        }
    }

    fn bad_request(what: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            source: anyhow::anyhow!("{what}"),
        }
    }

    fn forbidden(what: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            source: anyhow::anyhow!("{what}"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let message = format!("{:#}", self.source);
        if self.status.is_server_error() {
            tracing::error!(error = %message, "request failed");
        } else {
            tracing::debug!(status = %self.status, error = %message, "request refused");
        }
        (self.status, message).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            source: e.into(),
        }
    }
}

/// The filter, as it arrives on the query string.
///
/// `tags` is one parameter holding `key=value` pairs rather than a repeated
/// one, so the whole filter round-trips through a shareable URL without
/// needing a nested query-string parser.
#[derive(Debug, Deserialize, Default)]
struct FilterParams {
    /// `site=glasgow,ring=canary`
    tags: Option<String>,
    since_days: Option<f64>,
    cohort: Option<String>,
    q: Option<String>,
    flag: Option<String>,
}

impl FilterParams {
    /// Note what this does *not* set: `scopes`. Authorization is not something
    /// a caller gets to express, so it is attached from the session afterwards
    /// and never read from here.
    fn into_filter(self) -> Filter {
        let mut tags = std::collections::BTreeMap::new();
        for pair in self.tags.iter().flat_map(|s| s.split(',')) {
            if let Some((k, v)) = pair.split_once('=')
                && !k.trim().is_empty()
            {
                tags.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Filter {
            tags,
            max_age_days: self.since_days,
            cohort: self.cohort.filter(|s| !s.is_empty()),
            search: self.q.filter(|s| !s.trim().is_empty()),
            flag: self.flag.filter(|s| !s.is_empty()),
            scopes: Vec::new(),
        }
    }
}

/// The one place a request's visible fleet is decided.
fn visible(state: &AppState, caller: &Caller, params: FilterParams) -> Snapshot {
    let mut filter = params.into_filter();
    filter.scopes = caller.0.scopes.clone();
    state.snapshot().filtered(&filter, &state.thresholds)
}

async fn get_snapshot(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Query(params): Query<FilterParams>,
) -> Json<Snapshot> {
    Json(visible(&state, &caller, params))
}

/// Everything the drilldown needs for one machine, in one round trip.
#[derive(Serialize)]
struct MachinePayload {
    machine: MachineView,
    /// The machine's own flags, with the full explanation attached.
    flags: Vec<Flag>,
    /// Its peer group, if it has one. Statistics are fleet-wide.
    cohort: Option<Cohort>,
    #[serde(flatten)]
    detail: Detail,
}

async fn get_machine(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Path(key): Path<String>,
) -> Result<Json<MachinePayload>, AppError> {
    // Looked up in the caller's own view, so a machine outside their scope is
    // indistinguishable from one that was never measured.
    let snap = visible(&state, &caller, FilterParams::default());
    let Some(machine) = snap.machines.iter().find(|m| m.key == key) else {
        return Err(AppError::not_found(format!(
            "no machine keyed {key:?} in the index"
        )));
    };
    let detail = {
        let idx = state.index.lock().expect("index lock");
        analytics::detail(idx.conn(), &machine.key, machine.run_id)?
    };
    Ok(Json(MachinePayload {
        flags: snap
            .flags
            .iter()
            .filter(|f| f.machine_key == key)
            .cloned()
            .collect(),
        cohort: snap
            .cohorts
            .iter()
            .find(|c| c.id == machine.cohort)
            .cloned(),
        machine: machine.clone(),
        detail,
    }))
}

#[derive(Serialize)]
struct RescanResponse {
    seen: usize,
    ingested: usize,
    unchanged: usize,
    rejected: Vec<(String, String)>,
    machines: usize,
}

async fn post_rescan(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> Result<Json<RescanResponse>, AppError> {
    if !caller.0.may_rescan() {
        return Err(AppError::forbidden(
            "rescanning the collection folder needs the admin role",
        ));
    }
    if state.root.is_none() {
        return Err(AppError::bad_request(
            "this instance was started without a collection folder, so there is nothing to rescan",
        ));
    }
    // Walking a share and rewriting SQLite is blocking work; doing it on a
    // runtime thread would stall every other request for the duration.
    let scanning = Arc::clone(&state);
    let report = tokio::task::spawn_blocking(move || scanning.rescan())
        .await
        .map_err(|e| AppError::from(anyhow::anyhow!("the scan task failed: {e}")))??;
    let now_visible = visible(&state, &caller, FilterParams::default());
    Ok(Json(RescanResponse {
        seen: report.seen,
        ingested: report.ingested,
        unchanged: report.unchanged,
        rejected: report.rejected,
        machines: now_visible.summary.machines,
    }))
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // The assets are compiled into the binary, so an upgrade replaces
            // them without any way to bust a cached copy. Revalidating costs
            // one 304 on a local network and removes the whole class of "I
            // upgraded and the dashboard is still the old one".
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// The shell. Behind the session check, so an unauthenticated browser is sent
/// to sign in rather than handed a page whose first request can only fail.
async fn index_html(_caller: Caller) -> Response {
    asset(INDEX_HTML, "text/html; charset=utf-8")
}

/// Compare a presented credential without leaking, through timing, how much of
/// it was right.
///
/// Digesting both sides first means the comparison runs over 32 bytes of hash
/// whatever the inputs were, so neither the length nor any prefix of the real
/// token is recoverable from how long the answer took.
fn token_matches(presented: &str, configured: &str) -> bool {
    use sha2::{Digest, Sha256};
    Sha256::digest(presented.as_bytes()) == Sha256::digest(configured.as_bytes())
}

async fn get_metrics(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    if !state.metrics_config.enabled {
        // Not 403: a disabled endpoint should not advertise that it exists and
        // could be opened.
        return Err(AppError::not_found(
            "metrics are not enabled; set [metrics] enabled = true",
        ));
    }
    if !state.metrics_config.token.is_empty() {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !token_matches(presented, &state.metrics_config.token) {
            return Err(AppError {
                status: StatusCode::UNAUTHORIZED,
                source: anyhow::anyhow!("metrics need the configured bearer token"),
            });
        }
    }
    // Fleet-wide and unscoped: this is an operator's view of the service, and a
    // scrape must not depend on whose session happened to trigger it.
    let snapshot = state.snapshot();
    let runtime = state.runtime.lock().expect("runtime lock").clone();
    let body = metrics::render(&snapshot, &runtime, OffsetDateTime::now_utc());
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

pub fn router(state: Arc<AppState>) -> Router {
    // Nothing is loaded from anywhere but this origin, which is a property of
    // the build (no CDN, no npm) rather than a promise — so the header is
    // simply true, and it stays true on an air-gapped network.
    let security = [
        (
            "content-security-policy",
            "default-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; \
             frame-ancestors 'none'; form-action 'self'",
        ),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cross-origin-opener-policy", "same-origin"),
    ];
    let mut app = Router::new()
        .route("/", get(index_html))
        // The stylesheet and script stay reachable without a session: the
        // sign-in failure page is styled by the same stylesheet, and neither
        // file says anything about the fleet.
        .route(
            "/app.css",
            get(|| async { asset(APP_CSS, "text/css; charset=utf-8") }),
        )
        .route(
            "/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        // The favicon is fetched for every page including the sign-in failure
        // page, so like the stylesheet it stays outside the session check.
        .route(
            "/logo-mark.svg",
            get(|| async { asset(LOGO_SVG, "image/svg+xml") }),
        )
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        // Outside the session check by necessity: this is where signing out
        // lands, and needing a session there would bounce the user through the
        // provider and straight back in.
        .route("/auth/signed-out", get(auth::signed_out))
        .route("/api/me", get(auth::me))
        .route("/api/snapshot", get(get_snapshot))
        .route("/api/machine/{key}", get(get_machine))
        .route("/api/rescan", post(post_rescan))
        .route("/metrics", get(get_metrics))
        // Unauthenticated on purpose: a service manager or load balancer has to
        // be able to ask whether the process is alive, and the answer says
        // nothing about the estate.
        .route("/api/health", get(|| async { "ok" }));

    for (name, value) in security {
        app = app.layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        ));
    }

    app.layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Scan once at startup, and keep going if the folder cannot be read.
///
/// Deliberately not fatal. The index already holds the last sweep, and
/// refusing to show it because a share is briefly unreachable is worse than
/// showing it: the staleness flags say how old it is, the metrics say no scan
/// has succeeded, and the timer will pick the share up when it comes back. A
/// service that instead refused to start would restart-loop until somebody
/// noticed, showing nobody anything in the meantime.
pub fn report_startup_scan(state: &Arc<AppState>) {
    match state.rescan() {
        Ok(report) => {
            tracing::info!(
                seen = report.seen,
                ingested = report.ingested,
                unchanged = report.unchanged,
                rejected = report.rejected.len(),
                "startup scan"
            );
            for (path, why) in &report.rejected {
                tracing::warn!(%path, %why, "skipped");
            }
        }
        Err(e) => {
            tracing::error!(
                error = format!("{e:#}"),
                "the startup scan failed; serving whatever the index already holds"
            );
            eprintln!(
                "warning: the collection folder could not be read ({e:#}).\n\
                 Serving the existing index; the next scheduled scan will try again."
            );
        }
    }
}

/// Everything that should end the process, in one future.
///
/// A service manager stops a process by signal, not by keyboard: systemd sends
/// SIGTERM and waits, and Windows' service controller sends a stop request that
/// `service::run` turns into `stop`. Handling only Ctrl+C would mean being
/// killed after a timeout on every restart, with the index left mid-write.
async fn shutdown_signal(stop: Option<tokio::sync::watch::Receiver<bool>>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl+C"
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
                "SIGTERM"
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending().await
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<&str>();

    let requested = async {
        match stop {
            Some(mut rx) => {
                // Already set is still a stop: don't wait for a second change.
                while !*rx.borrow_and_update() {
                    if rx.changed().await.is_err() {
                        return "the stop channel closed";
                    }
                }
                "a service stop request"
            }
            None => std::future::pending().await,
        }
    };

    let reason = tokio::select! {
        r = ctrl_c => r,
        r = terminate => r,
        r = requested => r,
    };
    tracing::info!(reason, "shutting down");
}

/// Rescan on a timer, so the dashboard is current when nobody is looking at it.
fn spawn_periodic_scan(state: Arc<AppState>, every: Duration) {
    tokio::spawn(async move {
        // The startup scan has already run, so wait a full interval first.
        let mut ticker = tokio::time::interval(every);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let scanning = Arc::clone(&state);
            match tokio::task::spawn_blocking(move || scanning.rescan()).await {
                Ok(Ok(report)) => tracing::info!(
                    seen = report.seen,
                    ingested = report.ingested,
                    unchanged = report.unchanged,
                    rejected = report.rejected.len(),
                    "scheduled scan"
                ),
                // Neither of these should end the timer: a share that is
                // unreachable this minute is usually reachable the next, and a
                // dashboard that stops refreshing after one network blip is
                // worse than one that logs and carries on.
                Ok(Err(e)) => tracing::error!(error = format!("{e:#}"), "scheduled scan failed"),
                Err(e) => tracing::error!(error = %e, "the scan task panicked"),
            }
        }
    });
}

/// Serve until interrupted.
pub async fn serve(
    state: Arc<AppState>,
    config: &Config,
    allow_remote: bool,
    stop: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<()> {
    let bind: SocketAddr = config.server.bind;
    let loopback = match bind.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    let signed_in = config.auth.mode == AuthMode::Oidc;
    let encrypted = config.public_url_is_https();

    if !loopback && !allow_remote {
        // Two different mistakes, so two different explanations.
        if !signed_in {
            anyhow::bail!(
                "refusing to listen on {bind} with auth.mode = \"none\": anyone who could reach \
                 the port would get the hostname and firmware serial of every machine in the \
                 estate. Configure [auth] for single sign-on, bind to 127.0.0.1 and tunnel to \
                 it, or pass --allow-remote if this is a network you have already decided to \
                 trust."
            );
        }
        if !encrypted {
            anyhow::bail!(
                "refusing to listen on {bind} with server.public_url = {:?}: the session cookie \
                 would cross the network in the clear, and anyone who copied it would be signed \
                 in as that user. Put a TLS-terminating proxy in front and set public_url to its \
                 https address, or pass --allow-remote.",
                config.server.public_url
            );
        }
    }
    if !loopback && allow_remote && !(signed_in && encrypted) {
        tracing::warn!(
            %bind,
            sign_in = signed_in,
            https = encrypted,
            "serving the fleet on a network interface without both sign-in and TLS, as instructed"
        );
    }

    // Fleet-wide counts are not hostnames, but "how many machines this
    // organisation has and how many are failing" is still not for anyone who
    // can reach the port.
    if config.metrics.enabled && config.metrics.token.is_empty() && !loopback && !allow_remote {
        anyhow::bail!(
            "refusing to listen on {bind} with [metrics] enabled and no token: set \
             metrics.token, or bind to 127.0.0.1, or pass --allow-remote"
        );
    }

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    let local = listener.local_addr()?;
    tracing::info!(%local, sign_in = signed_in, "dashboard listening");
    println!(
        "loadbearer-fleet dashboard on http://{local}{}",
        if signed_in {
            ""
        } else {
            " (no sign-in configured: everyone who can reach this port is an administrator)"
        }
    );

    if config.server.scan_interval_minutes > 0 && state.root.is_some() {
        let every = Duration::from_secs(config.server.scan_interval_minutes * 60);
        tracing::info!(
            minutes = config.server.scan_interval_minutes,
            "scanning on a timer"
        );
        spawn_periodic_scan(Arc::clone(&state), every);
    } else if state.root.is_some() {
        tracing::info!("scan_interval_minutes is 0: the folder is only read on request");
    }

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal(stop))
        .await?;
    Ok(())
}

/// Authorization, driven through the real router.
///
/// These go through the actual request path — cookie parsing, the extractor,
/// the scope injection, the handler — because that is the boundary being
/// tested. A unit test of `Filter` proves the projection is right; only this
/// proves the projection is the one a request actually gets.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::body::Body;
    use axum::http::{HeaderMap, Method, Request};
    use tower::ServiceExt;

    use super::*;
    use crate::auth::Principal;
    use crate::config::{Auth, Grant, Role};

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn oidc_config() -> Config {
        Config {
            auth: Auth {
                mode: AuthMode::Oidc,
                issuer: "https://issuer.example/v2.0".into(),
                client_id: "client".into(),
                grants: vec![Grant {
                    group: "g".into(),
                    role: Role::Admin,
                    tags: BTreeMap::new(),
                }],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn state_for(config: &Config) -> Arc<AppState> {
        let mut index = Index::open_in_memory().expect("index");
        for f in [
            "win-modern.json",
            "linux-throttled.json",
            "low-end.json",
            "legacy-1.2.4.json",
        ] {
            index.ingest_file(&fixtures().join(f)).expect("fixture");
        }
        let mut config = config.clone();
        config.server.collection_dir = Some(fixtures());
        AppState::new(index, &config, Thresholds::default()).expect("state")
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    impl Reply {
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).expect("a JSON body")
        }

        fn header(&self, name: HeaderName) -> Option<String> {
            Some(self.headers.get(name)?.to_str().expect("ASCII").to_string())
        }
    }

    async fn request(
        state: &Arc<AppState>,
        method: Method,
        uri: &str,
        cookie: Option<&str>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, format!("lbf_session={c}"));
        }
        let response = router(Arc::clone(state))
            .oneshot(builder.body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).to_string(),
        }
    }

    async fn get(state: &Arc<AppState>, uri: &str, cookie: Option<&str>) -> Reply {
        request(state, Method::GET, uri, cookie).await
    }

    fn session(state: &Arc<AppState>, role: Role, scopes: &[&[(&str, &str)]]) -> String {
        state.auth().start_session(Principal::new(
            "test-subject",
            role,
            scopes
                .iter()
                .map(|set| {
                    set.iter()
                        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                        .collect()
                })
                .collect(),
        ))
    }

    #[tokio::test]
    async fn without_sign_in_configured_the_caller_is_a_local_administrator() {
        let state = state_for(&Config::default());
        let me = get(&state, "/api/me", None).await;
        assert_eq!(me.status, StatusCode::OK);
        let me = me.json();
        assert_eq!(me["role"], "admin");
        assert_eq!(me["may_rescan"], true);
        assert_eq!(
            me["authenticated"], false,
            "nobody signed in, and the UI must be able to say so"
        );
        assert_eq!(me["sign_in_enabled"], false);

        assert_eq!(
            get(&state, "/api/snapshot", None).await.status,
            StatusCode::OK
        );
        assert_eq!(
            request(&state, Method::POST, "/api/rescan", None)
                .await
                .status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn with_sign_in_configured_an_anonymous_caller_gets_nothing() {
        let state = state_for(&oidc_config());

        let api = get(&state, "/api/snapshot", None).await;
        assert_eq!(api.status, StatusCode::UNAUTHORIZED);
        assert!(
            api.body.contains("/auth/login"),
            "a fetch needs to be told where to go: {}",
            api.body
        );

        // A page, on the other hand, is redirected rather than handed JSON.
        let page = get(&state, "/", None).await;
        assert_eq!(page.status, StatusCode::SEE_OTHER);
        let location = page.body.clone();
        assert!(location.is_empty(), "a redirect has no body");

        assert_eq!(
            request(&state, Method::POST, "/api/rescan", None)
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(&state, "/api/machine/SN-WIN-0001", None).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_forged_session_cookie_is_not_a_session() {
        let state = state_for(&oidc_config());
        let forged = "f".repeat(64);
        assert_eq!(
            get(&state, "/api/snapshot", Some(&forged)).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_viewer_may_read_the_fleet_but_not_rescan_it() {
        let state = state_for(&oidc_config());
        let cookie = session(&state, Role::Viewer, &[]);

        let snap = get(&state, "/api/snapshot", Some(&cookie)).await;
        assert_eq!(snap.status, StatusCode::OK);
        assert_eq!(snap.json()["summary"]["machines"], 4);

        let rescan = request(&state, Method::POST, "/api/rescan", Some(&cookie)).await;
        assert_eq!(
            rescan.status,
            StatusCode::FORBIDDEN,
            "the button is hidden, but hiding it is not the control"
        );
        assert_eq!(
            get(&state, "/api/me", Some(&cookie)).await.json()["may_rescan"],
            false
        );
    }

    /// Signing out used to be a no-op, and this is the shape of why.
    ///
    /// The redirect went to `/`, which needs a session, so the browser was
    /// sent on to `/auth/login`; the provider still had a session of its own
    /// and handed back a fresh one immediately. The user saw a button that did
    /// nothing. So: the landing page must need no session, and must be
    /// reachable without one.
    #[tokio::test]
    async fn signing_out_does_not_land_where_it_will_be_signed_back_in() {
        let state = state_for(&oidc_config());
        let cookie = session(&state, Role::Admin, &[]);

        let out = request(&state, Method::POST, "/auth/logout", Some(&cookie)).await;
        let landing = out.header(header::LOCATION).expect("a redirect");
        assert_eq!(landing, "/auth/signed-out");
        assert!(
            out.header(header::SET_COOKIE)
                .expect("the cookie is cleared")
                .contains("Max-Age=0")
        );

        // Server-side too, not just the cookie: a copy of it is now worthless.
        assert_eq!(
            get(&state, "/api/snapshot", Some(&cookie)).await.status,
            StatusCode::UNAUTHORIZED
        );

        let page = get(&state, &landing, None).await;
        assert_eq!(
            page.status,
            StatusCode::OK,
            "the landing page must not itself need a session"
        );
        assert!(page.body.contains("Signed out"));
        assert!(
            page.body.contains("issuer.example"),
            "it should name the provider whose session is still open: {}",
            page.body
        );

        // The trap it avoids: `/` does need one, and would start a sign-in.
        let root = get(&state, "/", None).await;
        assert!(
            root.header(header::LOCATION)
                .is_some_and(|l| l.starts_with("/auth/login")),
            "if this ever stops redirecting, the reason for the landing page is gone"
        );
    }

    #[tokio::test]
    async fn an_admin_may_rescan() {
        let state = state_for(&oidc_config());
        let cookie = session(&state, Role::Admin, &[]);
        let rescan = request(&state, Method::POST, "/api/rescan", Some(&cookie)).await;
        assert_eq!(rescan.status, StatusCode::OK);
        assert_eq!(
            rescan.json()["unchanged"],
            4,
            "the same files, already indexed"
        );
    }

    /// The one that matters. A viewer scoped to one site must not be able to
    /// reach another by editing the query string.
    #[tokio::test]
    async fn a_scoped_viewer_cannot_reach_outside_their_scope() {
        let state = state_for(&oidc_config());
        let cookie = session(&state, Role::Viewer, &[&[("site", "glasgow")]]);

        let snap = get(&state, "/api/snapshot", Some(&cookie)).await.json();
        assert_eq!(
            snap["summary"]["machines"], 1,
            "one machine is tagged glasgow"
        );
        assert_eq!(snap["machines"][0]["tags"]["site"], "glasgow");

        // Asking for the other site returns an empty fleet, not that site.
        let elsewhere = get(&state, "/api/snapshot?tags=site%3Dedinburgh", Some(&cookie))
            .await
            .json();
        assert_eq!(elsewhere["summary"]["machines"], 0);
        assert_eq!(elsewhere["flags"].as_array().expect("array").len(), 0);

        // And the cohort statistics are still the whole fleet's, so the
        // comparison a scoped viewer sees means what it says.
        let unscoped = get(
            &state,
            "/api/snapshot",
            Some(&session(&state, Role::Admin, &[])),
        )
        .await
        .json();
        assert_eq!(
            snap["cohorts"].as_array().expect("array").len(),
            unscoped["cohorts"].as_array().expect("array").len()
        );
    }

    /// Whether a machine outside your scope exists is not something you are
    /// entitled to learn, so it is a 404 rather than a 403.
    #[tokio::test]
    async fn a_machine_outside_the_scope_is_indistinguishable_from_one_that_does_not_exist() {
        let state = state_for(&oidc_config());
        let scoped = session(&state, Role::Viewer, &[&[("site", "glasgow")]]);
        let admin = session(&state, Role::Admin, &[]);

        // Named from the Edinburgh fixture, which the admin can see.
        let all = get(&state, "/api/snapshot", Some(&admin)).await.json();
        let elsewhere = all["machines"]
            .as_array()
            .expect("array")
            .iter()
            .find(|m| m["tags"]["site"] == "edinburgh")
            .expect("an Edinburgh machine")["key"]
            .as_str()
            .expect("key")
            .to_string();

        assert_eq!(
            get(&state, &format!("/api/machine/{elsewhere}"), Some(&admin))
                .await
                .status,
            StatusCode::OK
        );
        let refused = get(&state, &format!("/api/machine/{elsewhere}"), Some(&scoped)).await;
        assert_eq!(refused.status, StatusCode::NOT_FOUND);
        let invented = get(&state, "/api/machine/no-such-machine", Some(&scoped)).await;
        assert_eq!(
            refused.status, invented.status,
            "the two must be indistinguishable"
        );
    }

    #[tokio::test]
    async fn health_and_the_assets_stay_reachable_without_a_session() {
        let state = state_for(&oidc_config());
        // A service manager has to be able to ask whether the process is alive,
        // and the sign-in failure page is styled by the same stylesheet it
        // would otherwise be unable to load.
        for uri in ["/api/health", "/app.css", "/app.js", "/logo-mark.svg"] {
            assert_eq!(get(&state, uri, None).await.status, StatusCode::OK, "{uri}");
        }
        assert!(
            !get(&state, "/app.js", None).await.body.contains("SN-"),
            "and none of them says anything about the fleet"
        );
    }

    fn metrics_config(enabled: bool, token: &str) -> Config {
        Config {
            metrics: MetricsConfig {
                enabled,
                token: token.to_string(),
            },
            ..Config::default()
        }
    }

    async fn get_with_bearer(state: &Arc<AppState>, uri: &str, token: &str) -> StatusCode {
        router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
            .status()
    }

    #[tokio::test]
    async fn metrics_are_absent_until_switched_on() {
        let state = state_for(&metrics_config(false, ""));
        assert_eq!(
            get(&state, "/metrics", None).await.status,
            StatusCode::NOT_FOUND,
            "a disabled endpoint should not advertise that it exists"
        );
    }

    #[tokio::test]
    async fn metrics_report_the_fleet_when_switched_on() {
        let state = state_for(&metrics_config(true, ""));
        let reply = get(&state, "/metrics", None).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(
            reply.body.contains("loadbearer_fleet_machines 4"),
            "{}",
            reply.body
        );
        assert!(
            reply
                .body
                .contains("# TYPE loadbearer_fleet_machines gauge")
        );
        // Nothing per-machine: a metrics store is usually less protected than
        // this service is.
        assert!(!reply.body.contains("SN-"), "a serial reached the metrics");
    }

    /// A scrape has no session and cannot get one, so the token is the whole
    /// control.
    #[tokio::test]
    async fn a_metrics_token_is_required_when_one_is_configured() {
        let state = state_for(&metrics_config(true, "s3cret-token"));
        assert_eq!(
            get(&state, "/metrics", None).await.status,
            StatusCode::UNAUTHORIZED,
            "no header at all"
        );
        assert_eq!(
            get_with_bearer(&state, "/metrics", "not-the-token").await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get_with_bearer(&state, "/metrics", "s3cret-token").await,
            StatusCode::OK
        );
    }

    #[test]
    fn a_token_comparison_does_not_depend_on_how_much_was_right() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("", "abc"));
        assert!(!token_matches("abc", ""));
        // A prefix must not pass, which is what a sloppy length-first
        // comparison can get wrong.
        assert!(!token_matches("ab", "abc"));
        assert!(!token_matches("abcd", "abc"));
    }

    /// The metric an operator pages on has to appear once a scan has happened,
    /// and stay absent before that: a zero timestamp would read as 1970 and
    /// fire an alert for the wrong reason.
    #[tokio::test]
    async fn a_scan_records_what_the_metrics_report() {
        let state = state_for(&metrics_config(true, ""));
        let before = get(&state, "/metrics", None).await.body;
        assert!(!before.contains("scan_last_success_timestamp_seconds"));
        assert!(before.contains("loadbearer_fleet_scans_total 0"));

        assert_eq!(
            request(&state, Method::POST, "/api/rescan", None)
                .await
                .status,
            StatusCode::OK
        );

        let after = get(&state, "/metrics", None).await.body;
        assert!(after.contains("loadbearer_fleet_scans_total 1"), "{after}");
        assert!(
            after.contains("scan_last_success_timestamp_seconds"),
            "{after}"
        );
        assert!(
            after.contains("loadbearer_fleet_scan_files_seen 4"),
            "{after}"
        );
        assert!(
            after.contains("loadbearer_fleet_scan_failures_total 0"),
            "{after}"
        );
    }

    /// A share that cannot be read must show up as a failure rather than as an
    /// estate that suddenly has nothing wrong with it.
    #[tokio::test]
    async fn an_unreachable_folder_counts_as_a_failure_and_changes_nothing() {
        let mut config = metrics_config(true, "");
        config.server.collection_dir = Some(PathBuf::from("no-such-folder-anywhere"));
        let mut index = Index::open_in_memory().expect("index");
        index
            .ingest_file(&fixtures().join("win-modern.json"))
            .expect("fixture");
        let state = AppState::new(index, &config, Thresholds::default()).expect("state");

        assert_eq!(
            request(&state, Method::POST, "/api/rescan", None)
                .await
                .status,
            StatusCode::INTERNAL_SERVER_ERROR
        );

        let metrics = get(&state, "/metrics", None).await.body;
        assert!(
            metrics.contains("loadbearer_fleet_scan_failures_total 1"),
            "{metrics}"
        );
        assert!(
            !metrics.contains("scan_last_success_timestamp_seconds"),
            "a failed scan must not look like a successful one"
        );
        assert!(
            metrics.contains("loadbearer_fleet_machines 1"),
            "and the fleet it already knew about is untouched"
        );
    }

    /// The snapshot is the largest thing this serves and it is mostly repeated
    /// JSON keys, so it compresses about five to one. That matters over a WAN
    /// link to a branch office, and it is the sort of thing that would quietly
    /// stop working on a dependency bump without anyone noticing — which is
    /// exactly what happened to prompt this test.
    #[tokio::test]
    async fn the_snapshot_is_compressed_when_the_client_asks() {
        let state = state_for(&Config::default());

        let compressed = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/snapshot")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(
            compressed
                .headers()
                .get(header::CONTENT_ENCODING)
                .map(|v| v.to_str().expect("ascii")),
            Some("gzip")
        );
        let squeezed = axum::body::to_bytes(compressed.into_body(), 1 << 22)
            .await
            .expect("body")
            .len();

        let plain = get(&state, "/api/snapshot", None).await.body.len();
        assert!(
            squeezed * 2 < plain,
            "{squeezed} compressed against {plain} plain is not worth the CPU"
        );
    }

    /// The page names the favicon, so the two have to agree: a link tag
    /// pointing at a 404 is a broken icon in every tab and an error nowhere.
    #[tokio::test]
    async fn the_favicon_the_page_asks_for_is_the_one_served() {
        let state = state_for(&Config::default());

        let page = get(&state, "/", None).await.body;
        let href = page
            .lines()
            .find(|l| l.contains("rel=\"icon\""))
            .and_then(|l| l.split("href=\"").nth(1))
            .and_then(|l| l.split('"').next())
            .expect("the page should declare an icon")
            .to_string();
        assert_eq!(href, "/logo-mark.svg");

        let icon = get(&state, &href, None).await;
        assert_eq!(icon.status, StatusCode::OK);
        assert!(icon.body.contains("<svg"), "not an SVG: {}", icon.body);

        // The topbar uses the same file, so there is one mark rather than two
        // that can drift apart.
        assert!(page.contains("src=\"/logo-mark.svg\""));
    }

    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        let state = state_for(&Config::default());
        let response = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let headers = response.headers();
        let csp = headers
            .get("content-security-policy")
            .expect("a CSP")
            .to_str()
            .expect("ascii");
        assert!(csp.contains("default-src 'self'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
    }
}
