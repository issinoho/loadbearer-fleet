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
//! That includes the paths, which fail in a worse way than the identity
//! settings do: they don't fail at all. An unedited `log.file` creates a
//! directory named `PUT-THE-PATH-FOR-THE-LOG-FILE-HERE` and logs into it
//! contentedly, and an unedited `collection_dir` is an unreachable folder,
//! which is reported and survived by design. Both leave you with something
//! that looks configured.
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
use time::OffsetDateTime;
use time::macros::format_description;

/// The marker a starter file uses. Any setting still containing it is a setting
/// nobody filled in.
const PLACEHOLDER: &str = "PUT-";

/// The shortest ingest token this will accept.
///
/// 32 hex characters is 128 bits, which is the point at which guessing stops
/// being a strategy. The refusal names the command that produces one, because
/// a length rule without a way to satisfy it just gets worked around.
const MIN_INGEST_TOKEN_LEN: usize = 32;

/// What `token new` prefixes a secret with. Recognisable on sight, and to the
/// secret scanners that look for exactly this kind of marker in a repository
/// or a paste — which is where a deployment credential usually turns up.
pub const INGEST_TOKEN_PREFIX: &str = "lbf_";

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
    pub ingest: Ingest,
    #[serde(default)]
    pub notify: Notify,
    #[serde(default)]
    pub thresholds: ThresholdSettings,
    #[serde(default)]
    pub log: Log,
}

/// `[thresholds]`: the lines the findings are drawn against. Every value is
/// optional and an unset one keeps the built-in default, so a file without the
/// section judges exactly as before it existed.
///
/// Read through a table rather than derived, because the values and the
/// `overrides` list share one TOML table and serde cannot refuse unknown keys
/// across a flattened struct. Refusing them matters as much here as anywhere:
/// `stale_day = 365` silently ignored is a lab that stays stale.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "toml::Table")]
pub struct ThresholdSettings {
    #[serde(flatten)]
    pub values: ThresholdValues,
    /// Tried in file order; see `analytics::thresholds` for how a machine's
    /// lines are chosen from them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<ThresholdOverride>,
}

/// One value per threshold, each optional. The names and meanings are
/// `analytics::Thresholds`', where each is documented with its reasoning.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThresholdValues {
    pub min_cohort: Option<usize>,
    pub cohort_z: Option<f64>,
    pub cohort_min_shortfall: Option<f64>,
    pub mad_floor_frac: Option<f64>,
    pub regression_frac: Option<f64>,
    pub stale_days: Option<f64>,
    pub battery_health_pct: Option<f64>,
    pub unstable_cpu_subtests: Option<i64>,
    pub drive_wear_pct: Option<f64>,
    pub drive_spare_pct: Option<f64>,
    pub disk_free_pct: Option<f64>,
    pub disk_free_critical_pct: Option<f64>,
    pub firmware_behind_days: Option<f64>,
    pub firmware_old_years: Option<f64>,
    pub replace_age_years: Option<f64>,
}

impl ThresholdValues {
    /// The values that are set, by name, in `analytics::FIELDS`' order.
    pub fn entries(&self) -> Vec<(&'static str, f64)> {
        [
            ("min_cohort", self.min_cohort.map(|v| v as f64)),
            ("cohort_z", self.cohort_z),
            ("cohort_min_shortfall", self.cohort_min_shortfall),
            ("mad_floor_frac", self.mad_floor_frac),
            ("regression_frac", self.regression_frac),
            ("stale_days", self.stale_days),
            ("battery_health_pct", self.battery_health_pct),
            (
                "unstable_cpu_subtests",
                self.unstable_cpu_subtests.map(|v| v as f64),
            ),
            ("drive_wear_pct", self.drive_wear_pct),
            ("drive_spare_pct", self.drive_spare_pct),
            ("disk_free_pct", self.disk_free_pct),
            ("disk_free_critical_pct", self.disk_free_critical_pct),
            ("firmware_behind_days", self.firmware_behind_days),
            ("firmware_old_years", self.firmware_old_years),
            ("replace_age_years", self.replace_age_years),
        ]
        .into_iter()
        .filter_map(|(name, v)| v.map(|v| (name, v)))
        .collect()
    }

