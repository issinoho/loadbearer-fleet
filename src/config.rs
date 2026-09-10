//! The configuration file.
//!
//! Everything here can also be passed on the command line; the file exists
//! because a service doesn't have a command line anyone reads, and because the
//! authentication settings are too many to be flags.
//!
//! Two things it does deliberately.
//!
//! **It refuses to start on a placeholder.** A starter file is full of
//! `PUT-YOUR-TENANT-ID-HERE`, and an issuer URL with that still in it produces
//! a redirect to a tenant that doesn't exist and an error message from the
//! identity provider that says nothing useful. So the values are checked for
//! the placeholder marker at load time and named individually.
//!
//! **It validates the combination, not just the fields.** Serving an
//! unauthenticated dashboard, or a session cookie, over plain HTTP on a network
//! interface is the mistake worth catching, and it is a property of several
//! settings together rather than any one of them.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The marker a starter file uses. Any setting still containing it is a setting
/// nobody filled in.
const PLACEHOLDER: &str = "PUT-";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default)]
    pub metrics: Metrics,
    #[serde(default)]
    pub log: Log,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub bind: SocketAddr,
    /// The URL users actually reach this on — which is not the bind address
    /// once there's a reverse proxy in front. The OIDC redirect is built from
    /// it, and `https` here is what allows the session cookie to be `Secure`.
    pub public_url: String,
    /// The collection folder. Scanned at startup and on demand.
    #[serde(default)]
    pub collection_dir: Option<PathBuf>,
    /// Where the derived index lives. Safe to delete.
    pub index: PathBuf,
    /// How often to rescan the folder by itself. Zero switches it off and
    /// leaves rescanning to the button.
    ///
    /// This is what makes it a service rather than a command: a dashboard that
    /// only refreshes when somebody happens to click is a dashboard that is out
    /// of date exactly when nobody is looking at it.
    #[serde(default = "scan_interval_minutes")]
    pub scan_interval_minutes: u64,
}

fn scan_interval_minutes() -> u64 {
    15
}

/// Prometheus metrics, off unless asked for.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Metrics {
    #[serde(default)]
    pub enabled: bool,
    /// Required as `Authorization: Bearer <token>` when set. Fleet-wide counts
    /// are not hostnames, but "how many machines this organisation has and how
    /// many are failing" is still something to put behind a credential once the
    /// port is reachable from anywhere but this host.
    #[serde(default)]
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable, for a console and for a person.
    #[default]
    Text,
    /// One JSON object per line, for a log collector.
    Json,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    #[serde(default)]
    pub format: LogFormat,
    #[serde(default = "log_level")]
    pub level: String,
    /// Where to write the log. Rotated daily, with the date appended.
    ///
    /// Required to run as a service, and the installer refuses without it: a
    /// service has no console, so with no log file there is no way at all to
    /// find out why it did not start.
    #[serde(default)]
    pub file: Option<PathBuf>,
}

fn log_level() -> String {
    "info".to_string()
}

impl Default for Log {
    fn default() -> Self {
        Self {
            format: LogFormat::default(),
            level: log_level(),
            file: None,
        }
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8787".parse().expect("literal"),
            public_url: "http://127.0.0.1:8787".to_string(),
            collection_dir: None,
            index: PathBuf::from("fleet-index.db"),
            scan_interval_minutes: scan_interval_minutes(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Everyone who can reach the port is an administrator. Only sane on
    /// loopback, which is enforced rather than suggested.
    #[default]
    None,
    /// OpenID Connect authorization code flow with PKCE.
    Oidc,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    #[serde(default)]
    pub mode: AuthMode,
    /// e.g. `https://login.microsoftonline.com/<tenant-id>/v2.0`. Discovery
    /// appends `/.well-known/openid-configuration` itself.
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub client_id: String,
    /// Empty for a public client, which is the intended shape: the
    /// authorization code flow with PKCE needs no secret, and a secret in a
    /// config file on a management server is a secret in a backup.
    #[serde(default)]
    pub client_secret: String,
    /// Which claim carries the caller's group memberships. Entra ID calls it
    /// `groups`; some tenants project app roles into `roles` instead.
    #[serde(default = "groups_claim")]
    pub groups_claim: String,
    /// How long a session lasts before the user signs in again.
    #[serde(default = "session_hours")]
    pub session_hours: u32,
    /// Scopes beyond `openid profile email`. Entra needs none of these for
    /// group claims — those come from the token configuration on the app
    /// registration, not from a scope.
    #[serde(default)]
    pub extra_scopes: Vec<String>,
    /// Group-to-role mapping. Order matters only as a tie-break; see
    /// `Auth::resolve`.
    #[serde(default)]
    pub grants: Vec<Grant>,
}

fn groups_claim() -> String {
    "groups".to_string()
}

fn session_hours() -> u32 {
    8
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            mode: AuthMode::None,
            issuer: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            groups_claim: groups_claim(),
            session_hours: session_hours(),
            extra_scopes: Vec::new(),
            grants: Vec::new(),
        }
    }
}

