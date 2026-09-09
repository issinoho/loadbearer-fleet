//! The HTTP layer: a JSON API over the index, plus the dashboard that reads it.
//!
//! Three decisions shape this module.
//!
//! **The snapshot is computed once and projected per request.** Cohort
//! statistics, red flags and the executive summary come out of one pass over
//! the index, held in memory, and each request narrows that result rather than
//! re-querying. A fleet view is read constantly and written rarely — a sweep
//! lands, then a room full of people look at it — so recomputing per request
//! would be paying for the write pattern on every read. `POST /api/rescan` is
//! what invalidates it.
//!
//! **The assets are compiled in and nothing is fetched from the internet.**
//! No CDN, no npm, no build step: one binary, and it works on an air-gapped
//! management network where a script tag pointing at a public CDN would simply
//! fail. That also lets the response carry `default-src 'self'` honestly.
//!
//! **It refuses to listen beyond loopback until it can authenticate.** There
//! is no sign-in yet, and the index holds hostnames and firmware serials for
//! the whole estate. Binding that to a network interface is a decision someone
//! has to make deliberately, so it takes an explicit flag and says why.

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
}

impl AppState {
    pub fn new(index: Index, root: Option<PathBuf>, thresholds: Thresholds) -> Result<Arc<Self>> {
        let snap = analytics::snapshot(index.conn(), &thresholds, OffsetDateTime::now_utc())?;
        Ok(Arc::new(Self {
            index: Mutex::new(index),
            cache: RwLock::new(Arc::new(snap)),
            thresholds,
            root,
        }))
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
    /// as an outage.
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
        }
    }
}

async fn get_snapshot(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FilterParams>,
) -> Json<Snapshot> {
    let snap = state.snapshot();
    Json(snap.filtered(&params.into_filter(), &state.thresholds))
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
    Path(key): Path<String>,
) -> Result<Json<MachinePayload>, AppError> {
    let snap = state.snapshot();
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

async fn post_rescan(State(state): State<Arc<AppState>>) -> Result<Json<RescanResponse>, AppError> {
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
    Ok(Json(RescanResponse {
        seen: report.seen,
        ingested: report.ingested,
        unchanged: report.unchanged,
        rejected: report.rejected,
        machines: state.snapshot().summary.machines,
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

pub fn router(state: Arc<AppState>) -> Router {
    // Nothing is loaded from anywhere but this origin, which is a property of
    // the build (no CDN, no npm) rather than a promise — so the header is
    // simply true, and it stays true on an air-gapped network.
    let security = [
        (
            "content-security-policy",
            "default-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; \
          frame-ancestors 'none'",
        ),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cross-origin-opener-policy", "same-origin"),
    ];
    let mut app = Router::new()
        .route(
            "/",
            get(|| async { asset(INDEX_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/app.css",
            get(|| async { asset(APP_CSS, "text/css; charset=utf-8") }),
        )
        .route(
            "/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route("/api/snapshot", get(get_snapshot))
        .route("/api/machine/{key}", get(get_machine))
        .route("/api/rescan", post(post_rescan))
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
///
/// Refuses a non-loopback bind without `allow_remote`: there is no
/// authentication yet, and the index holds the hostname and firmware serial of
/// every machine in the estate.
pub async fn serve(state: Arc<AppState>, bind: SocketAddr, allow_remote: bool) -> Result<()> {
    let loopback = match bind.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    if !loopback && !allow_remote {
        anyhow::bail!(
            "refusing to listen on {bind}: there is no authentication yet, and the index holds \
             the hostname and firmware serial of every machine in the estate. Bind to 127.0.0.1 \
             and tunnel to it, or pass --allow-remote if this network is one you have already \
             decided to trust."
        );
    }
    if allow_remote && !loopback {
        tracing::warn!(
            %bind,
            "serving an unauthenticated dashboard on a network interface, as instructed"
        );
    }

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    let local = listener.local_addr()?;
    tracing::info!(%local, "dashboard listening");
    println!("loadbearer-fleet dashboard on http://{local}");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
