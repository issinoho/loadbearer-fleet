//! Sign-in and authorization.
//!
//! OpenID Connect authorization code flow with PKCE, roles from group claims,
//! and tag-scoped access to the estate.
//!
//! ## What is enforced where
//!
//! Authorization is applied **in the data layer, not the UI**. A viewer's tag
//! scope becomes part of the `Filter` the snapshot is projected through, and
//! that scope is set from the session — never from the query string — so a
//! caller cannot widen it by editing a URL. The dashboard hides the Rescan
//! button from a viewer as a courtesy; the server refuses the request whether
//! it was hidden or not.
//!
//! ## Sessions
//!
//! Opaque random tokens in an `HttpOnly` cookie, with the session held server
//! side. The map is keyed by the **SHA-256 of the token** rather than the token
//! itself, so a memory dump or an accidentally logged key does not hand anyone
//! a live session. There is no refresh: when the session expires the user signs
//! in again, which is the right trade for a dashboard nobody sits in all day
//! and it means no refresh tokens are stored anywhere.
//!
//! Sessions live in memory, so a restart signs everyone out. For a tool that is
//! read constantly and restarted rarely that is a fair price for having no
//! session store to secure, and re-authentication against an already-signed-in
//! identity provider is a redirect the user barely sees.
//!
//! ## Why discovery happens per sign-in
//!
//! The provider metadata and its signing keys are fetched at each sign-in
//! rather than cached at startup. It costs one extra HTTP round trip on a rare
//! operation, and in exchange the service starts when the identity provider is
//! briefly unreachable, and a key rotation is picked up immediately instead of
//! at the next restart. `loadbearer-fleet check-auth` exists so an admin can
//! still fail fast on a bad tenant ID at deploy time.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use axum::extract::{FromRequestParts, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
};
// Through the OIDC crate's re-export, so the HTTP client can never be a
// different version from the one its traits are implemented for.
use openidconnect::reqwest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::config::{Auth as AuthConfig, AuthMode, Config, Entitlement, Role};
use crate::web::AppState;

const SESSION_COOKIE: &str = "lbf_session";
/// Holds the CSRF state for the few seconds the browser is at the identity
/// provider. Requiring it to match the state in the callback is what stops an
/// attacker completing a sign-in *into* someone else's browser.
const LOGIN_COOKIE: &str = "lbf_login";
/// How long a half-finished sign-in stays valid. Long enough for a password, a
/// second factor and a consent screen; short enough not to accumulate.
const PENDING_TTL_MINUTES: i64 = 15;
/// How many half-finished sign-ins to hold at once.
///
/// `/auth/login` is reachable without a session, and each call records a state,
/// a nonce and a PKCE verifier for fifteen minutes — so without a ceiling,
/// unauthenticated requests grow this map for as long as they keep coming. Far
/// more than a real estate needs concurrently in flight: a dashboard for a
/// thousand machines has a handful of people signing in at once, not a
/// thousand.
const MAX_PENDING: usize = 1024;

/// The client type after discovery has filled in the endpoints.
type OidcClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// Who is asking, and what they may see.
#[derive(Debug, Clone, Serialize)]
pub struct Principal {
    pub subject: String,
    pub name: String,
    pub email: Option<String>,
    pub role: Role,
    /// Tag sets, any one of which a machine may match. Empty means the whole
    /// fleet.
    pub scopes: Vec<BTreeMap<String, String>>,
    /// False for the stand-in principal used when sign-in is switched off, so
    /// the UI can say so rather than implying somebody signed in.
    pub authenticated: bool,
}

impl Principal {
    /// A signed-in caller. Built here rather than by hand so a test session
    /// and a real one are the same shape.
    pub(crate) fn new(subject: &str, role: Role, scopes: Vec<BTreeMap<String, String>>) -> Self {
        Self {
            subject: subject.to_string(),
            name: subject.to_string(),
            email: None,
            role,
            scopes,
            authenticated: true,
        }
    }

    /// The caller when `auth.mode = "none"`.
    ///
    /// Every request is served through the same authorization path whether
    /// sign-in is on or off — there is no "if auth disabled" branch anywhere
    /// downstream, because that is the branch that eventually gets the check
    /// wrong.
    fn local() -> Self {
        Self {
            subject: "local".into(),
            name: "Local access".into(),
            email: None,
            role: Role::Admin,
            scopes: Vec::new(),
            authenticated: false,
        }
    }