    /// The rest of a table once its other keys are taken out. A key that is
    /// not a threshold is refused here, with the list, rather than by serde,
    /// whose message cannot mention the keys taken out first.
    fn from_rest(rest: toml::Table, at: &str, others: &str) -> Result<Self, String> {
        for (key, value) in &rest {
            let Some(f) = crate::analytics::field(key) else {
                return Err(format!(
                    "{at}{key} is not a setting here. It takes {others}any of: {}",
                    crate::analytics::FIELDS
                        .iter()
                        .map(|f| f.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };
            // Said here, by name: serde's own message for a value of the
            // wrong type does not say which key it was.
            match value {
                toml::Value::Integer(_) => {}
                toml::Value::Float(_) if !f.whole() => {}
                toml::Value::Float(v) => {
                    return Err(format!(
                        "{at}{key} is {v}; it is a count, so a whole number"
                    ));
                }
                other => return Err(format!("{at}{key} is {other}; it wants a number")),
            }
        }
        toml::Value::Table(rest)
            .try_into()
            .map_err(|e| format!("{at}{}", e.message()))
    }
}

/// A `[[thresholds.overrides]]` rule: different lines for the machines it
/// picks.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "toml::Table")]
pub struct ThresholdOverride {
    /// What people see — on the machine page and in the snapshot. Unset, it
    /// is made from the selector: `site=lab`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Every one of these must be among the machine's tags, matched exactly as
    /// a grant's tags are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<BTreeMap<String, String>>,
    /// Exactly as the machine reports it, and as the Groups view lists it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    #[serde(flatten)]
    pub values: ThresholdValues,
}

/// One optional key out of a table, as `T`, the error naming the key.
fn take<T: serde::de::DeserializeOwned>(
    t: &mut toml::Table,
    key: &str,
    at: &str,
) -> Result<Option<T>, String> {
    t.remove(key)
        .map(|v| {
            v.try_into()
                .map_err(|e| format!("{at}{key}: {}", e.message()))
        })
        .transpose()
}

impl TryFrom<toml::Table> for ThresholdSettings {
    type Error = String;

    fn try_from(mut t: toml::Table) -> Result<Self, String> {
        let overrides = take(&mut t, "overrides", "thresholds.")?.unwrap_or_default();
        Ok(Self {
            values: ThresholdValues::from_rest(t, "thresholds.", "overrides and ")?,
            overrides,
        })
    }
}

impl TryFrom<toml::Table> for ThresholdOverride {
    type Error = String;

    fn try_from(mut t: toml::Table) -> Result<Self, String> {
        const AT: &str = "[[thresholds.overrides]] ";
        Ok(Self {
            name: take(&mut t, "name", AT)?,
            tags: take(&mut t, "tags", AT)?,
            system_model: take(&mut t, "system_model", AT)?,
            cpu_model: take(&mut t, "cpu_model", AT)?,
            values: ThresholdValues::from_rest(t, AT, "name, tags, system_model, cpu_model, and ")?,
        })
    }
}

impl ThresholdSettings {
    /// The thresholds the engine runs with, or the first thing wrong with the
    /// section, named by setting. `validate` calls this, so on a loaded config
    /// it cannot fail.
    pub fn tuning(&self) -> Result<crate::analytics::Tuning> {
        use crate::analytics::{Override, Thresholds, Tuning, field};

        let mut fleet = Thresholds::default();
        for (name, value) in self.values.entries() {
            let f = field(name).expect("entries names only thresholds");
            if let Err(why) = f.check(value) {
                bail!("thresholds.{name} {why}");
            }
            f.apply(&mut fleet, value);
        }
        if fleet.disk_free_critical_pct > fleet.disk_free_pct {
            bail!(
                "thresholds.disk_free_critical_pct ({}) is above disk_free_pct ({}). Critical is \
                 the lower line — a disk under it is under both — so with these values every \
                 nearly full disk would be critical and none a warning",
                fleet.disk_free_critical_pct,
                fleet.disk_free_pct
            );
        }

        let mut overrides: Vec<Override> = Vec::new();
        for (i, o) in self.overrides.iter().enumerate() {
            let tags = o.tags.clone().unwrap_or_default();
            let derived =
                Override::describe(&tags, o.system_model.as_deref(), o.cpu_model.as_deref());
            let at = format!(
                "thresholds.overrides[{i}] ({:?})",
                o.name.as_deref().unwrap_or(&derived)
            );
            if o.tags.as_ref().is_some_and(BTreeMap::is_empty) {
                bail!(
                    "{at} has an empty tags table, which would pick every machine. Give the tags \
                     it should pick, or put the values in [thresholds] if they are for the whole \
                     fleet"
                );
            }
            if o.tags.is_none() && o.system_model.is_none() && o.cpu_model.is_none() {
                bail!(
                    "{at} picks no machines: give it tags, a system_model or a cpu_model. Values \
                     for the whole fleet belong in [thresholds] itself"
                );
            }
            for (field_name, value) in [
                ("system_model", o.system_model.as_deref()),
                ("cpu_model", o.cpu_model.as_deref()),
                ("name", o.name.as_deref()),
            ] {
                if let Some(v) = value {
                    if v.trim().is_empty() {
                        bail!(
                            "{at} has an empty {field_name}. Leave it out instead{}",
                            if field_name == "name" {
                                " — the rule is then named after what it picks"
                            } else {
                                ": a machine with no value for it is never picked by one"
                            }
                        );
                    }
                    check_placeholder(&format!("{at} {field_name}"), v)?;
                }
            }
            for (k, v) in &tags {
                if k.trim().is_empty() || v.trim().is_empty() {
                    bail!("{at} has an empty tag key or value, which no machine carries");
                }
                check_placeholder(&format!("{at} tags.{k}"), v)?;
            }

            let mut set = BTreeMap::new();
            for (name, value) in o.values.entries() {
                let f = field(name).expect("entries names only thresholds");
                if !f.per_machine {
                    bail!(
                        "{at} sets {name}, which is fleet-wide only. min_cohort, cohort_z, \
                         cohort_min_shortfall and mad_floor_frac describe the comparison of a \
                         machine with every identical machine in the index — whether there are \
                         enough of them, and how spread they are — so a value for some machines \
                         would be a different calculation over the same cohort. Set it in \
                         [thresholds] itself"
                    );
                }
                if let Err(why) = f.check(value) {
                    bail!("{at} {name} {why}");
                }
                set.insert(f.name, value);
            }
            if set.is_empty() {
                bail!("{at} sets no thresholds, so it would change nothing");
            }
            let name = o.name.clone().unwrap_or(derived);
            if let Some(same) = overrides.iter().position(|p| p.name == name) {
                bail!(
                    "thresholds.overrides[{same}] and [{i}] are both named {name:?}. Names are what \
                     a machine's page shows as the reason it is judged differently, so they must \
                     tell the rules apart{}",
                    if o.name.is_none() {
                        " — two rules that pick the same machines are better merged into one"
                    } else {
                        ""
                    }
                );
            }
            overrides.push(Override {
                name,
                tags,
                system_model: o.system_model.clone(),
                cpu_model: o.cpu_model.clone(),
                set,
            });
        }

        // Critical has to stay at or below nearly full for every machine, and
        // a machine can take the two from different places: one rule's
        // critical line and a later rule's, or the fleet's, nearly-full line.
        // So every pairing some machine could end up with is checked. A rule
        // that sets a line keeps any later source of that line away from the
        // machines it picks, and two rules that ask for different values of
        // the same tag or model never pick the same machine. Other rules in
        // between are not considered, so this can refuse a pairing that one of
        // them would in fact hide; it never lets an impossible line through.
        let sets =
            |o: Option<usize>, name: &str| o.is_some_and(|i| overrides[i].set.contains_key(name));
        let line = |o: Option<usize>, name: &str, fleet_value: f64| match o {
            Some(i) => overrides[i].set.get(name).copied(),
            None => Some(fleet_value),
        };
        let sources: Vec<Option<usize>> = std::iter::once(None)
            .chain((0..overrides.len()).map(Some))
            .collect();
        for &a in &sources {
            let Some(crit) = line(a, "disk_free_critical_pct", fleet.disk_free_critical_pct) else {
                continue;
            };
            for &b in &sources {
                let Some(free) = line(b, "disk_free_pct", fleet.disk_free_pct) else {
                    continue;
                };
                let together = match (a, b) {
                    (None, None) => false, // checked above, with a plainer message
                    (None, Some(_)) => !sets(b, "disk_free_critical_pct"),
                    (Some(_), None) => !sets(a, "disk_free_pct"),
                    (Some(i), Some(j)) if i == j => true,
                    (Some(i), Some(j)) => {
                        !overrides[i].disjoint(&overrides[j])
                            && (!sets(b, "disk_free_critical_pct") || i < j)
                            && (!sets(a, "disk_free_pct") || j < i)
                    }
                };
                if together && crit > free {
                    let whose = |o: Option<usize>| match o {
                        Some(i) => format!("the override {:?}", overrides[i].name),
                        None => "[thresholds]".to_string(),
                    };
                    bail!(
                        "disk_free_critical_pct {crit} from {} is above disk_free_pct {free} from \
                         {}, and a machine could be judged by both. Critical is the lower line, so \
                         with these values every nearly full disk on it would be critical and none \
                         a warning. Set both in the override, or lower its critical line",
                        whose(a),
                        whose(b)
                    );
                }
            }
        }

        Ok(Tuning { fleet, overrides })
    }
}

/// Outbound notifications: one JSON POST to a URL the operator chooses. Off
/// unless `webhook_url` is set, and then the only connection this process
/// opens that nobody asked it to.
///
/// Generic on purpose. A receiver that wants a Teams card or a Slack message
/// builds it from the payload; this end stays one documented shape rather than
/// one per vendor.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    /// Where to POST. Unset switches notifications off entirely.
    ///
    /// `https://`, because the payload names machines and says what is wrong
    /// with them — the same reason the dashboard refuses to serve the estate
    /// in the clear. `http://` is accepted only for a receiver on this host
    /// (`localhost` or a loopback address), where nothing crosses a wire.
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Signs every request: `X-Loadbearer-Signature: sha256=<hex>`, the
    /// HMAC-SHA256 of the exact body with this as the key, so the receiver
    /// can tell a request came from here. Empty sends no signature.
    #[serde(default)]
    pub secret: String,
    /// Send as soon as a scan finds a new critical finding nobody has
    /// quieted.
    #[serde(default = "yes")]
    pub critical: bool,
    /// `off`, `daily` or `weekly` (Mondays): a summary of the fleet at
    /// `digest_at`.
    #[serde(default)]
    pub digest: DigestSchedule,
    /// When the digest goes, `HH:MM` in **UTC** — not the server's local time.
    #[serde(default = "digest_at")]
    pub digest_at: String,
}

fn yes() -> bool {
    true
}

fn digest_at() -> String {
    "08:00".to_string()
}

impl Default for Notify {
    fn default() -> Self {
        Self {
            webhook_url: None,
            secret: String::new(),
            critical: yes(),
            digest: DigestSchedule::default(),
            digest_at: digest_at(),
        }
    }
}

/// Written by hand so the secret never reaches a log line or a panic message
/// through `{:?}`. The URL is left out too: a webhook URL often carries a
/// token of its own in the path or the query.
impl std::fmt::Debug for Notify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notify")
            .field(
                "webhook_url",
                &self.webhook_url.as_ref().map(|_| "<redacted>"),
            )
            .field("secret", &(!self.secret.is_empty()).then_some("<redacted>"))
            .field("critical", &self.critical)
            .field("digest", &self.digest)
            .field("digest_at", &self.digest_at)
            .finish()
    }
}

impl Notify {
    /// `digest_at` as a time of day. Only meaningful on a validated config,
    /// where it is known to parse.
    pub fn digest_time(&self) -> Option<time::Time> {
        time::Time::parse(
            self.digest_at.trim(),
            format_description!("[hour]:[minute]"),
        )
        .ok()
    }
}

/// How often the digest goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DigestSchedule {
    #[default]
    Off,
    Daily,
    /// Mondays.
    Weekly,
}

impl DigestSchedule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }
}

/// A webhook URL this will send to, or the reason it will not.
///
/// The rule is the dashboard's own: nothing about the estate goes over the
/// network in the clear. Loopback is the exception because there is no
/// network — a relay on the same host, or a test.
pub fn webhook_url(raw: &str) -> Result<openidconnect::reqwest::Url> {
    use openidconnect::url::Host;
    let url = openidconnect::reqwest::Url::parse(raw.trim())
        .map_err(|e| anyhow::anyhow!("notify.webhook_url is not a URL ({e})"))?;
    let loopback = match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => bail!("notify.webhook_url has no host"),
    };
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        "http" => bail!(
            "notify.webhook_url must be https: the payload carries hostnames and findings, and \
             plain http would put them on the wire in the clear. http is accepted only for a \
             receiver on this host (localhost or a loopback address)"
        ),
        other => bail!("notify.webhook_url must be an https:// URL, not {other}://"),
    }
}

/// Machines submitting their own results, without a person and without a share.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ingest {
    #[serde(default)]
    pub tokens: Vec<IngestToken>,
}

/// One credential a deployment tool can post a result with.
///
/// **A token may write and may not read.** It is accepted on the upload
/// endpoint and nowhere else, so a credential sitting in a deployment script
/// cannot be used to enumerate the estate — which is the difference between
/// leaking a write path and leaking the fleet.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IngestToken {
    /// What this token is for, in the log and against every run it submits.
    /// Names are unique, so revoking one is unambiguous.
    pub name: String,
    /// The secret itself. Prefer `token_sha256`: exactly one of the two.
    #[serde(default)]
    pub token: Option<String>,
    /// SHA-256 of the secret, in hex — what `loadbearer-fleet token new`
    /// prints. The server only ever needs to recognise a token, never to
    /// produce one, so it need not hold it: a copy of this file, a backup or a
    /// support bundle then leaks nothing anyone can submit with. A plain
    /// digest rather than a slow one is enough, because the secret is 256
    /// random bits rather than something a person chose.
    #[serde(default)]
    pub token_sha256: Option<String>,
    /// Optional, and it means what the same key means on a grant: this token
    /// may only submit results carrying these tags. Absent means the whole
    /// fleet.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// Submissions a minute for this token, in place of
    /// `server.upload_limit_per_minute`. The limit is per credential, and one
    /// token per site is one bucket for every machine there — so a site that
    /// runs at 09:00 wants more than a person clicking Add results does.
    #[serde(default)]
    pub limit_per_minute: Option<u32>,
    /// The last day, UTC, this token is accepted: `expires = 2027-06-30`.
    /// Rotation that is scheduled happens; rotation that is remembered does
    /// not.
    #[serde(default)]
    pub expires: Option<toml::value::Datetime>,
}