/// "Members of this group get this role, over this much of the estate."
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// The group's object ID, not its display name. Display names are
    /// renameable and not unique; Entra puts object IDs in the token anyway.
    pub group: String,
    pub role: Role,
    /// Restrict this grant to machines carrying all of these tags. Absent means
    /// the whole fleet.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read the dashboard.
    Viewer,
    /// Read the dashboard, and trigger a rescan.
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Admin => "admin",
        }
    }
}

/// What a signed-in caller is allowed to see and do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entitlement {
    pub role: Role,
    /// Tag sets, any one of which a machine may match. Empty means the whole
    /// fleet.
    pub scopes: Vec<BTreeMap<String, String>>,
}

impl Auth {
    /// Turn a token's group claims into an entitlement, or `None` if none of
    /// the caller's groups appear in any grant — which is a refusal, not an
    /// empty view. Somebody who can authenticate is not thereby authorized.
    ///
    /// The most privileged matching grant sets the role. Scopes are the
    /// **union** of every matching grant at that role, so somebody in two site
    /// groups sees both sites; a matching grant with no tags means the whole
    /// fleet and swallows the rest.
    pub fn resolve(&self, groups: &[String]) -> Option<Entitlement> {
        let matched: Vec<&Grant> = self
            .grants
            .iter()
            .filter(|g| groups.iter().any(|had| had == &g.group))
            .collect();
        let role = matched.iter().map(|g| g.role).max()?;

        let at_role: Vec<&Grant> = matched.into_iter().filter(|g| g.role == role).collect();
        let scopes = if at_role.iter().any(|g| g.tags.is_empty()) {
            Vec::new()
        } else {
            at_role.iter().map(|g| g.tags.clone()).collect()
        };
        Some(Entitlement { role, scopes })
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        let mut config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config from {}", path.display()))?;
        config.resolve_paths_against(path.parent().unwrap_or(Path::new(".")));
        config
            .validate()
            .with_context(|| format!("in {}", path.display()))?;
        Ok(config)
    }

    /// Make relative paths mean "beside the config file".
    ///
    /// Windows starts a service in `%SystemRoot%\System32`, and systemd in
    /// `/`, so a relative `index = "fleet-index.db"` would otherwise put the
    /// database somewhere nobody expects and nobody backs up — and it would
    /// land somewhere *different* depending on how the process was started.
    /// Resolving against the config file makes the answer the same either way.
    fn resolve_paths_against(&mut self, base: &Path) {
        self.server.index = beside(base, &self.server.index);
        // A UNC share is already absolute, so the usual case is untouched.
        self.server.collection_dir = self
            .server
            .collection_dir
            .as_deref()
            .map(|p| beside(base, p));
        self.log.file = self.log.file.as_deref().map(|p| beside(base, p));
    }

    /// Is the dashboard reachable over a channel that hides a session cookie?
    pub fn public_url_is_https(&self) -> bool {
        self.server.public_url.starts_with("https://")
    }