    pub fn may_rescan(&self) -> bool {
        self.role == Role::Admin
    }
}

struct Session {
    principal: Principal,
    expires: OffsetDateTime,
}

struct Pending {
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    /// Where the user was heading before being sent to sign in.
    next: String,
    expires: OffsetDateTime,
}

/// Sign-in state: the configuration, live sessions, and sign-ins in flight.
pub struct Authenticator {
    config: AuthConfig,
    /// `https`, so the session cookie can be marked `Secure`.
    secure_cookies: bool,
    redirect_url: String,
    sessions: Mutex<HashMap<String, Session>>,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Authenticator {
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.auth.clone(),
            secure_cookies: config.public_url_is_https(),
            redirect_url: config.redirect_url(),
            sessions: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.mode == AuthMode::Oidc
    }

    fn ttl(&self) -> Duration {
        Duration::hours(i64::from(self.config.session_hours))
    }

    /// Random, opaque, and never derived from anything about the user.
    fn new_token() -> String {
        rand::random::<[u8; 32]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn key(token: &str) -> String {
        Sha256::digest(token.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The only way a session comes into being. Crate-visible rather than
    /// private so the HTTP tests can mint one and drive the real router with
    /// it — testing the authorization boundary through anything less than the
    /// actual request path would be testing the wrong thing.
    pub(crate) fn start_session(&self, principal: Principal) -> String {
        let token = Self::new_token();
        let now = OffsetDateTime::now_utc();
        let mut sessions = self.sessions.lock().expect("sessions lock");
        // Sweeping on write keeps expired sessions from accumulating without a
        // background task to own.
        sessions.retain(|_, s| s.expires > now);
        sessions.insert(
            Self::key(&token),
            Session {
                principal,
                expires: now + self.ttl(),
            },
        );
        token
    }

    fn lookup(&self, token: &str) -> Option<Principal> {
        let sessions = self.sessions.lock().expect("sessions lock");
        let session = sessions.get(&Self::key(token))?;
        (session.expires > OffsetDateTime::now_utc()).then(|| session.principal.clone())
    }

    fn end_session(&self, token: &str) {
        self.sessions
            .lock()
            .expect("sessions lock")
            .remove(&Self::key(token));
    }

    /// `false` when the map is full, which the caller turns into a refusal.
    fn remember_pending(&self, state: &str, pending: Pending) -> bool {
        let now = OffsetDateTime::now_utc();
        let mut map = self.pending.lock().expect("pending lock");
        map.retain(|_, p| p.expires > now);
        // Sweeping first means the cap only bites when that many sign-ins are
        // genuinely in flight inside the TTL. Refusing the new one rather than
        // evicting an old one is deliberate: evicting would let a flood of
        // requests break the sign-in someone is halfway through, turning a
        // memory bound into a way to deny them access.
        if map.len() >= MAX_PENDING && !map.contains_key(state) {
            tracing::warn!(
                pending = map.len(),
                "refusing to start another sign-in: too many are already in flight. If this is \
                 not a burst of real users, something is calling /auth/login in a loop"
            );
            return false;
        }
        map.insert(state.to_string(), pending);
        true
    }

    /// Single use: a code may be redeemed once, so the record goes with it.
    fn take_pending(&self, state: &str) -> Option<Pending> {
        let p = self.pending.lock().expect("pending lock").remove(state)?;
        (p.expires > OffsetDateTime::now_utc()).then_some(p)
    }

    async fn discover(&self) -> Result<(reqwest::Client, OidcClient)> {
        let http = reqwest::ClientBuilder::new()
            // Following redirects from an identity provider's discovery URL
            // turns this into an SSRF primitive.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the HTTP client")?;
        let issuer = IssuerUrl::new(self.config.issuer.clone())
            .with_context(|| format!("auth.issuer {:?} is not a URL", self.config.issuer))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .with_context(|| {
                format!(
                    "fetching {}/.well-known/openid-configuration — check auth.issuer, and that \
                     this host can reach the identity provider",
                    self.config.issuer.trim_end_matches('/')
                )
            })?;
        let secret = (!self.config.client_secret.is_empty())
            .then(|| ClientSecret::new(self.config.client_secret.clone()));
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.config.client_id.clone()),
            secret,
        )
        .set_redirect_uri(
            RedirectUrl::new(self.redirect_url.clone())
                .with_context(|| format!("{:?} is not a URL", self.redirect_url))?,
        );
        Ok((http, client))
    }

    /// Confirm the settings without waiting for somebody to try signing in.
    pub async fn check(&self) -> Result<String> {
        let (_http, _client) = self.discover().await?;
        Ok(format!(
            "discovery succeeded for {}\n  client_id:    {}\n  client type:  {}\n  \
             redirect URI: {}\n  groups claim: {}\n  grants:       {}\n\nRegister that exact \
             redirect URI on the app registration, and make sure the {:?} claim is emitted in \
             the ID token — on Entra that is Token configuration, not a scope.",
            self.config.issuer,
            self.config.client_id,
            if self.config.client_secret.is_empty() {
                "public (PKCE, no secret)"
            } else {
                "confidential (client secret configured)"
            },
            self.redirect_url,
            self.config.groups_claim,
            self.config.grants.len(),
            self.config.groups_claim,
        ))
    }
}

/// Cookie attributes, in one place.
///
/// `SameSite=Lax` rather than `Strict`: the sign-in callback is a cross-site
/// navigation back from the identity provider, and `Strict` would withhold the
/// cookie that the callback has to check.
fn set_cookie(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    let mut c = format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    if secure {
        c.push_str("; Secure");
    }
    c
}

fn clear_cookie(name: &str, secure: bool) -> String {
    set_cookie(name, "", 0, secure)
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// Extractor for the caller. Anything behind it has been authorized.
pub struct Caller(pub Principal);

/// How a missing session is reported depends on who asked. A page gets sent to
/// sign in; an API call gets a 401 it can act on, because redirecting a `fetch`
/// to an HTML sign-in page produces a confusing parse error rather than a
/// useful one.
pub struct NotSignedIn {
    api: bool,
    next: String,
}

impl IntoResponse for NotSignedIn {
    fn into_response(self) -> Response {
        if self.api {
            (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"error":"not signed in","login":"/auth/login"}"#,
            )
                .into_response()
        } else {
            Redirect::to(&format!("/auth/login?next={}", urlencode(&self.next))).into_response()
        }
    }
}

impl FromRequestParts<Arc<AppState>> for Caller {
    type Rejection = NotSignedIn;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let auth = state.auth();
        if !auth.enabled() {
            return Ok(Caller(Principal::local()));
        }
        let principal = read_cookie(&parts.headers, SESSION_COOKIE).and_then(|t| auth.lookup(&t));
        match principal {
            Some(p) => Ok(Caller(p)),
            None => Err(NotSignedIn {
                api: parts.uri.path().starts_with("/api/"),
                next: parts
                    .uri
                    .path_and_query()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "/".into()),
            }),
        }
    }
}