impl IngestToken {
    /// The digest a presented secret is compared against. Only meaningful on
    /// a validated entry, where exactly one of the two forms is set and the
    /// hex is well formed.
    pub fn digest(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        if let Some(hex) = &self.token_sha256 {
            let mut out = [0u8; 32];
            for (i, byte) in out.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
            }
            return out;
        }
        Sha256::digest(self.token.as_deref().unwrap_or_default().as_bytes()).into()
    }

    /// The `expires` date, if there is one and it is a date.
    pub fn expires_on(&self) -> Option<time::Date> {
        let d = self.expires.as_ref()?.date?;
        let month = time::Month::try_from(d.month).ok()?;
        time::Date::from_calendar_date(i32::from(d.year), month, d.day).ok()
    }
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
    /// Where acknowledgements, snoozes and suppressions are kept: the state
    /// file. Absent means beside the index, named after it —
    /// `fleet-index.db` keeps its state in `fleet-index.state.db`.
    ///
    /// **Not derived, and not safe to delete.** Everything else this tool
    /// holds can be rebuilt from the collection folder; what people have said
    /// about the findings cannot, which is exactly why it is not in the index.
    /// `backup` copies it alongside the index snapshot.
    #[serde(default)]
    pub state: Option<PathBuf>,
    /// Keep a copy of every result document this indexes, content-addressed.
    ///
    /// Unset by default, because it is only worth its disk when the collection
    /// folder doesn't already hold a file per run. When each machine overwrites
    /// a single file, the folder holds only the latest result and the index
    /// becomes the sole record of the runs before it — this puts that record
    /// back into files, so the index is genuinely derived again and moving to
    /// another server is a copy rather than a migration.
    ///
    /// About 8 KB per run: gzipped `loadbearer.result/1` documents exactly as
    /// they arrived, named by their SHA-256, so the same document is never
    /// stored twice and a backup tool sees immutable files it need only copy
    /// once.
    #[serde(default)]
    pub archive_dir: Option<PathBuf>,
    /// Where results submitted through the dashboard are written.
    ///
    /// Unset switches uploading off entirely, whatever anybody's role says —
    /// a server that has not been given somewhere to put a result cannot be
    /// persuaded to accept one.
    ///
    /// **It has to sit inside the collection folder**, and is refused at load
    /// if it does not. The whole design rests on that folder being the source
    /// of truth and the index being derived from it, so an upload stored
    /// anywhere else would exist only in the index — and the rescan this
    /// documentation calls safe would silently destroy it. Inside the folder,
    /// an uploaded run is an ordinary result file that any rebuild finds
    /// again.
    ///
    /// Files are named by the SHA-256 of their contents, never by anything the
    /// uploader sent.
    #[serde(default)]
    pub upload_dir: Option<PathBuf>,
    /// The most results one credential may submit per minute. Zero is no
    /// limit.
    ///
    /// A safety valve rather than a defence: what it stops is a script
    /// retrying on a timer somebody misread, or a leaked token filling a disk
    /// 40 KB at a time. It is counted per principal — a person's subject or a
    /// token's name — because behind a reverse proxy every request arrives
    /// from the proxy, so an address-keyed limit would be one bucket for the
    /// whole world.
    ///
    /// Idle time banks up to a minute's worth, so a rollout that submits for
    /// two thousand machines at nine o'clock gets the first minute's worth at
    /// once and the rest at the sustained rate, each told when to come back.
    /// Resending is safe: ingest is idempotent by content.
    #[serde(default = "upload_limit_per_minute")]
    pub upload_limit_per_minute: u32,
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