    pub fn redirect_url(&self) -> String {
        format!(
            "{}/auth/callback",
            self.server.public_url.trim_end_matches('/')
        )
    }

    fn validate(&self) -> Result<()> {
        if self.server.public_url.trim().is_empty() {
            bail!("server.public_url is empty; it is what the sign-in redirect is built from");
        }
        if !self.server.public_url.starts_with("http") {
            bail!(
                "server.public_url must be a URL starting http:// or https://, got {:?}",
                self.server.public_url
            );
        }
        check_placeholder("server.public_url", &self.server.public_url)?;

        if self.auth.mode == AuthMode::None {
            if !self.auth.grants.is_empty() {
                // Silently ignoring the grants would look like they applied.
                bail!(
                    "auth.mode is \"none\" but {} grant(s) are configured; either set \
                     auth.mode = \"oidc\" or remove them, because with no sign-in there is \
                     nobody to apply them to",
                    self.auth.grants.len()
                );
            }
            return Ok(());
        }

        check_placeholder("auth.issuer", &self.auth.issuer)?;
        check_placeholder("auth.client_id", &self.auth.client_id)?;
        if self.auth.issuer.trim().is_empty() {
            bail!("auth.mode is \"oidc\" but auth.issuer is empty");
        }
        if !self.auth.issuer.starts_with("https://") {
            bail!(
                "auth.issuer must be https, got {:?} — an identity provider reached over plain \
                 HTTP can be impersonated",
                self.auth.issuer
            );
        }
        // Verified against the live endpoint: Entra's multi-tenant issuers
        // return a *templated* issuer, `https://login.microsoftonline.com/
        // {tenantid}/v2.0`, which can never equal the URL discovery was
        // requested from. Strict issuer checking is not something to relax —
        // it is what stops one tenant's tokens being accepted as another's —
        // so the answer is the tenant-specific issuer, and the failure is far
        // less puzzling here than three redirects later.
        for shared in ["/common/", "/organizations/", "/consumers/"] {
            if self.auth.issuer.contains(shared) {
                bail!(
                    "auth.issuer is a multi-tenant endpoint ({shared}), which cannot work: it \
                     advertises its issuer as \"https://login.microsoftonline.com/{{tenantid}}/\
                     v2.0\", a template that never matches the URL it was fetched from, so \
                     issuer validation fails. Use your own tenant: \
                     https://login.microsoftonline.com/<your-tenant-id>/v2.0"
                );
            }
        }
        if self.auth.client_id.trim().is_empty() {
            bail!("auth.mode is \"oidc\" but auth.client_id is empty");
        }
        if self.auth.session_hours == 0 {
            bail!("auth.session_hours is 0, which would sign every user out immediately");
        }
        if self.auth.grants.is_empty() {
            // Authentication with no grants authorizes nobody, so the dashboard
            // would be unreachable by design. Better to say so now.
            bail!(
                "auth.mode is \"oidc\" but no [[auth.grants]] are configured, so every \
                 successful sign-in would still be refused. Add at least one group."
            );
        }
        for (i, g) in self.auth.grants.iter().enumerate() {
            check_placeholder(&format!("auth.grants[{i}].group"), &g.group)?;
            if g.group.trim().is_empty() {
                bail!("auth.grants[{i}].group is empty");
            }
            for (k, v) in &g.tags {
                check_placeholder(&format!("auth.grants[{i}].tags.{k}"), v)?;
            }
        }
        Ok(())
    }