/// Percent-encode enough to make a path safe in a query parameter.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Only ever send a user back to somewhere on this site. An open redirect in a
/// sign-in flow is how a phishing page borrows a real domain.
fn safe_next(next: Option<String>) -> String {
    let candidate = next.unwrap_or_default();
    let ok = candidate.starts_with('/')
        && !candidate.starts_with("//")
        && !candidate.contains('\\')
        && !candidate.contains("://")
        // Control characters cannot appear in a `Location` header, and a
        // browser strips tabs and newlines from a URL before acting on it — so
        // one here is either an attempt to smuggle something past the checks
        // above or a response that will not build. Neither is a redirect worth
        // attempting.
        && !candidate.chars().any(char::is_control);
    if ok { candidate } else { "/".to_string() }
}

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LoginQuery>,
) -> Result<Response, AuthError> {
    let auth = state.auth();
    if !auth.enabled() {
        return Ok(Redirect::to("/").into_response());
    }
    let (_http, client) = auth.discover().await?;

    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut request = client.authorize_url(
        CoreAuthenticationFlow::AuthorizationCode,
        CsrfToken::new_random,
        Nonce::new_random,
    );
    for scope in ["openid", "profile", "email"]
        .iter()
        .map(|s| (*s).to_string())
        .chain(auth.config.extra_scopes.iter().cloned())
    {
        request = request.add_scope(Scope::new(scope));
    }
    let (url, csrf, nonce) = request.set_pkce_challenge(challenge).url();

    let state_value = csrf.secret().clone();
    if !auth.remember_pending(
        &state_value,
        Pending {
            verifier,
            nonce,
            next: safe_next(q.next),
            expires: OffsetDateTime::now_utc() + Duration::minutes(PENDING_TTL_MINUTES),
        },
    ) {
        return Err(AuthError::busy(
            "too many sign-ins are already in progress; try again in a moment",
        ));
    }

    Ok((
        [(
            header::SET_COOKIE,
            HeaderValue::from_str(&set_cookie(
                LOGIN_COOKIE,
                &state_value,
                PENDING_TTL_MINUTES * 60,
                auth.secure_cookies,
            ))
            .map_err(|e| AuthError::internal(anyhow!(e)))?,
        )],
        Redirect::to(url.as_str()),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub async fn callback(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, AuthError> {
    let auth = app.auth();
    if !auth.enabled() {
        return Ok(Redirect::to("/").into_response());
    }
    if let Some(error) = q.error {
        // The provider declined. Its own description is far more use than
        // anything invented here.
        return Err(AuthError::denied(format!(
            "the identity provider refused the sign-in: {error}{}",
            q.error_description
                .map(|d| format!(" — {d}"))
                .unwrap_or_default()
        )));
    }
    let (code, returned_state) = match (q.code, q.state) {
        (Some(c), Some(s)) => (c, s),
        _ => return Err(AuthError::denied("callback had no authorization code")),
    };

    // Both halves must agree: the state must be one this server issued *and*
    // the browser completing the flow must be the one that started it.
    let cookie_state = read_cookie(&headers, LOGIN_COOKIE)
        .ok_or_else(|| AuthError::denied("no sign-in was in progress in this browser"))?;
    if cookie_state != returned_state {
        return Err(AuthError::denied(
            "the sign-in did not start in this browser, so it was not completed",
        ));
    }
    let pending = auth
        .take_pending(&returned_state)
        .ok_or_else(|| AuthError::denied("that sign-in has already been used or has expired"))?;

    let (http, client) = auth.discover().await?;
    let tokens = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| AuthError::internal(anyhow!("no token endpoint: {e}")))?
        .set_pkce_verifier(pending.verifier)
        .request_async(&http)
        .await
        .map_err(|e| AuthError::internal(anyhow!("exchanging the authorization code: {e}")))?;

    let id_token = tokens
        .id_token()
        .ok_or_else(|| AuthError::internal(anyhow!("the provider returned no ID token")))?;
    // Signature, issuer, audience, expiry and nonce, all checked here.
    let claims = id_token
        .claims(&client.id_token_verifier(), &pending.nonce)
        .map_err(|e| AuthError::denied(format!("the ID token did not verify: {e}")))?;

    let groups = groups_from(&id_token.to_string(), &auth.config.groups_claim)
        .map_err(AuthError::internal)?;
    let Entitlement { role, scopes } = auth.config.resolve(&groups).ok_or_else(|| {
        AuthError::forbidden(format!(
            "signed in as {}, but none of that account's {} group(s) appear in this dashboard's \
             grants, so there is nothing it is allowed to see. Ask for membership of one of the \
             configured groups.",
            claims.subject().as_str(),
            groups.len()
        ))
    })?;

    let mut principal = Principal::new(claims.subject().as_str(), role, scopes);
    principal.name = claims
        .name()
        .and_then(|n| n.get(None).or_else(|| n.iter().next().map(|(_, v)| v)))
        .map(|n| n.as_str().to_string())
        .or_else(|| claims.preferred_username().map(|u| u.to_string()))
        .unwrap_or_else(|| claims.subject().to_string());
    principal.email = claims.email().map(|e| e.to_string());
    tracing::info!(
        subject = %principal.subject,
        role = principal.role.as_str(),
        scopes = principal.scopes.len(),
        "signed in"
    );

    let token = auth.start_session(principal);
    let secure = auth.secure_cookies;
    let cookies = [
        set_cookie(
            SESSION_COOKIE,
            &token,
            i64::from(auth.config.session_hours) * 3600,
            secure,
        ),
        clear_cookie(LOGIN_COOKIE, secure),
    ];
    let mut response = Redirect::to(&pending.next).into_response();
    for c in cookies {
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&c).map_err(|e| AuthError::internal(anyhow!(e)))?,
        );
    }
    Ok(response)
}

pub async fn logout(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let auth = app.auth();
    if let Some(token) = read_cookie(&headers, SESSION_COOKIE) {
        auth.end_session(&token);
    }
    // Only this dashboard's session ends. Signing the user out of the identity
    // provider as well would sign them out of everything else in the tenant,
    // which is not this tool's decision to make.
    (
        [(
            header::SET_COOKIE,
            clear_cookie(SESSION_COOKIE, auth.secure_cookies),
        )],
        Redirect::to("/"),
    )
        .into_response()
}

/// What the dashboard needs to know about the current caller.
#[derive(Serialize)]
pub struct Me {
    #[serde(flatten)]
    principal: Principal,
    /// Whether sign-in is configured at all, so the UI can label local access
    /// honestly instead of showing a Sign out button that does nothing.
    sign_in_enabled: bool,
    may_rescan: bool,
}

pub async fn me(State(app): State<Arc<AppState>>, Caller(principal): Caller) -> axum::Json<Me> {
    axum::Json(Me {
        may_rescan: principal.may_rescan(),
        sign_in_enabled: app.auth().enabled(),
        principal,
    })
}

/// Read a claim that the standard set doesn't cover, out of an ID token whose
/// signature has **already been verified** by the caller.
///
/// The payload is re-parsed rather than re-validated: the signature covers this
/// exact byte string, and it was checked a few lines above by
/// `IdToken::claims`. Doing it this way avoids threading a custom
/// `AdditionalClaims` type through the client's dozen generic parameters, and
/// it lets the claim be named in configuration — `groups` on Entra, `roles`
/// where a tenant projects app roles instead.
///
/// It must never be called on a token that has not been verified.
fn groups_from(id_token: &str, claim: &str) -> Result<Vec<String>> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("the ID token is not a three-part JWS"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("the ID token payload is not base64url")?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).context("the ID token payload is not JSON")?;

    Ok(match json.get(claim) {
        // Entra sends an array. Some providers send one value, or a
        // space-separated list.
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(serde_json::Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
        _ => Vec::new(),
    })
}

/// A sign-in that could not be completed.
///
/// Rendered as a page rather than JSON: the reader is a person who has just
/// been redirected back from an identity provider, and the useful thing to tell
/// them is what to ask for.
pub struct AuthError {
    status: StatusCode,
    message: String,
    internal: Option<anyhow::Error>,
}

impl AuthError {
    fn denied(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            internal: None,
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
            internal: None,
        }
    }

    /// 503 rather than 500: nothing is broken, and a proxy or a browser may
    /// reasonably retry a moment later.
    fn busy(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
            internal: None,
        }
    }

    fn internal(e: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{e:#}"),
            internal: Some(e),
        }
    }
}