fn upload_limit_per_minute() -> u32 {
    120
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

impl Log {
    /// The file this run will actually write, date and all.
    ///
    /// `file` is a *stem*: the rotation appends the UTC date, so the path in
    /// the configuration never exists as a file and `tail -f` on it fails with
    /// `No such file or directory`. Anything that tells somebody where the log
    /// is should tell them this instead of the stem.
    pub fn current_file(&self) -> Option<PathBuf> {
        let path = self.file.as_ref()?;
        let stamp = OffsetDateTime::now_utc()
            .format(format_description!("[year]-[month]-[day]"))
            .ok()?;
        let mut name = path.file_name()?.to_os_string();
        name.push(format!(".{stamp}"));
        Some(path.with_file_name(name))
    }
}

impl Server {
    /// The state file this configuration means: `state` if set, otherwise
    /// beside the index with `.state` before its extension.
    ///
    /// Worked out from the index path *as finally resolved* — after
    /// `--index` has overridden the file — rather than defaulted when the
    /// config is read, so `--index dev.db` keeps its state in `dev.state.db`
    /// and never in somebody else's.
    pub fn state_path(&self) -> PathBuf {
        match &self.state {
            Some(p) => p.clone(),
            None => crate::state::sibling(&self.index),
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
            state: None,
            archive_dir: None,
            upload_dir: None,
            upload_limit_per_minute: upload_limit_per_minute(),
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
    /// registration, not from a scope. A self-hosted provider usually does:
    /// Authelia, for one, emits `groups` only when the `groups` scope is asked
    /// for.
    #[serde(default)]
    pub extra_scopes: Vec<String>,
    /// A PEM file of extra certificate authorities to trust when talking to
    /// the identity provider.
    ///
    /// Only needed for a provider whose certificate is signed by a CA that
    /// isn't publicly trusted — a self-hosted one behind an internal CA, most
    /// often. The HTTP client here trusts a **built-in** root set and does not
    /// read the operating system's trust store, so installing a CA on the
    /// machine has no effect and this is the only way to add one.
    ///
    /// Added to the built-in roots rather than replacing them, so a provider
    /// that later moves to a publicly-trusted certificate keeps working.
    ///
    /// `[notify]`'s webhook trusts it too. A receiver is as likely as a
    /// provider to sit behind the same internal CA, and with no trust store
    /// to fall back on there would otherwise be no way to reach one.
    #[serde(default)]
    pub ca_bundle: Option<PathBuf>,
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
            ca_bundle: None,
            grants: Vec::new(),
        }
    }
}

/// "Members of this group get this role, over this much of the estate."
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// Whatever the provider actually puts in the groups claim, matched
    /// exactly.
    ///
    /// Which is not the same thing on every provider, and guessing wrong is
    /// the usual reason a sign-in succeeds and lands no role. **Entra** emits
    /// group **object IDs**, so use those — display names there are
    /// renameable and not unique. **Authelia, Keycloak and Authentik** emit
    /// group **names**, so use the name as written in their user directory.
    ///
    /// If in doubt, sign in and read the refusal: it counts the groups it
    /// decoded, so `0 group(s)` means the claim never reached the ID token,
    /// and any other number means the claim is fine and these values don't
    /// match it.
    pub group: String,
    pub role: Role,
    /// Restrict this grant to machines carrying all of these tags. Absent means
    /// the whole fleet.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
/// **The order of these variants is the order of privilege**, because
/// `resolve` takes the `max()` of a caller's matching grants. Sorting them
/// alphabetically, or inserting one in the wrong place, silently changes who
/// is allowed what — which is why a test pins the order rather than trusting
/// it to be obvious.
pub enum Role {
    /// Read the dashboard.
    Viewer,
    /// Read the dashboard, submit results to it, and acknowledge or snooze
    /// findings on the machines in scope.
    Contributor,
    /// All of that, trigger a rescan, and create or remove suppression rules.
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Contributor => "contributor",
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
        self.server.state = self.server.state.as_deref().map(|p| beside(base, p));
        // A UNC share is already absolute, so the usual case is untouched.
        self.server.collection_dir = self
            .server
            .collection_dir
            .as_deref()
            .map(|p| beside(base, p));
        self.server.archive_dir = self.server.archive_dir.as_deref().map(|p| beside(base, p));
        self.server.upload_dir = self.server.upload_dir.as_deref().map(|p| beside(base, p));
        self.log.file = self.log.file.as_deref().map(|p| beside(base, p));
        self.auth.ca_bundle = self.auth.ca_bundle.as_deref().map(|p| beside(base, p));
    }

    /// Is the dashboard reachable over a channel that hides a session cookie?
    pub fn public_url_is_https(&self) -> bool {
        self.server.public_url.starts_with("https://")
    }

    /// The thresholds the analysis runs with, as every entry point builds
    /// them — `serve`, the Windows service and `report` — so the server and
    /// the command line judge the fleet by the same lines. Warns, once per
    /// start, about an override that can never change anything.
    pub fn tuning(&self) -> Result<crate::analytics::Tuning> {
        let tuning = self.thresholds.tuning()?;
        for warning in tuning.shadowed() {
            tracing::warn!("{warning}");
        }
        Ok(tuning)
    }

    pub fn redirect_url(&self) -> String {
        format!(
            "{}/auth/callback",
            self.server.public_url.trim_end_matches('/')
        )
    }

    pub(crate) fn validate(&self) -> Result<()> {
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
        // Paths, checked before the `auth.mode = "none"` return below, because
        // they matter whether or not anybody signs in. A placeholder left in
        // one of these doesn't fail loudly: `log.file` would create a
        // *directory* named `PUT-THE-PATH-FOR-THE-LOG-FILE-HERE`, and an
        // unreachable collection folder is reported and survived. Both look
        // like a configuration that worked.
        check_placeholder_path(
            "server.collection_dir",
            self.server.collection_dir.as_deref(),
        )?;
        check_placeholder_path("server.archive_dir", self.server.archive_dir.as_deref())?;
        check_placeholder_path("server.upload_dir", self.server.upload_dir.as_deref())?;
        check_placeholder_path("server.state", self.server.state.as_deref())?;
        // The state file opens the index and refuses it anyway, but by then
        // the message is about a database; here it can be about the setting.
        if self.server.state.as_deref() == Some(self.server.index.as_path()) {
            bail!(
                "server.state and server.index are the same file ({}). The index is derived and \
                 may be rebuilt; the state file holds what people have recorded and must not be, \
                 so the two cannot share a file.",
                self.server.index.display()
            );
        }
        check_placeholder_path("log.file", self.log.file.as_deref())?;
        check_placeholder_path("auth.ca_bundle", self.auth.ca_bundle.as_deref())?;
        self.validate_notify()?;
        self.thresholds.tuning()?;

        // An upload has to become a file the next scan will find, or it lives
        // only in the index — and this documentation tells people the index is
        // safe to delete. Refusing here is the difference between a rule and a
        // hope.
        if let Some(upload) = &self.server.upload_dir {
            // Before the comparison below, which is on the paths as written.
            // `Path::starts_with` is component-wise, so
            // `/srv/collection/../../etc` does begin with `/srv/collection`
            // and would pass a rule this file documents as containment.
            // Refusing `..` outright is the honest version: the alternative is
            // resolving a path that need not exist yet.
            for (field, path) in [
                ("server.upload_dir", Some(upload)),
                ("server.collection_dir", self.server.collection_dir.as_ref()),
            ] {
                if let Some(path) = path
                    && path
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    bail!(
                        "{field} ({}) contains `..`. Uploading requires the upload directory \
                         to be inside the collection folder, and that is checked on the path \
                         as written — a path that climbs back out would satisfy the check and \
                         not the rule. Write the destination out in full.",
                        path.display()
                    );
                }
            }
            match &self.server.collection_dir {
                None => bail!(
                    "server.upload_dir is set to {} but there is no server.collection_dir. An \
                     uploaded result has to land in the folder that gets scanned, or it exists \
                     only in the index and a rebuild loses it.",
                    upload.display()
                ),
                Some(collection) if !upload.starts_with(collection) => bail!(
                    "server.upload_dir ({}) is outside server.collection_dir ({}). An uploaded \
                     result has to land in the folder that gets scanned, or it exists only in \
                     the index and a rebuild loses it — put it inside, for example {}.",
                    upload.display(),
                    collection.display(),
                    collection.join("uploaded").display()
                ),
                Some(_) => {}
            }
        }

        // Ingest tokens are bearer credentials reachable from every machine in
        // the estate, so the weak ones are refused rather than warned about.
        let mut named: Vec<&str> = Vec::new();
        for t in &self.ingest.tokens {
            if t.name.trim().is_empty() {
                bail!("an [[ingest.tokens]] entry has no name; the name is what you revoke");
            }
            match (&t.token, &t.token_sha256) {
                (Some(_), Some(_)) => bail!(
                    "the ingest token {:?} sets both token and token_sha256; keep token_sha256 \
                     and delete the secret, which is the point of having the digest",
                    t.name
                ),
                (None, None) => bail!(
                    "the ingest token {:?} has neither token nor token_sha256; \
                     `loadbearer-fleet token new --name {}` prints an entry to paste",
                    t.name,
                    t.name
                ),
                (Some(secret), None) => {
                    check_placeholder(&format!("ingest.tokens.{}.token", t.name), secret)?;
                    if secret.len() < MIN_INGEST_TOKEN_LEN {
                        bail!(
                            "the ingest token {:?} is {} characters; {} is the minimum. It is a \
                             bearer credential that lets a machine write to the fleet, so \
                             generate one rather than choosing it: loadbearer-fleet token new",
                            t.name,
                            secret.len(),
                            MIN_INGEST_TOKEN_LEN
                        );
                    }
                }
                (None, Some(hex)) => {
                    check_placeholder(&format!("ingest.tokens.{}.token_sha256", t.name), hex)?;
                    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                        bail!(
                            "the ingest token {:?} has a token_sha256 that is not 64 hex \
                             characters. It is the digest `loadbearer-fleet token new` prints, \
                             not the secret itself",
                            t.name
                        );
                    }
                }
            }
            if let Some(expires) = &t.expires
                && t.expires_on().is_none_or(|_| expires.time.is_some())
            {
                bail!(
                    "the ingest token {:?} has expires = {expires}; it wants a plain date, the \
                     last day the token is accepted, such as expires = 2027-06-30",
                    t.name
                );
            }
            if named.contains(&t.name.as_str()) {
                bail!(
                    "two [[ingest.tokens]] entries are both named {:?}; revoking one would be \
                     ambiguous, and so would the provenance on everything they submit",
                    t.name
                );
            }
            named.push(&t.name);
        }
        if !self.ingest.tokens.is_empty() && self.server.upload_dir.is_none() {
            bail!(
                "{} ingest token(s) are configured but there is no server.upload_dir, so nothing \
                 can be submitted. Set one inside the collection folder, or remove the tokens.",
                self.ingest.tokens.len()
            );
        }

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

    /// `[notify]`, checked whether or not anybody signs in: it decides what
    /// leaves this host, which matters either way.
    fn validate_notify(&self) -> Result<()> {
        let n = &self.notify;
        if n.digest_time().is_none() {
            bail!(
                "notify.digest_at is {:?}; it wants a time of day as HH:MM, in UTC, such as \
                 \"08:00\"",
                n.digest_at
            );
        }
        check_placeholder("notify.secret", &n.secret)?;
        let Some(url) = &n.webhook_url else {
            // Settings with nowhere to send would look like they applied.
            if n.digest != DigestSchedule::Off {
                bail!(
                    "notify.digest is \"{}\" but there is no notify.webhook_url, so there is \
                     nowhere to send it. Set the URL, or set digest = \"off\"",
                    n.digest.as_str()
                );
            }
            if !n.secret.is_empty() {
                bail!(
                    "notify.secret is set but there is no notify.webhook_url, so nothing would \
                     ever be signed with it. Set the URL, or remove the secret"
                );
            }
            return Ok(());
        };
        if url.trim().is_empty() {
            bail!(
                "notify.webhook_url is empty. Remove the line to switch notifications off, or \
                 set the receiver's https:// URL"
            );
        }
        // Not the value itself in the message: a webhook URL often carries a
        // token, and this error is printed and logged.
        if url.contains(PLACEHOLDER) {
            bail!(
                "notify.webhook_url still has the starter placeholder in it. Fill it in with the \
                 receiver's https:// URL, or remove the line to switch notifications off"
            );
        }
        webhook_url(url)?;
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

# Optional. Where acknowledgements, snoozes and suppressions of findings are
# kept. Unset, it sits beside the index, named after it: fleet-index.db keeps
# its state in fleet-index.state.db.
#
# Unlike the index this is NOT derived and NOT safe to delete: it is what
# people have recorded about the findings, and nothing in the collection
# folder can bring it back. `backup` copies it beside the index snapshot; back
# it up with whatever backs up this server. A version this release does not
# understand is refused at startup rather than set aside or recreated.
#
# state = 'PUT-THE-PATH-FOR-THE-STATE-FILE-HERE'

# Optional. Keeps a gzipped copy of every result document this indexes, named
# by its SHA-256, at about 8 KB per run.
#
# Worth setting when your collector overwrites one file per machine, because
# then the collection folder holds only the latest result and the index is the
# only record of everything earlier. With this set, that record is in files
# again: the index goes back to being disposable, backups see immutable files,
# and moving to another server is a copy rather than a migration. Leave it
# unset if your collector already writes a file per run - it would only be
# storing a second copy of what you already keep.
#
# archive_dir = 'PUT-THE-PATH-FOR-THE-DOCUMENT-ARCHIVE-HERE'

# Optional, and off unless set: where results submitted through the dashboard
# are written. Without it nobody can upload, whatever their role says.
#
# It must sit *inside* collection_dir, and is refused at load if it does not.
# An uploaded result has to become an ordinary file in the folder that gets
# scanned - anywhere else and it exists only in the index, so the rebuild this
# documentation calls safe would quietly destroy it. Files are named by the
# SHA-256 of their contents, never by anything the uploader sent.
#
# Uploading also needs a grant with role = "contributor" or "admin", and the
# service account needs write access to this directory.
#
# upload_dir = 'PUT-THE-PATH-FOR-UPLOADED-RESULTS-HERE'

# The most results one credential may submit per minute. Zero is no limit.
#
# A safety valve rather than a defence: it stops a script retrying on a timer
# somebody misread, or a leaked token filling the disk 40 KB at a time. Counted
# per person or per token, never per address - behind a reverse proxy every
# request arrives from the proxy, so an address is either everybody or whatever
# the caller claims.
#
# Idle time banks up to a minute's worth, so a rollout for two thousand
# machines at nine o'clock gets the first minute's worth at once and the rest
# at the sustained rate, each refusal saying when to come back. Retrying is
# safe: ingest is idempotent by content, so a document that did land is
# answered as already indexed rather than stored twice.
upload_limit_per_minute = 120

# How often to re-read the collection folder by itself. This is what makes it a
# service rather than a command: a dashboard that only refreshes when somebody
# happens to click is out of date exactly when nobody is looking at it. Zero
# switches the timer off and leaves rescanning to the button. Raise it if the
# share is large - every scan re-reads and re-hashes every document, which is
# what makes indexing idempotent by content.
scan_interval_minutes = 15

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

# Scopes beyond "openid profile email". Entra needs none of these for group
# claims - those come from the token configuration on the app registration
# rather than from a scope - so leave it empty unless your provider says
# otherwise.
#
# A self-hosted provider usually does need one. Authelia emits `groups` only
# when the groups scope is asked for:  extra_scopes = ["groups"]
extra_scopes = []

# Extra certificate authorities to trust when contacting the provider, as a PEM
# file. Only needed where the provider's certificate is signed by a CA that is
# not publicly trusted - a self-hosted one behind an internal CA, usually.
#
# This trusts a built-in root set and does not read the machine's trust store,
# so installing the CA on the server has no effect and this is the only way to
# add it. Added to the built-in roots, not instead of them. The [notify]
# webhook below trusts it too, for a receiver behind the same internal CA.
#
# ca_bundle = 'PUT-THE-PATH-TO-YOUR-CA-BUNDLE-HERE'

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

[notify]
# Off unless webhook_url is set, and then one JSON POST to that URL when a scan
# finds a new critical finding, and a digest if you ask for one. Nothing else
# leaves this host. Generic on purpose: no Teams or Slack card format - point
# it at whatever relays to those. The payload is in the README.
#
# It carries hostnames, machine keys and the text of findings, for the whole
# fleet regardless of anybody's scope, so it must be https. http is accepted
# only for a receiver on this host (localhost or a loopback address).
#
# webhook_url = "https://PUT-THE-URL-OF-YOUR-WEBHOOK-RECEIVER-HERE"

# Optional. Signs every request with X-Loadbearer-Signature: sha256=<hex>,
# the HMAC-SHA256 of the body with this as the key, so the receiver can tell
# it came from here. A secret: treat this file as you would any other that
# holds one. Generate something long and random rather than choosing it.
secret = ""

# Send as soon as a scan finds a critical finding that was not critical before
# and that nobody has acknowledged, snoozed or suppressed.
critical = true

# A summary of the fleet: "off", "daily", or "weekly" (Mondays), at digest_at.
# digest_at is HH:MM in UTC, not the server's local time. If the server is not
# running at that minute the digest is skipped, not sent late.
digest = "off"
digest_at = "08:00"

[thresholds]
# The lines the findings are drawn against. Every one is optional: unset, it
# keeps the default shown here, so a file that leaves this section alone judges
# the fleet exactly as the README describes. Changing one re-judges the past as
# well as the present - the history is reconstructed with today's lines - and
# a finding a change makes go away counts as resolved, so an acknowledgement or
# snooze of it is spent.
#
# The cohort comparison - a machine against every identical machine measured
# the same way. Fleet-wide only: an override below cannot set these four.
# min_cohort = 4                # machines a cohort needs to be compared against; at least 3
# cohort_z = 3.5                # modified z-score at which a member is an outlier
# cohort_min_shortfall = 0.10   # a shortfall smaller than this is never named
# mad_floor_frac = 0.03         # the spread is floored at this fraction of the median
#
# One machine against a line. These can also be set for some machines below.
# regression_frac = 0.15        # a drop against its own earlier runs that is a regression
# stale_days = 90               # after this, a result is history rather than inventory
# battery_health_pct = 70       # capacity, % of design, below which the battery is worn
# unstable_cpu_subtests = 2     # low-confidence CPU subtests at which a run is too shaky to read
# drive_wear_pct = 80           # rated endurance used, %, at which a drive is worn
# drive_spare_pct = 20          # spare blocks left, %, below which a drive is worn
# disk_free_pct = 10            # free space on the fullest volume, %, below which it is nearly full
# disk_free_critical_pct = 5    # ... and below which that is critical; not above disk_free_pct
# firmware_behind_days = 90     # how far behind the newest firmware on its model before it is named
# firmware_old_years = 4        # firmware older than this when measured is named anyway
# replace_age_years = 5         # from this age a machine is old, for the lifecycle verdict

# Different lines for some machines. A rule picks machines by tags - every one
# given must be on the machine, as on a grant - and by system_model and
# cpu_model, exactly as the machine reports them (the Groups view lists them).
# Everything given must match, and a rule must give at least one of them.
#
# For each threshold, a machine takes the value from the first rule in this
# file that picks it and sets that threshold; otherwise the value above;
# otherwise the default. First match wins, threshold by threshold, so a later
# rule still supplies what an earlier one leaves alone. Each machine's page
# names the rules that changed how it was judged.
#
# [[thresholds.overrides]]
# name = "lab machines"         # optional: what the machine page shows
# tags = { site = "lab" }
# stale_days = 365              # measured once a year, on purpose
#
# [[thresholds.overrides]]
# name = "kiosks"
# system_model = "PUT-THE-SYSTEM-MODEL-OF-YOUR-KIOSKS-HERE"
# disk_free_pct = 5             # a small fixed image, mostly full by design
# disk_free_critical_pct = 2

# Who gets in, and over how much of the estate.
#
# `group` is matched exactly against whatever the provider puts in the claim,
# and that differs by provider:
#
#   Entra ID                       the group's object ID. Display names there
#                                  are renameable and not unique, and the token
#                                  carries object IDs anyway.
#   Authelia / Keycloak /          the group name, as written in the user
#   Authentik                      directory.
#
# Guessing wrong is the usual reason a sign-in succeeds and lands no role. If
# it happens, read the refusal: it counts the groups it decoded, so "0 group(s)"
# means the claim never reached the ID token and any other number means the
# claim is fine and these values do not match it.
#
# The most privileged matching grant wins. Scopes are the union of the matching
# grants at that role, so somebody in two site groups sees both sites.

[[auth.grants]]
group = "PUT-THE-GROUP-YOUR-FLEET-ADMINS-ARE-IN-HERE"
role = "admin"    # read the dashboard, trigger a rescan, suppress findings

[[auth.grants]]
group = "PUT-THE-GROUP-YOUR-FLEET-VIEWERS-ARE-IN-HERE"
role = "viewer"   # read the dashboard

# The three roles nest: a contributor does everything a viewer does and may
# also submit results and acknowledge or snooze findings on the machines in
# its scope; an admin does everything a contributor does and may also trigger
# a rescan and create or remove suppression rules.
#
# [[auth.grants]]
# group = "PUT-THE-GROUP-THAT-MAY-SUBMIT-RESULTS-HERE"
# role = "contributor"

# A team that should only see its own site. Tags come from loadbearer's --tag,
# so this only means anything if your deployment tool sets them.
#
# [[auth.grants]]
# group = "PUT-THE-OBJECT-ID-OF-THE-GLASGOW-DESKTOP-TEAM-HERE"
# role = "viewer"
# tags = { site = "glasgow" }

# Machines that submit their own results, with no person and no share.
#
# A token is a bearer credential: whatever runs loadbearer sends it as
# "Authorization: Bearer <token>" to POST /api/upload. It may write and may not
# read - it is accepted on that endpoint and nowhere else - so a credential
# sitting in a deployment script cannot be used to enumerate the estate.
#
# Generate one, do not invent one:
#
#   loadbearer-fleet token new --name site-glasgow --tag site=glasgow
#
# prints the secret once, for the machines, and an entry for this file that
# holds only its SHA-256. The server never needs the secret itself, so this
# file, and every backup of it, leaks nothing anyone could submit with.
# A plain `token = "..."` still works, but anything shorter than 32 characters
# is refused at startup.
#
# Needs upload_dir set above: tokens with nowhere to put a result are refused
# at startup rather than at the first submission. To revoke, delete the entry
# and restart. The name is what appears against everything it submitted, so it
# is what you go looking for afterwards.
#
# [[ingest.tokens]]
# name = "deployment-tool"
# token_sha256 = "PUT-THE-DIGEST-FROM-TOKEN-NEW-HERE"
#
# Optional, and it means what the same key means on a grant: this token may
# only submit results carrying these tags, and loadbearer has to be run with
# the matching --tag. Absent means the whole fleet.
#
# tags = { site = "glasgow" }
#
# Optional. Submissions a minute for this token, in place of
# upload_limit_per_minute above. One token per site is one bucket for every
# machine at that site, so raise it to roughly the number that finish in the
# same minute, or spread them with loadbearer's --jitter.
#
# limit_per_minute = 600
#
# Optional. The last day, UTC, the token is accepted. Afterwards a submission
# is refused as expired, so rotation happens because it was scheduled.
#
# expires = 2027-06-30
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
            "{field} still has the starter placeholder in it ({value:?}). Fill it in — left as \
             it is, it fails much later and somewhere else, with an error that says nothing \
             about this file."
        );
    }
    Ok(())
}

