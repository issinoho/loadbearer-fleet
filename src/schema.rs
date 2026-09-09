//! Reading `loadbearer.result/1` documents.
//!
//! These types are deliberately **not** shared with loadbearer's own. Its
//! `VERSIONING.md` makes the `schema`-tagged JSON a stability contract and says
//! in the same breath that the internal Rust API is not one — module layout and
//! function signatures can change in any release. So the supported way to
//! consume a result file is to parse the document, which is what this does.
//! The two projects can then move independently.
//!
//! Everything here follows from that:
//!
//! * **Unknown fields are ignored**, because the contract says a new optional
//!   field is not a breaking change and consumers must tolerate one. There is
//!   no `deny_unknown_fields` anywhere in this file.
//! * **Nearly everything is optional.** `gates`, `notes`, `tags`,
//!   `machine.identity`, `telemetry` and `battery` are all omitted when empty
//!   or unsupported, and older files predate them entirely — a 1.2.4 result has
//!   no identity block at all.
//! * **Open enumerations stay `String`.** `grade`, `confidence` and
//!   `representative` are parsed as text rather than enums so that a value this
//!   build has never heard of degrades to "unrecognised" instead of failing the
//!   whole file. A dashboard that refuses to load a fleet because one machine
//!   ran a newer loadbearer would be worse than useless.

// These structs mirror the document, not this build's current appetite. Parsing
// the whole thing keeps the mapping to the schema obvious and means adding a
// view later is a query change rather than a parser change — so several fields
// are read by nothing yet, on purpose.
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::Deserialize;