impl From<anyhow::Error> for AuthError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(e)
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        if let Some(e) = &self.internal {
            tracing::error!(error = format!("{e:#}"), "sign-in failed");
        } else {
            tracing::warn!(status = %self.status, message = %self.message, "sign-in refused");
        }
        // Escaped, because part of this text can come from the provider's
        // error_description.
        let body = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <title>Sign-in failed</title><link rel=\"stylesheet\" href=\"/app.css\"></head>\
             <body><main><div class=\"banner\"><h2>Sign-in failed</h2><p>{}</p>\
             <p><a href=\"/auth/login\">Try again</a></p></div></main></body></html>",
            html_escape(&self.message)
        );
        (
            self.status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            body,
        )
            .into_response()
    }
}

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Grant;

    fn config_with_grants(grants: Vec<Grant>) -> Config {
        Config {
            auth: AuthConfig {
                mode: AuthMode::Oidc,
                issuer: "https://issuer.example/v2.0".into(),
                client_id: "client".into(),
                grants,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn grant(group: &str, role: Role) -> Grant {
        Grant {
            group: group.into(),
            role,
            tags: BTreeMap::new(),
        }
    }

    fn principal(role: Role) -> Principal {
        Principal::new("u1", role, Vec::new())
    }

    #[test]
    fn a_session_survives_a_lookup_and_stops_at_a_sign_out() {
        let auth = Authenticator::new(&config_with_grants(vec![grant("g", Role::Viewer)]));
        let token = auth.start_session(principal(Role::Viewer));
        assert_eq!(auth.lookup(&token).map(|p| p.role), Some(Role::Viewer));
        auth.end_session(&token);
        assert!(auth.lookup(&token).is_none());
    }

    /// The token in the cookie must not be recoverable from the server's own
    /// memory, so what is stored is its hash.
    #[test]
    fn the_session_map_never_holds_the_cookie_value() {
        let auth = Authenticator::new(&Config::default());
        let token = auth.start_session(principal(Role::Admin));
        let sessions = auth.sessions.lock().unwrap();
        assert!(
            !sessions.contains_key(&token),
            "the raw token is the key to a live session"
        );
        assert!(sessions.contains_key(&Authenticator::key(&token)));
    }

    #[test]
    fn a_random_token_is_not_a_session() {
        let auth = Authenticator::new(&Config::default());
        auth.start_session(principal(Role::Admin));
        assert!(auth.lookup(&Authenticator::new_token()).is_none());
        assert!(auth.lookup("").is_none());
    }

    #[test]
    fn an_expired_session_is_not_accepted() {
        let auth = Authenticator::new(&Config::default());
        let token = Authenticator::new_token();
        auth.sessions.lock().unwrap().insert(
            Authenticator::key(&token),
            Session {
                principal: principal(Role::Admin),
                expires: OffsetDateTime::now_utc() - Duration::seconds(1),
            },
        );
        assert!(auth.lookup(&token).is_none());
    }

    #[test]
    fn a_sign_in_can_only_be_completed_once() {
        let auth = Authenticator::new(&Config::default());
        auth.remember_pending(
            "state-1",
            Pending {
                verifier: PkceCodeVerifier::new("v".repeat(43)),
                nonce: Nonce::new("n".into()),
                next: "/".into(),
                expires: OffsetDateTime::now_utc() + Duration::minutes(5),
            },
        );
        assert!(auth.take_pending("state-1").is_some());
        assert!(
            auth.take_pending("state-1").is_none(),
            "replaying a callback must not work"
        );
    }

    #[test]
    fn a_stale_sign_in_is_not_completed() {
        let auth = Authenticator::new(&Config::default());
        auth.remember_pending(
            "old",
            Pending {
                verifier: PkceCodeVerifier::new("v".repeat(43)),
                nonce: Nonce::new("n".into()),
                next: "/".into(),
                expires: OffsetDateTime::now_utc() - Duration::seconds(1),
            },
        );
        assert!(auth.take_pending("old").is_none());
    }

    /// `/auth/login` needs no session, and each call holds a state, a nonce and
    /// a PKCE verifier for fifteen minutes — so without a ceiling, requests
    /// nobody authenticated grow this map for as long as they keep arriving.
    #[test]
    fn half_finished_sign_ins_cannot_grow_without_limit() {
        let auth = Authenticator::new(&Config::default());
        let pending = || Pending {
            verifier: PkceCodeVerifier::new("v".repeat(43)),
            nonce: Nonce::new("n".into()),
            next: "/".into(),
            expires: OffsetDateTime::now_utc() + Duration::minutes(PENDING_TTL_MINUTES),
        };

        for i in 0..MAX_PENDING {
            assert!(
                auth.remember_pending(&format!("state-{i}"), pending()),
                "sign-in {i} is within the cap and must be accepted"
            );
        }
        assert!(
            !auth.remember_pending("one-too-many", pending()),
            "past the cap a new sign-in is refused rather than making room"
        );

        // And the refusal must not have cost anyone their sign-in: evicting to
        // make space would turn a memory bound into a way to deny access.
        assert!(
            auth.take_pending("state-0").is_some(),
            "the oldest in-flight sign-in must still complete"
        );
        assert!(
            auth.remember_pending("now-there-is-room", pending()),
            "taking one should free a slot"
        );
    }

    /// Where sign-in is switched off there is still exactly one authorization
    /// path — the caller is simply a local administrator.
    #[test]
    fn local_access_is_an_unauthenticated_administrator() {
        let p = Principal::local();
        assert_eq!(p.role, Role::Admin);
        assert!(p.may_rescan());
        assert!(p.scopes.is_empty());
        assert!(!p.authenticated, "nobody signed in, and the UI must say so");
    }

    #[test]
    fn cookies_carry_the_attributes_that_matter() {
        let c = set_cookie(SESSION_COOKIE, "abc", 3600, true);
        assert!(c.contains("HttpOnly"), "{c}");
        assert!(c.contains("Secure"), "{c}");
        // Strict would withhold the cookie on the callback navigation back from
        // the identity provider, breaking sign-in.
        assert!(c.contains("SameSite=Lax"), "{c}");
        assert!(c.contains("Path=/"), "{c}");
        assert!(
            !set_cookie(SESSION_COOKIE, "abc", 3600, false).contains("Secure"),
            "Secure over plain http would make the cookie unusable"
        );
        assert!(clear_cookie(SESSION_COOKIE, true).contains("Max-Age=0"));
    }

    #[test]
    fn a_cookie_is_read_out_of_a_crowded_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; lbf_session=wanted; another=2"),
        );
        assert_eq!(
            read_cookie(&headers, SESSION_COOKIE),
            Some("wanted".to_string())
        );
        assert_eq!(read_cookie(&headers, "absent"), None);
    }

    /// An open redirect in a sign-in flow lets a phishing page borrow a real
    /// domain, so anything that is not a path on this site is discarded.
    #[test]
    fn the_post_sign_in_destination_cannot_leave_this_site() {
        assert_eq!(safe_next(Some("/#/cohorts".into())), "/#/cohorts");
        assert_eq!(
            safe_next(Some("/api/snapshot?q=x".into())),
            "/api/snapshot?q=x"
        );
        for hostile in [
            "//evil.example.com",
            "https://evil.example.com",
            "http://evil.example.com",
            "/\\evil.example.com",
            "javascript:alert(1)",
            "",
            // A browser strips these from a URL before acting on it, and they
            // cannot go in a `Location` header either.
            "/\nevil",
            "/\revil",
            "/\tevil",
            "/ok\u{0}",
        ] {
            assert_eq!(
                safe_next(Some(hostile.into())),
                "/",
                "{hostile:?} must not be honoured"
            );
        }
        assert_eq!(safe_next(None), "/");
    }

    fn jwt_with(payload: serde_json::Value) -> String {
        let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            b64(br#"{"alg":"RS256"}"#),
            b64(payload.to_string().as_bytes()),
            b64(b"not-checked-here")
        )
    }

    #[test]
    fn group_claims_are_read_in_the_shapes_providers_actually_send() {
        let array = jwt_with(serde_json::json!({"sub": "u", "groups": ["a", "b"]}));
        assert_eq!(groups_from(&array, "groups").unwrap(), vec!["a", "b"]);

        // A single value, and a space-separated list.
        let one = jwt_with(serde_json::json!({"groups": "only"}));
        assert_eq!(groups_from(&one, "groups").unwrap(), vec!["only"]);
        let spaced = jwt_with(serde_json::json!({"groups": "a b"}));
        assert_eq!(groups_from(&spaced, "groups").unwrap(), vec!["a", "b"]);

        // A configurable claim name, because some tenants project app roles.
        let roles = jwt_with(serde_json::json!({"roles": ["fleet.admin"]}));
        assert_eq!(groups_from(&roles, "roles").unwrap(), vec!["fleet.admin"]);

        // Absent, or the wrong type, is "no groups" - which the caller turns
        // into a refusal rather than into access.
        assert!(
            groups_from(&jwt_with(serde_json::json!({})), "groups")
                .unwrap()
                .is_empty()
        );
        assert!(
            groups_from(&jwt_with(serde_json::json!({"groups": 7})), "groups")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_malformed_id_token_is_an_error_not_an_empty_group_list() {
        assert!(groups_from("not-a-jwt", "groups").is_err());
        assert!(groups_from("a.!!!not-base64!!!.c", "groups").is_err());
        assert!(groups_from("a.aGVsbG8.c", "groups").is_err(), "not JSON");
    }

    #[test]
    fn provider_error_text_cannot_inject_markup_into_the_failure_page() {
        let e = AuthError::denied("<script>alert(1)</script>");
        let body = format!("{:?}", e.into_response());
        assert!(
            !body.contains("<script>"),
            "the response was built with the raw string"
        );
        assert_eq!(
            html_escape("<a href='x'>&</a>"),
            "&lt;a href=&#39;x&#39;&gt;&amp;&lt;/a&gt;"
        );
    }

    #[test]
    fn authenticator_reflects_whether_sign_in_is_configured() {
        assert!(!Authenticator::new(&Config::default()).enabled());
        let auth = Authenticator::new(&config_with_grants(vec![grant("g", Role::Admin)]));
        assert!(auth.enabled());
        assert_eq!(auth.redirect_url, "http://127.0.0.1:8787/auth/callback");
        assert!(!auth.secure_cookies, "the default public_url is plain http");
    }
}