    /// A starter file. Every value that has to be filled in carries the
    /// placeholder marker, so a copy of this refuses to start until it has been
    /// edited rather than half-working.
    pub fn starter() -> String {
        let example = r##"# loadbearer-fleet configuration.
#
# Start it with:  loadbearer-fleet serve --config loadbearer-fleet.toml
# Check the identity provider settings without waiting for a user to try:
#                 loadbearer-fleet check-auth --config loadbearer-fleet.toml

[server]
# The address to listen on. Leave this on loopback and put a reverse proxy in
# front of it for TLS; the dashboard does not terminate TLS itself.
bind = "127.0.0.1:8787"

# The URL your users actually type. Behind a proxy this is the proxy's URL, not
# the bind address: the sign-in redirect is built from it, and https here is
# what lets the session cookie be marked Secure.
public_url = "https://PUT-THE-HOSTNAME-USERS-REACH-THIS-ON-HERE"

# The folder your deployment tool drops loadbearer result files into.
collection_dir = 'PUT-THE-PATH-TO-YOUR-RESULTS-SHARE-HERE'

# Derived data: delete it and it rebuilds from the folder above. If an upgrade
# ever changes its internal shape, the old file is renamed rather than dropped -
# see "Upgrading" in the README, because what that costs you depends on whether
# your collector keeps a file per run.
index = "fleet-index.db"

[auth]
# "oidc" for single sign-on, "none" for an unauthenticated dashboard — which is
# refused on anything but a loopback bind.
mode = "oidc"

# Entra ID: https://login.microsoftonline.com/<tenant-id>/v2.0
# The tenant ID is the Directory (tenant) ID on the app registration overview.
#
# It must be your tenant's own ID. The /common/, /organizations/ and
# /consumers/ endpoints cannot be used: they advertise their issuer as a
# template ("{tenantid}"), which never matches the URL it was fetched from, so
# the token's issuer can't be validated. This is checked at startup.
issuer = "https://login.microsoftonline.com/PUT-YOUR-TENANT-ID-HERE/v2.0"

# The Application (client) ID of the app registration.
client_id = "PUT-YOUR-APPLICATION-CLIENT-ID-HERE"

# Leave empty. The authorization code flow with PKCE needs no secret, and a
# secret in a config file on a management server is a secret in every backup of
# that server. Register the app as a public client with the redirect URI
# <public_url>/auth/callback and the "Web" platform.
client_secret = ""

# Which token claim carries group membership. On Entra, configure the app
# registration's Token configuration to emit the "groups" claim (Security
# groups, or Groups assigned to the application) — group claims are a token
# setting, not a scope. If you project app roles instead, set this to "roles"
# and put the role values in the grants below.
groups_claim = "groups"

# How long a session lasts before signing in again.
session_hours = 8

[log]
# "text" for a console, "json" for a log collector.
format = "text"
level = "info"

# Where to write the log, rotated daily with the date appended. Required to run
# as a service: a service has no console, so without this there is no way to
# find out why it did not start.
file = 'PUT-THE-PATH-FOR-THE-LOG-FILE-HERE'

[metrics]
# Serves Prometheus metrics on /metrics. Fleet-wide counts only - no machine,
# hostname or cohort ever appears in a label, because a metrics store is
# usually less protected than this service is.
enabled = false

# Required as "Authorization: Bearer <token>" when set. Leave empty only if the
# port is reachable from this host alone.
token = ""

# Who gets in, and over how much of the estate. Use the group's object ID, not
# its display name: names are renameable and not unique, and the token carries
# object IDs anyway.
#
# The most privileged matching grant wins. Scopes are the union of the matching
# grants at that role, so somebody in two site groups sees both sites.

[[auth.grants]]
group = "PUT-THE-OBJECT-ID-OF-YOUR-FLEET-ADMINS-GROUP-HERE"
role = "admin"    # read the dashboard, and trigger a rescan

[[auth.grants]]
group = "PUT-THE-OBJECT-ID-OF-YOUR-FLEET-VIEWERS-GROUP-HERE"
role = "viewer"   # read the dashboard

# A team that should only see its own site. Tags come from loadbearer's --tag,
# so this only means anything if your deployment tool sets them.
#
# [[auth.grants]]
# group = "PUT-THE-OBJECT-ID-OF-THE-GLASGOW-DESKTOP-TEAM-HERE"
# role = "viewer"
# tags = { site = "glasgow" }
"##;
        example.to_string()
    }
}