/// The schema family this understands. The number after the slash is the major
/// version: `/2` would mean fields were removed, renamed or retyped, and this
/// build should decline rather than guess.
pub const SCHEMA_FAMILY: &str = "loadbearer.result/";
pub const SCHEMA_MAJOR: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct ResultFile {
    pub schema: String,
    pub tool_version: String,
    /// RFC 3339, UTC, as written by the run.
    pub timestamp: String,
    pub machine: Machine,
    pub config: RunConfig,
    pub overall: Overall,
    #[serde(default)]
    pub components: Vec<ScoredComponent>,
    /// Unscored per-subtest detail, including the per-run values behind each
    /// figure. Present in every version so far.
    #[serde(default)]
    pub raw: Vec<RawComponent>,
    /// `--tag k=v` labels. 1.3.0+.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// What didn't run as asked but didn't stop the run. 1.3.0+.
    #[serde(default)]
    pub notes: Vec<String>,
    /// What the unattended-run gates observed. 1.5.0+.
    #[serde(default)]
    pub gates: Option<Gates>,
    #[serde(default)]
    pub telemetry: Option<Telemetry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Machine {
    #[serde(default)]
    pub hostname: Option<String>,
    /// Firmware / OS identifiers. 1.3.0+; absent on older files and on a
    /// machine that would report none.
    #[serde(default)]
    pub identity: Option<Identity>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub kernel: Option<String>,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub cpu_model: String,
    #[serde(default)]
    pub cpu_vendor: String,
    #[serde(default)]
    pub cpu_physical_cores: Option<usize>,
    #[serde(default)]
    pub cpu_logical_cores: usize,
    #[serde(default)]
    pub ram_bytes: u64,
    #[serde(default)]
    pub disks: Vec<Disk>,
    #[serde(default)]
    pub battery: Option<Battery>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Identity {
    #[serde(default)]
    pub machine_id: Option<String>,
    #[serde(default)]
    pub smbios_uuid: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub asset_tag: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Disk {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mount_point: String,
    #[serde(default)]
    pub file_system: String,
    /// `"SSD"`, `"HDD"` or `"Unknown"`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Battery {
    #[serde(default)]
    pub charge_pct: f64,
    #[serde(default)]
    pub state: String,
    /// Present capacity as a percentage of design capacity — the wear signal.
    #[serde(default)]
    pub health_pct: Option<f64>,
    #[serde(default)]
    pub cycle_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunConfig {
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub duration_preset: String,
    #[serde(default)]
    pub curve_k: f64,
    #[serde(default)]
    pub seed: u64,
    #[serde(default)]
    pub threads: usize,
    #[serde(default)]
    pub baseline: String,
    /// Vector instruction set the CPU kernels were allowed to use. Results are
    /// only comparable within the same value.
    #[serde(default)]
    pub build_isa: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Overall {
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub grade: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub why: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScoredComponent {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub grade: String,
    /// False for a component measured but kept out of the overall grade —
    /// `network` and `gpu`.
    #[serde(default = "yes")]
    pub graded: bool,
    #[serde(default)]
    pub subtests: Vec<ScoredSubtest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScoredSubtest {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub value: f64,
    #[serde(default)]
    pub baseline: f64,
    /// Direction-adjusted ratio to the baseline; >1 is better than baseline.
    #[serde(default)]
    pub ratio: f64,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub confidence: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawComponent {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub subtests: Vec<RawSubtest>,
    /// Per-component caveats: a capped working set, a buffered-I/O fallback, a
    /// RAM-backed target directory. These are the reasons a number may not mean
    /// what it looks like.
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawSubtest {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub value: f64,
    /// Which statistic `value` is — `"median"` or `"peak"`. 1.5.0+; absent on
    /// older files, which were all medians.
    #[serde(default)]
    pub representative: Option<String>,
    #[serde(default)]
    pub stats: Option<Stats>,
    #[serde(default)]
    pub confidence: String,
    #[serde(default = "yes")]
    pub scored: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Stats {
    #[serde(default)]
    pub runs: Vec<f64>,
    #[serde(default)]
    pub median: f64,
    #[serde(default)]
    pub mean: f64,
    #[serde(default)]
    pub min: f64,
    #[serde(default)]
    pub max: f64,
    #[serde(default)]
    pub stddev: f64,
    /// Coefficient of variation as a fraction, not a percentage.
    #[serde(default)]
    pub cv: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Gates {
    #[serde(default)]
    pub on_ac: Option<bool>,
    #[serde(default)]
    pub cpu_load_pct: Option<f64>,
    #[serde(default)]
    pub jitter_secs: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Telemetry {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub sample_count: usize,
    #[serde(default)]
    pub mhz_start: f64,
    #[serde(default)]
    pub mhz_end: f64,
    #[serde(default)]
    pub mhz_min: f64,
    #[serde(default)]
    pub mhz_max: f64,
    #[serde(default)]
    pub mhz_mean: f64,
    #[serde(default)]
    pub thermal_limited: bool,
    /// The busiest-core window means the throttle verdict is made from. 1.5.1+.
    #[serde(default)]
    pub busiest_head_mhz: Option<f64>,
    #[serde(default)]
    pub busiest_tail_mhz: Option<f64>,
    #[serde(default)]
    pub package_watts_mean: Option<f64>,
}

fn yes() -> bool {
    true
}

/// Which identifier a machine ended up being keyed on, worst-to-best ordered so
/// a UI can say how much to trust the grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyKind {
    /// Renameable, sometimes unset, reissued between machines. A last resort.
    Hostname,
    /// Assigned by the OS install: always readable, but resets on reimage and
    /// is cloned by a careless VM template.
    MachineId,
    /// Firmware. Survives a reimage.
    SmbiosUuid,
    /// Firmware, and what asset, warranty and lease records key on.
    Serial,
}

impl KeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyKind::Serial => "serial",
            KeyKind::SmbiosUuid => "smbios_uuid",
            KeyKind::MachineId => "machine_id",
            KeyKind::Hostname => "hostname",
        }
    }
}

/// How a run is attributed to a machine across repeated sweeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineKey {
    pub value: String,
    pub kind: KeyKind,
}

impl ResultFile {
    /// Parse, and refuse a schema major this build doesn't understand rather
    /// than silently misreading it.
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        let doc: ResultFile = serde_json::from_str(text)?;
        let Some(rest) = doc.schema.strip_prefix(SCHEMA_FAMILY) else {
            anyhow::bail!("not a loadbearer result document (schema {:?})", doc.schema);
        };
        let major: u32 = rest
            .split('/')
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|_| anyhow::anyhow!("unreadable schema version in {:?}", doc.schema))?;
        if major != SCHEMA_MAJOR {
            anyhow::bail!(
                "result uses schema major {major}, this build reads {SCHEMA_MAJOR} \
                 (fields have been removed, renamed or retyped — upgrade the dashboard)"
            );
        }
        Ok(doc)
    }

    /// The identifier to attribute this run to, best available first.
    ///
    /// Firmware identifiers survive a reimage and match the asset record, so
    /// they win. `machine_id` is always readable but resets on reimage.
    /// `hostname` is the fallback and is the weakest: renameable, occasionally
    /// unset, and reissued between machines — which is the whole reason
    /// loadbearer started recording the others.
    pub fn machine_key(&self) -> MachineKey {
        let id = self.machine.identity.clone().unwrap_or_default();
        for (kind, value) in [
            (KeyKind::Serial, id.serial),
            (KeyKind::SmbiosUuid, id.smbios_uuid),
            (KeyKind::MachineId, id.machine_id),
            (KeyKind::Hostname, self.machine.hostname.clone()),
        ] {
            if let Some(v) = value {
                let v = v.trim();
                if !v.is_empty() {
                    return MachineKey {
                        value: v.to_string(),
                        kind,
                    };
                }
            }
        }
        // Nothing at all to key on. Falling back to the source path would make
        // every re-run look like a new machine, so say so explicitly instead.
        MachineKey {
            value: "unidentified".to_string(),
            kind: KeyKind::Hostname,
        }
    }

    /// True when the run completed but not everything ran, so its figures are
    /// usable for what did run and misleading if treated as a full sweep.
    pub fn is_partial(&self) -> bool {
        !self.notes.is_empty()
    }

    /// Reasons this run's numbers should not be compared at face value, drawn
    /// from the signals loadbearer already records.
    pub fn quality_caveats(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(t) = &self.telemetry
            && t.thermal_limited
        {
            out.push("clocks fell during the run; peak figures may not have reached boost".into());
        }
        if let Some(g) = &self.gates
            && g.on_ac == Some(false)
        {
            out.push("measured on battery, where clocks are usually capped".into());
        }
        for c in &self.raw {
            for n in &c.notes {
                // The RAM-backed target directory is the one that silently
                // turns the disk component into a memory benchmark.
                if n.contains("RAM-backed") {
                    out.push(format!("{}: {}", c.id, n));
                }
            }
        }
        out.extend(self.notes.iter().cloned());
        out
    }
}
