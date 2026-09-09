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
use crate::config::{AuthMode, Config};
use crate::index::Index;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

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
    let Some(root) = state.root.clone() else {
        return Err(AppError::bad_request(
            "this instance was started without a collection folder, so there is nothing to rescan",
        ));
    };
    let report = {
        let mut idx = state.index.lock().expect("index lock");
        idx.scan(&root)?
    };
    state.recompute()?;
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
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/api/me", get(auth::me))
        .route("/api/snapshot", get(get_snapshot))
        .route("/api/machine/{key}", get(get_machine))
        .route("/api/rescan", post(post_rescan))
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

/// Serve until interrupted.
pub async fn serve(state: Arc<AppState>, config: &Config, allow_remote: bool) -> Result<()> {
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

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
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
    use axum::http::{Method, Request};
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
        body: String,
    }

    impl Reply {
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).expect("a JSON body")
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
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        Reply {
            status,
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
        for uri in ["/api/health", "/app.css", "/app.js"] {
            assert_eq!(get(&state, uri, None).await.status, StatusCode::OK, "{uri}");
        }
        assert!(
            !get(&state, "/app.js", None).await.body.contains("SN-"),
            "and none of them says anything about the fleet"
        );
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