fn beside(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() || base.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn check_placeholder(field: &str, value: &str) -> Result<()> {
    if value.contains(PLACEHOLDER) {
        bail!(
            "{field} still has the starter placeholder in it ({value:?}). Fill it in — a \
             placeholder here fails later, at the identity provider, with an error that says \
             nothing useful."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(group: &str, role: Role, tags: &[(&str, &str)]) -> Grant {
        Grant {
            group: group.to_string(),
            role,
            tags: tags
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    fn auth_with(grants: Vec<Grant>) -> Auth {
        Auth {
            mode: AuthMode::Oidc,
            grants,
            ..Default::default()
        }
    }

    #[test]
    fn the_starter_file_parses_but_refuses_to_run() {
        let text = Config::starter();
        let parsed: Config = toml::from_str(&text).expect("the starter file must be valid TOML");
        assert_eq!(parsed.auth.mode, AuthMode::Oidc);
        let err = parsed
            .validate()
            .expect_err("a file full of placeholders must not start")
            .to_string();
        assert!(err.contains("placeholder"), "{err}");
    }

    /// The failure this check exists for: the admin filled in the tenant but
    /// not the client ID, so sign-in would fail at the provider with something
    /// unhelpful.
    #[test]
    fn a_half_filled_config_names_the_field_still_missing() {
        let mut c = Config {
            auth: auth_with(vec![grant("00000000-group", Role::Admin, &[])]),
            ..Default::default()
        };
        c.auth.issuer = "https://login.microsoftonline.com/real-tenant/v2.0".into();
        c.auth.client_id = "PUT-YOUR-APPLICATION-CLIENT-ID-HERE".into();
        let err = c.validate().expect_err("must refuse").to_string();
        assert!(err.contains("auth.client_id"), "{err}");
    }

    #[test]
    fn authentication_with_nobody_authorized_is_refused_at_load() {
        let c = Config {
            auth: Auth {
                mode: AuthMode::Oidc,
                issuer: "https://issuer.example/v2.0".into(),
                client_id: "abc".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = c.validate().expect_err("must refuse").to_string();
        assert!(err.contains("grants"), "{err}");
    }

    /// Grants under `mode = "none"` would look like they were being applied.
    #[test]
    fn grants_without_sign_in_are_refused_rather_than_ignored() {
        let c = Config {
            auth: Auth {
                mode: AuthMode::None,
                grants: vec![grant("g", Role::Viewer, &[])],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    /// Confirmed against the live endpoint before this check existed: the
    /// multi-tenant issuers advertise a templated issuer and fail validation.
    #[test]
    fn a_multi_tenant_issuer_is_refused_with_the_reason() {
        for shared in [
            "https://login.microsoftonline.com/common/v2.0",
            "https://login.microsoftonline.com/organizations/v2.0",
            "https://login.microsoftonline.com/consumers/v2.0",
        ] {
            let c = Config {
                auth: Auth {
                    mode: AuthMode::Oidc,
                    issuer: shared.into(),
                    client_id: "abc".into(),
                    grants: vec![grant("g", Role::Viewer, &[])],
                    ..Default::default()
                },
                ..Default::default()
            };
            let err = c.validate().expect_err(shared).to_string();
            assert!(err.contains("multi-tenant"), "{err}");
            assert!(
                err.contains("your-tenant-id"),
                "it must say what to use instead: {err}"
            );
        }
    }

    #[test]
    fn a_tenant_specific_issuer_passes() {
        let c = Config {
            auth: Auth {
                mode: AuthMode::Oidc,
                issuer:
                    "https://login.microsoftonline.com/11111111-2222-3333-4444-555555555555/v2.0"
                        .into(),
                client_id: "abc".into(),
                grants: vec![grant("g", Role::Viewer, &[])],
                ..Default::default()
            },
            ..Default::default()
        };
        c.validate().expect("a real tenant issuer is fine");
    }

    #[test]
    fn an_issuer_over_plain_http_is_refused() {
        let c = Config {
            auth: Auth {
                mode: AuthMode::Oidc,
                issuer: "http://login.example.com/v2.0".into(),
                client_id: "abc".into(),
                grants: vec![grant("g", Role::Viewer, &[])],
                ..Default::default()
            },
            ..Default::default()
        };
        let err = c.validate().expect_err("must refuse").to_string();
        assert!(err.contains("https"), "{err}");
    }

    #[test]
    fn the_default_config_is_a_loopback_dashboard_with_no_sign_in() {
        let c = Config::default();
        c.validate().expect("the default must be usable as-is");
        assert_eq!(c.auth.mode, AuthMode::None);
        assert!(c.server.bind.ip().is_loopback());
        assert!(!c.public_url_is_https());
    }

    #[test]
    fn the_redirect_url_survives_a_trailing_slash() {
        let mut c = Config::default();
        c.server.public_url = "https://fleet.example.com/".into();
        assert_eq!(c.redirect_url(), "https://fleet.example.com/auth/callback");
    }

    #[test]
    fn the_most_privileged_matching_grant_sets_the_role() {
        let auth = auth_with(vec![
            grant("viewers", Role::Viewer, &[]),
            grant("admins", Role::Admin, &[]),
        ]);
        let e = auth
            .resolve(&["viewers".into(), "admins".into()])
            .expect("both groups match");
        assert_eq!(e.role, Role::Admin);
        assert!(e.scopes.is_empty(), "unrestricted");
    }

    #[test]
    fn a_caller_in_no_granted_group_is_refused_rather_than_shown_nothing() {
        let auth = auth_with(vec![grant("admins", Role::Admin, &[])]);
        assert!(
            auth.resolve(&["some-other-group".into()]).is_none(),
            "authenticating is not being authorized"
        );
        assert!(auth.resolve(&[]).is_none(), "no groups in the token at all");
    }

    #[test]
    fn two_site_grants_union_into_seeing_both_sites() {
        let auth = auth_with(vec![
            grant("glasgow", Role::Viewer, &[("site", "glasgow")]),
            grant("edinburgh", Role::Viewer, &[("site", "edinburgh")]),
            grant("perth", Role::Viewer, &[("site", "perth")]),
        ]);
        let e = auth
            .resolve(&["glasgow".into(), "edinburgh".into()])
            .expect("two grants match");
        assert_eq!(e.role, Role::Viewer);
        assert_eq!(e.scopes.len(), 2, "both sites, not one and not all three");
        assert!(e.scopes.iter().any(|s| s["site"] == "glasgow"));
        assert!(e.scopes.iter().any(|s| s["site"] == "edinburgh"));
    }

    /// A grant with no tags means the whole fleet, so it has to swallow the
    /// scoped ones rather than being ANDed with them.
    #[test]
    fn an_unscoped_grant_beats_a_scoped_one_at_the_same_role() {
        let auth = auth_with(vec![
            grant("glasgow", Role::Viewer, &[("site", "glasgow")]),
            grant("everyone", Role::Viewer, &[]),
        ]);
        let e = auth
            .resolve(&["glasgow".into(), "everyone".into()])
            .expect("matches");
        assert!(e.scopes.is_empty(), "the whole fleet");
    }

    /// Role wins before scope: an admin scoped to one site stays scoped to it,
    /// and a viewer grant elsewhere must not widen that.
    #[test]
    fn a_scoped_admin_does_not_inherit_a_viewers_wider_scope() {
        let auth = auth_with(vec![
            grant("site-admins", Role::Admin, &[("site", "glasgow")]),
            grant("all-viewers", Role::Viewer, &[]),
        ]);
        let e = auth
            .resolve(&["site-admins".into(), "all-viewers".into()])
            .expect("matches");
        assert_eq!(e.role, Role::Admin);
        assert_eq!(e.scopes.len(), 1);
        assert_eq!(e.scopes[0]["site"], "glasgow");
    }

    #[test]
    fn an_unknown_key_in_the_file_is_an_error_not_a_shrug() {
        // A typo in a security setting must not be silently ignored.
        let err = toml::from_str::<Config>("[auth]\nmode = \"oidc\"\nissuerr = \"x\"\n")
            .expect_err("unknown keys are refused");
        assert!(err.to_string().contains("issuerr"), "{err}");
    }
}