/// The same check for the path settings, which are optional.
fn check_placeholder_path(field: &str, value: Option<&Path>) -> Result<()> {
    match value {
        Some(p) => check_placeholder(field, &p.to_string_lossy()),
        None => Ok(()),
    }
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

    /// The path placeholders are checked on the way past `auth.mode = "none"`,
    /// not after it. Sign-in is the last thing somebody configures, so the
    /// unauthenticated config is exactly the one that reaches a placeholder
    /// path — and left in, `log.file` creates a directory with that name and
    /// then logs happily into it.
    #[test]
    fn placeholder_paths_are_refused_without_sign_in_configured() {
        for (field, mut c) in [
            (
                "log.file",
                Config {
                    log: Log {
                        file: Some(PathBuf::from("PUT-THE-PATH-FOR-THE-LOG-FILE-HERE")),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            (
                "server.collection_dir",
                Config {
                    server: Server {
                        collection_dir: Some(PathBuf::from(
                            "PUT-THE-PATH-TO-YOUR-RESULTS-SHARE-HERE",
                        )),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            (
                "server.archive_dir",
                Config {
                    server: Server {
                        archive_dir: Some(PathBuf::from(
                            "PUT-THE-PATH-FOR-THE-DOCUMENT-ARCHIVE-HERE",
                        )),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
        ] {
            c.auth.mode = AuthMode::None;
            let err = c
                .validate()
                .expect_err("a placeholder path must not start")
                .to_string();
            assert!(err.contains(field), "{field} not named in: {err}");
        }
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

    /// **The declaration order of `Role` is the order of privilege**, because
    /// `resolve` takes `max()` of the matching grants. Nothing in the type
    /// system says so, and nothing about reading the enum makes it obvious —
    /// somebody tidying it alphabetically would demote every admin to below a
    /// contributor and no other test would notice.
    #[test]
    fn the_role_order_is_the_privilege_order() {
        assert!(Role::Viewer < Role::Contributor);
        assert!(Role::Contributor < Role::Admin);

        // And the thing that order is *for*: two grants, the stronger wins.
        let auth = auth_with(vec![
            grant("readers", Role::Viewer, &[]),
            grant("submitters", Role::Contributor, &[]),
        ]);
        let e = auth
            .resolve(&["readers".into(), "submitters".into()])
            .expect("matches");
        assert_eq!(e.role, Role::Contributor);
    }

    /// The role names are a published interface: they are what somebody writes
    /// in `fleet.toml`, so renaming one breaks every deployment silently.
    #[test]
    fn role_names_round_trip_through_the_config_file() {
        for (text, role) in [
            ("viewer", Role::Viewer),
            ("contributor", Role::Contributor),
            ("admin", Role::Admin),
        ] {
            let parsed: Role = toml::from_str::<toml::Value>(&format!("r = \"{text}\""))
                .expect("toml")
                .get("r")
                .cloned()
                .expect("key")
                .try_into()
                .expect("a role");
            assert_eq!(parsed, role);
            assert_eq!(role.as_str(), text);
        }
    }

    /// An upload has to become a file the next scan will find. Anywhere else
    /// and it exists only in the index — which this project tells people is
    /// safe to delete — so the rule is enforced at load rather than hoped for.
    #[test]
    fn an_upload_directory_outside_the_collection_folder_is_refused() {
        let with = |collection: Option<&str>, upload: Option<&str>| Config {
            server: Server {
                collection_dir: collection.map(PathBuf::from),
                upload_dir: upload.map(PathBuf::from),
                ..Server::default()
            },
            ..Config::default()
        };

        assert!(
            with(Some("/srv/collection"), Some("/srv/collection/uploaded"))
                .validate()
                .is_ok(),
            "inside the collection folder is the whole point"
        );

        let err = with(Some("/srv/collection"), Some("/var/lib/lbf/uploads"))
            .validate()
            .expect_err("outside is refused")
            .to_string();
        assert!(err.contains("outside server.collection_dir"), "{err}");
        assert!(err.contains("a rebuild loses it"), "and says why: {err}");

        // With nowhere to scan at all, an upload directory cannot mean anything.
        let err = with(None, Some("/srv/uploads"))
            .validate()
            .expect_err("refused")
            .to_string();
        assert!(err.contains("no server.collection_dir"), "{err}");

        // `Path::starts_with` compares components, so a path that climbs back
        // out satisfies it: ["/", "srv", "collection", "..", "..", "etc"]
        // does begin with ["/", "srv", "collection"]. The rule is documented
        // as containment, so it has to mean containment.
        let err = with(
            Some("/srv/collection"),
            Some("/srv/collection/../../etc/cron.d"),
        )
        .validate()
        .expect_err("a path that climbs out is not inside")
        .to_string();
        assert!(err.contains(".."), "{err}");
    }

    /// A bearer credential every machine in the estate holds is worth being
    /// unfriendly about at load time, because every one of these fails later
    /// as something that looks like a different problem.
    #[test]
    fn a_weak_or_ambiguous_ingest_token_is_refused_at_load() {
        let with = |tokens: Vec<IngestToken>, upload: Option<&str>| Config {
            server: Server {
                collection_dir: Some(PathBuf::from("/srv/collection")),
                upload_dir: upload.map(PathBuf::from),
                ..Server::default()
            },
            ingest: Ingest { tokens },
            ..Config::default()
        };
        let tok = |name: &str, secret: &str| IngestToken {
            name: name.to_string(),
            token: Some(secret.to_string()),
            token_sha256: None,
            tags: BTreeMap::new(),
            limit_per_minute: None,
            expires: None,
        };
        let good = "0123456789abcdef0123456789abcdef";
        let uploads = Some("/srv/collection/uploaded");

        assert!(
            with(vec![tok("deployment-tool", good)], uploads)
                .validate()
                .is_ok()
        );

        let err = with(vec![tok("", good)], uploads)
            .validate()
            .expect_err("a token you cannot name is a token you cannot revoke")
            .to_string();
        assert!(err.contains("no name"), "{err}");

        let err = with(vec![tok("deployment-tool", "hunter2")], uploads)
            .validate()
            .expect_err("a chosen token is a guessable one")
            .to_string();
        assert!(err.contains("7 characters"), "{err}");
        assert!(
            err.contains("loadbearer-fleet token new"),
            "a length rule with no way to satisfy it gets worked around: {err}"
        );

        let err = with(
            vec![tok("deployment-tool", good), tok("deployment-tool", good)],
            uploads,
        )
        .validate()
        .expect_err("two tokens of one name")
        .to_string();
        assert!(err.contains("ambiguous"), "{err}");

        let err = with(
            vec![tok("deployment-tool", "PUT-A-GENERATED-TOKEN-HERE")],
            uploads,
        )
        .validate()
        .expect_err("the starter file's own placeholder")
        .to_string();
        assert!(err.contains("placeholder"), "{err}");

        // Tokens with nowhere to put a result: every submission would be a 400
        // about a setting the person holding the token cannot see.
        let err = with(vec![tok("deployment-tool", good)], None)
            .validate()
            .expect_err("refused")
            .to_string();
        assert!(err.contains("no server.upload_dir"), "{err}");
    }

    /// The digest form, as `token new` writes it, and the ways an entry can
    /// be half right.
    #[test]
    fn an_ingest_token_may_be_held_as_its_digest() {
        let entry = |extra: &str| {
            format!(
                "[server]\nbind = \"127.0.0.1:8787\"\npublic_url = \"http://localhost:8787\"\nindex = \"i.db\"\n\
                 collection_dir = \"/srv/c\"\nupload_dir = \"/srv/c/uploaded\"\n\
                 [[ingest.tokens]]\nname = \"site-glasgow\"\n{extra}"
            )
        };
        let load = |text: &str| -> Result<Config> {
            let config: Config = toml::from_str(text)?;
            config.validate()?;
            Ok(config)
        };
        let secret = "lbf_0123456789abcdef0123456789abcdef";
        let hex: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(secret.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };

        let by_digest = load(&entry(&format!(
            "token_sha256 = \"{hex}\"\nexpires = 2027-06-30\nlimit_per_minute = 600\n"
        )))
        .expect("the form token new prints");
        let by_secret = load(&entry(&format!("token = \"{secret}\"\n"))).expect("plaintext");
        let t = &by_digest.ingest.tokens[0];
        assert_eq!(
            t.digest(),
            by_secret.ingest.tokens[0].digest(),
            "the two forms of one secret are one token"
        );
        assert_eq!(
            by_digest.ingest.tokens[0].digest(),
            load(&entry(&format!(
                "token_sha256 = \"{}\"\n",
                hex.to_uppercase()
            )))
            .expect("upper-case hex")
            .ingest
            .tokens[0]
                .digest(),
        );
        assert_eq!(t.expires_on(), Some(time::macros::date!(2027 - 06 - 30)));
        assert_eq!(t.limit_per_minute, Some(600));

        for (case, (extra, want)) in [
            (
                format!("token = \"{secret}\"\ntoken_sha256 = \"{hex}\"\n"),
                "both",
            ),
            (String::new(), "neither"),
            ("token_sha256 = \"abc123\"\n".to_string(), "64 hex"),
            (format!("token_sha256 = \"{secret}\"\n"), "64 hex"),
            (
                format!("token_sha256 = \"{hex}\"\nexpires = 2027-06-30T12:00:00Z\n"),
                "plain date",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            // Cases by number, not by text: the text holds a secret, and an
            // assertion message is a log line.
            let err = load(&entry(&extra))
                .expect_err(&format!("case {case} refused"))
                .to_string();
            assert!(err.contains(want), "case {case}: {err}");
        }
    }

    fn notifying(url: &str) -> Config {
        let mut c = Config::default();
        c.notify.webhook_url = Some(url.to_string());
        c
    }

    /// The payload names machines and what is wrong with them, so it goes
    /// over https — except to a receiver on this host, where there is no wire.
    #[test]
    fn a_webhook_must_be_https_unless_it_is_on_this_host() {
        for ok in [
            "https://hooks.example.com/services/T000/B000/XXXX",
            "https://10.0.0.5:8443/hook?token=abc",
            "http://127.0.0.1:9000/hook",
            "http://127.8.9.10/hook",
            "http://localhost:9000/hook",
            "http://LOCALHOST/hook",
            "http://[::1]:9000/hook",
        ] {
            notifying(ok)
                .validate()
                .unwrap_or_else(|e| panic!("{ok} refused: {e:#}"));
        }
        for (bad, want) in [
            ("http://hooks.example.com/hook", "must be https"),
            ("http://10.0.0.5/hook", "must be https"),
            ("http://localhost.example.com/hook", "must be https"),
            ("ftp://hooks.example.com/hook", "https://"),
            ("hooks.example.com/hook", "not a URL"),
            ("", "empty"),
            (
                "https://PUT-THE-URL-OF-YOUR-WEBHOOK-RECEIVER-HERE",
                "placeholder",
            ),
        ] {
            let err = notifying(bad).validate().expect_err(bad).to_string();
            assert!(err.contains("notify.webhook_url"), "{bad}: {err}");
            assert!(err.contains(want), "{bad}: {err}");
        }
        // The value is not echoed: a webhook URL is often a credential.
        let err = notifying("http://hooks.example.com/hook?token=do-not-print")
            .validate()
            .expect_err("refused")
            .to_string();
        assert!(!err.contains("do-not-print"), "{err}");
    }

    #[test]
    fn notify_settings_without_a_webhook_are_refused_rather_than_ignored() {
        let mut c = Config::default();
        c.notify.digest = DigestSchedule::Daily;
        let err = c.validate().expect_err("nowhere to send").to_string();
        assert!(err.contains("notify.digest"), "{err}");

        let mut c = Config::default();
        c.notify.secret = "s3cret".into();
        let err = c.validate().expect_err("nothing to sign").to_string();
        assert!(err.contains("notify.secret"), "{err}");
        assert!(!err.contains("s3cret"), "the secret is not echoed: {err}");

        let mut c = notifying("https://hooks.example.com/x");
        for bad in ["8:00", "24:00", "08:00:00", "noon", ""] {
            c.notify.digest_at = bad.into();
            let err = c.validate().expect_err(bad).to_string();
            assert!(err.contains("HH:MM") && err.contains("UTC"), "{bad}: {err}");
        }
        c.notify.digest_at = "23:59".into();
        c.notify.digest = DigestSchedule::Weekly;
        c.validate()
            .expect("a weekly digest at a minute to midnight");
        assert_eq!(c.notify.digest_time(), Some(time::macros::time!(23:59)));
    }

    /// `{:?}` of the config must not carry the secret, or the URL that is
    /// often a secret too.
    #[test]
    fn the_notify_secret_does_not_reach_debug_output() {
        let mut c = notifying("https://hooks.example.com/x?token=url-token");
        c.notify.secret = "hmac-secret".into();
        let text = format!("{c:?}");
        assert!(!text.contains("hmac-secret"), "{text}");
        assert!(!text.contains("url-token"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn the_notify_section_parses_from_the_file() {
        let c: Config = toml::from_str(
            "[notify]\nwebhook_url = \"https://hooks.example.com/x\"\nsecret = \"s\"\n\
             critical = false\ndigest = \"weekly\"\ndigest_at = \"07:30\"\n",
        )
        .expect("parses");
        c.validate().expect("valid");
        assert!(!c.notify.critical);
        assert_eq!(c.notify.digest, DigestSchedule::Weekly);
        // Defaults: on for critical, no digest, 08:00.
        let d = Notify::default();
        assert!(d.critical);
        assert_eq!(d.digest, DigestSchedule::Off);
        assert_eq!(d.digest_at, "08:00");
        let err = toml::from_str::<Config>("[notify]\ndigest = \"hourly\"\n")
            .expect_err("not a schedule");
        assert!(err.to_string().contains("hourly"), "{err}");
    }

    /// `[thresholds]` and its overrides through the reader and `validate`, as
    /// `Config::load` takes them.
    fn thresholds_from(text: &str) -> Result<crate::analytics::Tuning> {
        let c: Config = toml::from_str(text)?;
        c.validate()?;
        c.thresholds.tuning()
    }

    fn refused(text: &str) -> String {
        format!(
            "{:#}",
            thresholds_from(text).expect_err(&format!("should be refused:\n{text}"))
        )
    }

    /// The doc comment on `Thresholds` promised a config change; a file that
    /// does not make one must change nothing.
    #[test]
    fn without_a_thresholds_section_the_engine_runs_on_the_defaults() {
        let t = thresholds_from("").expect("an empty file");
        assert_eq!(t, crate::analytics::Tuning::default());
        assert_eq!(t.fleet, crate::analytics::Thresholds::default());
        assert!(t.overrides.is_empty());
        assert_eq!(
            thresholds_from("[thresholds]\n").expect("an empty section"),
            crate::analytics::Tuning::default()
        );
        assert_eq!(
            Config::default().tuning().expect("default"),
            crate::analytics::Tuning::default()
        );
    }

    #[test]
    fn every_threshold_can_be_set_for_the_fleet() {
        let t = thresholds_from(
            "[thresholds]\nmin_cohort = 6\ncohort_z = 3\ncohort_min_shortfall = 0.12\n\
             mad_floor_frac = 0.05\nregression_frac = 0.2\nstale_days = 365\n\
             battery_health_pct = 60\nunstable_cpu_subtests = 3\ndrive_wear_pct = 90\n\
             drive_spare_pct = 15\ndisk_free_pct = 15\ndisk_free_critical_pct = 7.5\n\
             firmware_behind_days = 180\nfirmware_old_years = 5\nreplace_age_years = 6\n",
        )
        .expect("valid");
        let f = &t.fleet;
        assert_eq!(f.min_cohort, 6);
        assert_eq!(f.cohort_z, 3.0, "an integer is read as a number");
        assert_eq!(f.cohort_min_shortfall, 0.12);
        assert_eq!(f.mad_floor_frac, 0.05);
        assert_eq!(f.regression_frac, 0.2);
        assert_eq!(f.stale_days, 365.0);
        assert_eq!(f.battery_health_pct, 60.0);
        assert_eq!(f.unstable_cpu_subtests, 3);
        assert_eq!(f.drive_wear_pct, 90.0);
        assert_eq!(f.drive_spare_pct, 15.0);
        assert_eq!(f.disk_free_pct, 15.0);
        assert_eq!(f.disk_free_critical_pct, 7.5);
        assert_eq!(f.firmware_behind_days, 180.0);
        assert_eq!(f.firmware_old_years, 5.0);
        assert_eq!(f.replace_age_years, 6.0);
        // Every one of `FIELDS`, so a threshold added to the struct and the
        // table but not to the file is caught here.
        assert_eq!(
            ThresholdValues::default().entries().len(),
            0,
            "nothing set is nothing listed"
        );
        let all: Config = toml::from_str(&format!(
            "[thresholds]\n{}",
            crate::analytics::FIELDS
                .iter()
                .map(|f| format!(
                    "{} = {}\n",
                    f.name,
                    if f.name == "min_cohort" { 4 } else { 1 }
                ))
                .collect::<String>()
        ))
        .expect("every field is a key");
        assert_eq!(
            all.thresholds.values.entries().len(),
            crate::analytics::FIELDS.len()
        );
    }

    #[test]
    fn an_override_is_read_with_its_selector_and_name() {
        let t = thresholds_from(
            "[thresholds]\nstale_days = 120\n\
             [[thresholds.overrides]]\nname = \"lab machines\"\ntags = { site = \"lab\" }\n\
             stale_days = 365\n\
             [[thresholds.overrides]]\nsystem_model = \"Dell Inc. OptiPlex 3000\"\n\
             cpu_model = \"Intel(R) Core(TM) i5-12500\"\ndisk_free_pct = 5\n",
        )
        .expect("valid");
        assert_eq!(t.fleet.stale_days, 120.0);
        assert_eq!(t.overrides.len(), 2);
        assert_eq!(t.overrides[0].name, "lab machines");
        assert_eq!(t.overrides[0].tags["site"], "lab");
        assert_eq!(t.overrides[0].set["stale_days"], 365.0);
        assert_eq!(
            t.overrides[1].name,
            "system_model=Dell Inc. OptiPlex 3000, cpu_model=Intel(R) Core(TM) i5-12500",
            "unnamed, it is named after what it picks"
        );
        assert!(t.shadowed().is_empty());
    }

    /// The cohort statistics describe a calculation across machines, so a
    /// value for some machines is refused — with the reason, not as an
    /// unknown key.
    #[test]
    fn a_cohort_statistic_in_an_override_is_refused_with_the_reason() {
        for (field, value) in [
            ("min_cohort", "6"),
            ("cohort_z", "3"),
            ("cohort_min_shortfall", "0.2"),
            ("mad_floor_frac", "0.05"),
        ] {
            let err = refused(&format!(
                "[[thresholds.overrides]]\ntags = {{ site = \"lab\" }}\n{field} = {value}\n"
            ));
            assert!(err.contains(field), "{err}");
            assert!(err.contains("fleet-wide only"), "{err}");
            assert!(
                err.contains("[thresholds] itself"),
                "and says where it goes: {err}"
            );
            // And fine in [thresholds].
            thresholds_from(&format!("[thresholds]\n{field} = {value}\n"))
                .unwrap_or_else(|e| panic!("{field} fleet-wide: {e:#}"));
        }
    }

    #[test]
    fn a_threshold_out_of_range_is_refused_naming_the_setting() {
        for (setting, want) in [
            ("disk_free_pct = 120", "percentage"),
            ("battery_health_pct = -1", "percentage"),
            ("regression_frac = 15", "fraction"),
            ("cohort_min_shortfall = 0", "fraction"),
            ("mad_floor_frac = 1", "fraction"),
            ("stale_days = 0", "more than zero"),
            ("replace_age_years = -5", "more than zero"),
            ("cohort_z = 0", "more than zero"),
            ("stale_days = nan", "finite"),
            ("firmware_old_years = inf", "finite"),
            ("unstable_cpu_subtests = 0", "at least 1"),
            ("min_cohort = 2", "at least 3"),
            ("min_cohort = 3.5", "whole number"),
            ("unstable_cpu_subtests = 2.5", "whole number"),
            ("stale_days = \"two\"", "a number"),
        ] {
            let name = setting.split(' ').next().unwrap();
            let err = refused(&format!("[thresholds]\n{setting}\n"));
            assert!(err.contains(name), "{setting}: {err}");
            assert!(err.contains(want), "{setting}: {err}");

            // The same check in an override, for the ones it may set.
            if crate::analytics::field(name).is_some_and(|f| f.per_machine) {
                let err = refused(&format!(
                    "[[thresholds.overrides]]\nname = \"lab\"\ntags = {{ site = \"lab\" }}\n{setting}\n"
                ));
                assert!(err.contains(name) && err.contains(want), "{setting}: {err}");
            }
        }
        thresholds_from("[thresholds]\ndisk_free_pct = 0\ndisk_free_critical_pct = 0\n")
            .expect("0% is a line: nothing is ever under it");
    }

    /// Critical has to be the lower line, wherever a machine's two values
    /// come from.
    #[test]
    fn a_critical_disk_line_above_the_warning_line_is_refused() {
        let err = refused("[thresholds]\ndisk_free_pct = 4\n");
        assert!(
            err.contains("disk_free_critical_pct (5) is above disk_free_pct (4)"),
            "{err}"
        );

        // An override's critical line against the fleet's warning line.
        let err = refused(
            "[[thresholds.overrides]]\nname = \"lab\"\ntags = { site = \"lab\" }\n\
             disk_free_critical_pct = 12\n",
        );
        assert!(
            err.contains("the override \"lab\"") && err.contains("[thresholds]"),
            "{err}"
        );

        // Two overrides that a machine could match both of.
        let two = |second: &str| {
            format!(
                "[[thresholds.overrides]]\nname = \"crit\"\ntags = {{ site = \"lab\" }}\n\
                 disk_free_critical_pct = 8\n\
                 [[thresholds.overrides]]\nname = \"free\"\n{second}\ndisk_free_pct = 6\n"
            )
        };
        let err = refused(&two("tags = { ring = \"pilot\" }"));
        assert!(
            err.contains("\"crit\"") && err.contains("\"free\""),
            "a lab machine in the pilot ring takes 8 from one and 6 from the other: {err}"
        );
        // Not when no machine can match both.
        thresholds_from(&two("tags = { site = \"office\" }"))
            .expect("a machine is in one site or the other");
        // A rule that sets both lines keeps a later rule's away from the
        // machines it picks.
        thresholds_from(
            "[[thresholds.overrides]]\nname = \"lab\"\ntags = { site = \"lab\" }\n\
             disk_free_critical_pct = 8\ndisk_free_pct = 9\n\
             [[thresholds.overrides]]\nname = \"pilot\"\ntags = { ring = \"pilot\" }\n\
             disk_free_pct = 6\ndisk_free_critical_pct = 3\n",
        )
        .expect("a lab machine in the pilot ring takes both lines from the lab rule");
        // An override that sets both, consistently, is fine.
        thresholds_from(
            "[[thresholds.overrides]]\ntags = { site = \"kiosk\" }\ndisk_free_pct = 3\n\
             disk_free_critical_pct = 1\n",
        )
        .expect("both lines lowered together");
    }

    #[test]
    fn an_override_that_cannot_mean_what_it_says_is_refused() {
        for (rule, want) in [
            ("stale_days = 365", "picks no machines"),
            ("tags = {}\nstale_days = 365", "empty tags table"),
            (
                "system_model = \"\"\nstale_days = 365",
                "empty system_model",
            ),
            ("cpu_model = \"  \"\nstale_days = 365", "empty cpu_model"),
            (
                "name = \"\"\ntags = { site = \"lab\" }\nstale_days = 365",
                "empty name",
            ),
            ("tags = { site = \"\" }\nstale_days = 365", "empty tag"),
            ("tags = { site = \"lab\" }", "sets no thresholds"),
            (
                "tags = { site = \"PUT-THE-SITE-HERE\" }\nstale_days = 365",
                "placeholder",
            ),
            (
                "tags = { site = \"lab\" }\nstale_day = 365",
                "stale_day is not a setting",
            ),
            ("tags = \"site=lab\"\nstale_days = 365", "tags"),
            (
                "tags = { site = \"lab\" }\noverrides = []",
                "overrides is not a setting",
            ),
        ] {
            let err = refused(&format!("[[thresholds.overrides]]\n{rule}\n"));
            assert!(err.contains(want), "{rule}: {err}");
        }
    }

    #[test]
    fn two_overrides_of_one_name_are_refused() {
        let err = refused(
            "[[thresholds.overrides]]\nname = \"lab\"\ntags = { site = \"lab\" }\nstale_days = 365\n\
             [[thresholds.overrides]]\nname = \"lab\"\ntags = { site = \"lab2\" }\nstale_days = 365\n",
        );
        assert!(err.contains("both named \"lab\""), "{err}");
        // Two unnamed rules with the same selector get the same name, and are
        // better written as one.
        let err = refused(
            "[[thresholds.overrides]]\ntags = { site = \"lab\" }\nstale_days = 365\n\
             [[thresholds.overrides]]\ntags = { site = \"lab\" }\ndisk_free_pct = 5\n",
        );
        assert!(err.contains("merged"), "{err}");
    }

    #[test]
    fn an_unknown_threshold_is_an_error_not_a_shrug() {
        let err = refused("[thresholds]\nstale_day = 365\n");
        assert!(
            err.contains("thresholds.stale_day is not a setting"),
            "{err}"
        );
        assert!(
            err.contains("stale_days"),
            "the list names the real ones: {err}"
        );
        let err = refused("[thresholds]\nname = \"lab\"\n");
        assert!(err.contains("thresholds.name"), "{err}");
    }

    /// The starter's override examples are written to be uncommented: the
    /// lab rule works as it stands, and the kiosk rule refuses until its
    /// placeholder is filled in.
    #[test]
    fn the_starter_files_threshold_examples_work_once_uncommented() {
        let starter = Config::starter();
        let at = starter.find("[thresholds]").expect("the section");
        let end = starter[at..]
            .find("\n[[auth.grants]]")
            .expect("ends before the grants")
            + at;
        // Uncomment the settings and headers; drop the prose around them.
        let setting = |l: &str| {
            l.starts_with("[[")
                || l.split_once(" = ").is_some_and(|(k, _)| {
                    !k.is_empty() && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                })
        };
        let section: String = starter[at..end]
            .lines()
            .filter_map(|l| match l.strip_prefix("# ") {
                Some(rest) if setting(rest) => Some(rest),
                Some(_) => None,
                None if l.starts_with('#') => None,
                None => Some(l),
            })
            .map(|l| format!("{l}\n"))
            .collect();
        let err = format!("{:#}", thresholds_from(&section).expect_err("placeholder"));
        assert!(err.contains("placeholder"), "{err}\n{section}");
        let t = thresholds_from(&section.replace(
            "PUT-THE-SYSTEM-MODEL-OF-YOUR-KIOSKS-HERE",
            "HP t640 Thin Client",
        ))
        .unwrap_or_else(|e| panic!("{e:#}\n{section}"));
        assert_eq!(
            t.fleet,
            crate::analytics::Thresholds::default(),
            "{section}"
        );
        assert_eq!(t.overrides.len(), 2);
    }

    #[test]
    fn an_unknown_key_in_the_file_is_an_error_not_a_shrug() {
        // A typo in a security setting must not be silently ignored.
        let err = toml::from_str::<Config>("[auth]\nmode = \"oidc\"\nissuerr = \"x\"\n")
            .expect_err("unknown keys are refused");
        assert!(err.to_string().contains("issuerr"), "{err}");
    }
}
