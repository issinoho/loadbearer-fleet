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
//!   `machine.identity`, `telemetry`, `battery`, and `machine.firmware` /
//!   `drives` / `age` are all omitted when empty
//!   or unsupported, and older files predate them entirely — a 1.2.4 result has
//!   no identity block at all. The same goes for `run`, `machine.os_detail` and
//!   `machine.memory`, and for every field inside them: absent is unknown,
//!   never zero, false or "none".
//! * **Opt-in facts have three states.** `machine.boot`, `machine.security`
//!   and `run.elevated` are written only when the run was asked for them with
//!   `--collect`, and `run.collect` records what was asked. A category not in
//!   that list was *not asked* — every run from an older loadbearer included —
//!   which says nothing about the machine; one in it whose field is absent was
//!   *asked and could not be read*. Neither is ever "off" or "no".
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
    /// Which run this was, when its benchmarks started and how long they took.
    /// From a loadbearer release after 1.8.1; absent on older files and on a
    /// synthetic result no run produced.
    #[serde(default)]
    pub run: Option<RunInfo>,
}

/// The `run` block. Every field is optional on its own: the ID is left out
/// when the OS would not supply randomness, rather than made up.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunInfo {
    /// Random, UUID-v4-shaped, generated fresh for each run and derived from
    /// nothing. It names a run; it is **not** what makes two documents the
    /// same here — see `Index::ingest_text`.
    #[serde(default)]
    pub id: Option<String>,
    /// RFC 3339 UTC. `timestamp` is when the document was written, at the end.
    #[serde(default)]
    pub started_at: Option<String>,
    /// Wall-clock seconds from `started_at` until the run was scored — the
    /// `--jitter` wait, the link probe and the soak are not in it.
    #[serde(default)]
    pub duration_secs: Option<f64>,
    /// The opt-in categories this run was asked to collect (`--collect`):
    /// `uptime`, `elevation`, `security`. Omitted when none was, and absent
    /// on every run from an older loadbearer. Kept as text, like any open
    /// vocabulary, so a category this build has never heard of is carried
    /// rather than refused. A category missing from here was **not asked**;
    /// one listed whose block or field is absent was asked and **could not
    /// be read** — see `Collected`.
    #[serde(default)]
    pub collect: Vec<String>,
    /// Whether this run had administrator / root rights. Only with
    /// `elevation` asked, and absent when the query failed — never a
    /// guessed `false`.
    #[serde(default)]
    pub elevated: Option<bool>,
}

/// What a run was asked to collect, from `run.collect`, normalised: trimmed,
/// lower-cased, de-duplicated and sorted, so the index's text form of it is
/// the same for the same set however the document ordered or spelled it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected(Vec<String>);

impl Collected {
    pub fn of(names: &[String]) -> Self {
        let mut v: Vec<String> = names
            .iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        v.sort();
        v.dedup();
        Collected(v)
    }

    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|n| n == name)
    }

    /// `elevation,security,uptime`, or `None` when nothing was asked — which
    /// is also what every run from an older loadbearer reads as.
    pub fn text(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.join(","))
    }
}

/// `machine.boot`, only with `--collect uptime`: when the machine last
/// booted and how long it had been up **when the inventory was taken** —
/// a fact as of the run, never a live uptime. On Windows with Fast Startup
/// (the default) a shutdown does not restart the kernel, so this is the time
/// since the last *restart*.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Boot {
    /// RFC 3339 UTC, to the second.
    #[serde(default)]
    pub booted_at: Option<String>,
    #[serde(default)]
    pub uptime_secs: Option<u64>,
}

/// `machine.security`, only with `--collect security`. Every part is absent
/// where the CLI could not read it, and the fixed vocabularies stay text so
/// that a value this build has never heard of is carried, not refused.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Security {
    /// `"uefi"` or `"bios"` (not booted through UEFI: legacy BIOS/CSM, or a VM
    /// started straight into its kernel).
    #[serde(default)]
    pub firmware_mode: Option<String>,
    /// Whether Secure Boot is enforcing. Absent on a machine that did not boot
    /// through UEFI — there is no Secure Boot there to be off — as well as
    /// where it could not be read.
    #[serde(default)]
    pub secure_boot: Option<bool>,
    #[serde(default)]
    pub tpm: Option<Tpm>,
    /// **Linux only**, and only the root filesystem. Never present on
    /// Windows, where BitLocker cannot be read within the CLI's privacy
    /// promises.
    #[serde(default)]
    pub disk_encryption: Option<DiskEncryption>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Tpm {
    /// Whether the OS exposes a TPM. `false` covers none fitted, one off in
    /// firmware and (Linux) no driver for it. Required in the CLI's own type;
    /// optional here, like everything else this reads.
    #[serde(default)]
    pub present: Option<bool>,
    /// `"2.0"` or `"1.2"`; absent when the OS does not say.
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DiskEncryption {
    /// `"encrypted"`: the root filesystem sits on a dm-crypt mapping.
    /// `"none_detected"`: no dm-crypt under `/` — which is **not**
    /// "unencrypted": ZFS native encryption, fscrypt, eCryptfs and
    /// self-encrypting drives are not looked for.
    #[serde(default)]
    pub system_volume: Option<String>,
    /// With `"encrypted"`, the cryptsetup type: `luks1`, `luks2`, `plain`, …
    #[serde(default)]
    pub method: Option<String>,
}

/// `machine.os_detail`: the OS release in parts, beside the free-text `os`
/// and `kernel`. Windows fills `version` (`24H2`), `build` (`26100.2314`) and
/// `edition` (`Enterprise`); Linux `distro`, `version` and sometimes
/// `edition`, from os-release's `ID`, `VERSION_ID` and `VARIANT_ID`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OsDetail {
    #[serde(default)]
    pub distro: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub build: Option<String>,
    #[serde(default)]
    pub edition: Option<String>,
}

/// `machine.memory`: how the RAM is built, from the SMBIOS memory-device
/// table. Absent without root on Linux, where that table is root-only — which
/// says nothing about the memory, so it is never read as "no slots".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Memory {
    /// Sockets for system RAM, filled or not.
    #[serde(default)]
    pub slots: Option<u32>,
    #[serde(default)]
    pub populated: Option<u32>,
    /// One per filled slot, in table order.
    #[serde(default)]
    pub modules: Vec<MemoryModule>,
}

/// One memory module. Anything the firmware reported as unknown is absent.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemoryModule {
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// `DDR4`, `DDR5`, `LPDDR5`, … — open, so kept as text.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// `DIMM`, `SODIMM`, `Row Of Chips` (soldered), …
    #[serde(default)]
    pub form_factor: Option<String>,
    /// The module's rated speed, MT/s.
    #[serde(default)]
    pub speed_mts: Option<u32>,
    /// What the memory controller runs it at, MT/s. Below `speed_mts` is
    /// common and often the platform's limit rather than a fault, which is
    /// why nothing here judges it.
    #[serde(default)]
    pub configured_speed_mts: Option<u32>,
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
    /// System model and firmware. 1.6.0+.
    #[serde(default)]
    pub firmware: Option<Firmware>,
    /// Health counters for each NVMe or SATA drive loadbearer could query —
    /// absent on a run without admin / root. 1.6.0+.
    #[serde(default)]
    pub drives: Vec<Drive>,
    /// loadbearer's estimate of how old the machine is. 1.6.0+.
    #[serde(default)]
    pub age: Option<Age>,
    /// Mains or battery and the power settings in force. 1.7.0+.
    #[serde(default)]
    pub power: Option<Power>,
    /// The OS build, release and edition. A loadbearer release after 1.8.1.
    #[serde(default)]
    pub os_detail: Option<OsDetail>,
    /// Memory slots and modules. A loadbearer release after 1.8.1, and only
    /// as root on Linux.
    #[serde(default)]
    pub memory: Option<Memory>,
    /// When the machine last booted. Only with `--collect uptime` (a
    /// loadbearer release after 1.8.1); `run.collect` says whether it was
    /// asked for.
    #[serde(default)]
    pub boot: Option<Boot>,
    /// Firmware mode, Secure Boot, TPM and (Linux) disk encryption. Only with
    /// `--collect security`.
    #[serde(default)]
    pub security: Option<Security>,
}

/// The power settings a run had. Windows records a power mode for plugged in
/// and on battery; Linux has no such split and records the platform profile
/// in force instead.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Power {
    /// `mains` or `battery`.
    #[serde(default)]
    pub source: Option<String>,
    /// The power plan's name, in the machine's display language (Windows).
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub plugged_in: Option<PowerColumn>,
    #[serde(default)]
    pub on_battery: Option<PowerColumn>,
    /// e.g. `balanced`, `low-power` (Linux).
    #[serde(default)]
    pub platform_profile: Option<String>,
    /// The settings in force that would hold a benchmark back, battery power
    /// aside. Descriptive text — loadbearer's `VERSIONING.md` says to key on
    /// whether it's empty, not on the wording.
    #[serde(default)]
    pub held_back: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PowerColumn {
    /// `best power efficiency`, `balanced` or `best performance`.
    #[serde(default)]
    pub power_mode: Option<String>,
}

impl Power {
    pub fn on_ac(&self) -> Option<bool> {
        match self.source.as_deref() {
            Some("mains") => Some(true),
            Some("battery") => Some(false),
            _ => None,
        }
    }

    /// The Windows power mode for the power source the run was on, or the
    /// Linux platform profile.
    pub fn mode_in_force(&self) -> Option<String> {
        let column = if self.on_ac() == Some(false) {
            &self.on_battery
        } else {
            &self.plugged_in
        };
        column
            .as_ref()
            .and_then(|c| c.power_mode.clone())
            .or_else(|| self.platform_profile.clone())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Firmware {
    #[serde(default)]
    pub system_vendor: Option<String>,
    #[serde(default)]
    pub system_model: Option<String>,
    /// The marketing name on Lenovo, where `system_model` is a type code.
    #[serde(default)]
    pub system_version: Option<String>,
    #[serde(default)]
    pub bios_version: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(default)]
    pub bios_date: Option<String>,
}

impl Firmware {
    /// "Dell Inc. Latitude 7440", with Lenovo's marketing name in brackets —
    /// the same rendering loadbearer's own `info` uses.
    pub fn system_name(&self) -> Option<String> {
        let mut name = [self.system_vendor.as_deref(), self.system_model.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let lenovo = self
            .system_vendor
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("lenovo"));
        if let (true, Some(v)) = (lenovo, &self.system_version) {
            name = if name.is_empty() {
                v.clone()
            } else {
                format!("{name} ({v})")
            };
        }
        (!name.is_empty()).then_some(name)
    }
}

/// One drive's health counters: an NVMe health log or a SATA drive's Device
/// Statistics. Everything but the hours is optional, because SATA can't fill
/// the NVMe-only fields and a hard disk has no wear figure.
#[derive(Debug, Clone, Deserialize)]
pub struct Drive {
    #[serde(default)]
    pub device: String,
    /// `"nvme"` or `"sata"`; open, so kept as text.
    #[serde(default)]
    pub interface: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub power_on_hours: u64,
    #[serde(default)]
    pub power_cycles: Option<u64>,
    #[serde(default)]
    pub unsafe_shutdowns: Option<u64>,
    /// The vendor's estimate of rated endurance used. Can pass 100.
    #[serde(default)]
    pub percentage_used: Option<u8>,
    #[serde(default)]
    pub available_spare_pct: Option<u8>,
    #[serde(default)]
    pub data_written_tb: Option<f64>,
    #[serde(default)]
    pub media_errors: Option<u64>,
    /// Non-zero means the drive itself is reporting a problem. NVMe only.
    #[serde(default)]
    pub critical_warning: Option<u8>,
}

/// The age estimate. Dates are `YYYY-MM`, spans whole months, all as of the
/// run. loadbearer's `VERSIONING.md` says the *values* come from a data table
/// that changes between releases and `cpu_family` is descriptive, so this keys
/// on `cpu_launched`, never the wording.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Age {
    #[serde(default)]
    pub as_of: String,
    #[serde(default)]
    pub cpu_family: Option<String>,
    /// When the CPU's generation launched — the machine can't be older.
    #[serde(default)]
    pub cpu_launched: Option<String>,
    /// Upper bound on the machine's age at the run.
    #[serde(default)]
    pub max_age_months: Option<u32>,
    #[serde(default)]
    pub firmware_dated: Option<String>,
    #[serde(default)]
    pub firmware_after_launch_months: Option<u32>,
    /// The firmware is probably what the machine shipped with, which dates the
    /// machine to about `firmware_dated`.
    #[serde(default)]
    pub firmware_likely_original: Option<bool>,
    #[serde(default)]
    pub drive_power_on_hours: Option<u64>,
    /// The drive averages minutes per power cycle, so its hours measure time
    /// spent working rather than time in service — some SATA SSDs stop the
    /// clock when idle. The hours are as reported; don't read them as age.
    #[serde(default)]
    pub drive_hours_undercounted: Option<bool>,
    #[serde(default)]
    pub battery_cycles: Option<u32>,
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
    /// A USB stick or card reader. Present in every version so far, but
    /// defaulted rather than required — the reader tolerates its absence.
    #[serde(default)]
    pub removable: bool,
}

/// Smaller than this is not a volume anyone stores work on: an EFI system
/// partition, a recovery partition, a snap's loop mount, a RAM-backed `/tmp`.
/// Those run nearly full by design, so judging free space on them would flag
/// every machine in the estate.
pub const MIN_FIXED_VOLUME_BYTES: u64 = 8 * 1024 * 1024 * 1024;

impl Machine {
    /// The fixed volume with the least free space as a share of its size —
    /// the one a "disk nearly full" finding is about. Removable media and
    /// anything under [`MIN_FIXED_VOLUME_BYTES`] are left out.
    ///
    /// Ties keep the first listed, so the answer doesn't depend on anything
    /// but the document.
    pub fn fullest_fixed_volume(&self) -> Option<(&Disk, f64)> {
        self.disks
            .iter()
            .filter(|d| !d.removable && d.total_bytes >= MIN_FIXED_VOLUME_BYTES)
            .map(|d| (d, d.available_bytes as f64 / d.total_bytes as f64 * 100.0))
            .fold(None, |best: Option<(&Disk, f64)>, (d, pct)| match best {
                Some((_, b)) if b <= pct => best,
                _ => Some((d, pct)),
            })
    }
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
    /// `higher_is_better` or `lower_is_better`, straight from the document.
    ///
    /// A string rather than an enum: this is somebody else's schema, and a
    /// value we do not recognise should be carried through and declined by
    /// whatever needs it, not refuse the whole document at parse time. Only
    /// the `raw` block carries it — the scored block leaves direction implicit
    /// in a `ratio` that has already been adjusted for it.
    #[serde(default)]
    pub direction: Option<String>,
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
    /// Whether the run was on mains: the gates' reading where one was asked
    /// for, otherwise what loadbearer 1.7.0+ records in `machine.power`.
    pub fn on_ac(&self) -> Option<bool> {
        self.gates
            .as_ref()
            .and_then(|g| g.on_ac)
            .or_else(|| self.machine.power.as_ref().and_then(Power::on_ac))
    }

    pub fn quality_caveats(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(t) = &self.telemetry
            && t.thermal_limited
        {
            out.push("clocks fell during the run; peak figures may not have reached boost".into());
        }
        if self.on_ac() == Some(false) {
            out.push("measured on battery, where clocks are usually capped".into());
        }
        if let Some(p) = &self.machine.power
            && !p.held_back.is_empty()
        {
            out.push(format!(
                "power settings held it back: {}",
                p.held_back.join(", ")
            ));
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

/// The blocks a loadbearer release after 1.8.1 adds, for the tests to put on a
/// real fixture: none of the fixtures was taken by one. Each is shaped as that
/// CLI writes it — every field optional, nothing written for what it could not
/// read.
#[cfg(test)]
pub(crate) mod samples {
    use serde_json::{Value, json};

    /// The real fixture, as JSON to mutate.
    pub(crate) fn fixture(name: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
    }

    /// Windows-shaped: a run ID and duration, the build, display version and
    /// edition, and two of four slots filled with matching SODIMMs held below
    /// their rating.
    pub(crate) fn windows(d: &mut Value) {
        d["run"] = json!({
            "id": "0f8b5a52-7c1e-4c39-9d0e-5a1f3b2c4d6e",
            "started_at": "2026-09-09T14:42:01.1234567Z",
            "duration_secs": 472.318
        });
        d["machine"]["os_detail"] = json!({
            "version": "24H2", "build": "26100.2314", "edition": "Enterprise"
        });
        d["machine"]["memory"] = json!({
            "slots": 4, "populated": 2,
            "modules": [
                {"size_bytes": 17179869184_u64, "type": "DDR5", "form_factor": "SODIMM",
                 "speed_mts": 5600, "configured_speed_mts": 4800},
                {"size_bytes": 17179869184_u64, "type": "DDR5", "form_factor": "SODIMM",
                 "speed_mts": 5600, "configured_speed_mts": 4800}
            ]
        });
    }

    /// Linux-shaped, as root: the distribution, release and variant and no
    /// build; one slot of two empty, and a module whose firmware reported no
    /// speed at all. No `run.id`, as when the OS had no randomness to give.
    pub(crate) fn linux_root(d: &mut Value) {
        d["run"] = json!({ "started_at": "2026-09-09T10:00:00Z", "duration_secs": 95.5 });
        d["machine"]["os_detail"] = json!({
            "distro": "ubuntu", "version": "26.04", "edition": "server"
        });
        d["machine"]["memory"] = json!({
            "slots": 2, "populated": 1,
            "modules": [{"size_bytes": 8589934592_u64, "type": "DDR4", "form_factor": "DIMM"}]
        });
    }

    /// Linux-shaped, unprivileged: os-release without a variant, and no
    /// memory block at all.
    pub(crate) fn linux_user(d: &mut Value) {
        d["run"] = json!({ "id": "5b1d7e0c-3a2f-4e8b-9c6d-1e2f3a4b5c6d", "duration_secs": 61.0 });
        d["machine"]["os_detail"] = json!({ "distro": "debian", "version": "13" });
    }

    /// The opt-in blocks (`--collect`, a loadbearer release after 1.8.1), each
    /// added to whatever `run` block the document already has.
    fn asked(d: &mut Value, names: &[&str]) {
        d["run"]["collect"] = json!(names);
    }

    /// Windows, all three asked: up four hours, not elevated, UEFI with Secure
    /// Boot on and a TPM 2.0 — and no `disk_encryption`, which the CLI never
    /// writes on Windows.
    pub(crate) fn optin_windows(d: &mut Value) {
        asked(d, &["uptime", "elevation", "security"]);
        d["run"]["elevated"] = json!(false);
        d["machine"]["boot"] = json!({ "booted_at": "2026-09-09T10:41:28Z", "uptime_secs": 14492 });
        d["machine"]["security"] = json!({
            "firmware_mode": "uefi", "secure_boot": true,
            "tpm": { "present": true, "version": "2.0" }
        });
    }

    /// Linux as root, all three asked: Secure Boot off, a TPM 2.0, and the root
    /// filesystem on LUKS2. Up three days.
    pub(crate) fn optin_linux_luks(d: &mut Value) {
        asked(d, &["uptime", "elevation", "security"]);
        d["run"]["elevated"] = json!(true);
        d["machine"]["boot"] =
            json!({ "booted_at": "2026-09-06T08:00:00Z", "uptime_secs": 266400 });
        d["machine"]["security"] = json!({
            "firmware_mode": "uefi", "secure_boot": false,
            "tpm": { "present": true, "version": "2.0" },
            "disk_encryption": { "system_volume": "encrypted", "method": "luks2" }
        });
    }

    /// Linux, only `security` asked: Secure Boot on, no TPM exposed, and no
    /// dm-crypt under `/` — which is not "unencrypted".
    pub(crate) fn optin_linux_none_detected(d: &mut Value) {
        asked(d, &["security"]);
        d["machine"]["security"] = json!({
            "firmware_mode": "uefi", "secure_boot": true,
            "tpm": { "present": false },
            "disk_encryption": { "system_volume": "none_detected" }
        });
    }

    /// A legacy-BIOS boot: no `secure_boot` at all, because there is none to
    /// be on or off; a TPM 1.2. Uptime asked too.
    pub(crate) fn optin_legacy_bios(d: &mut Value) {
        asked(d, &["security", "uptime"]);
        d["machine"]["boot"] =
            json!({ "booted_at": "2026-08-30T07:15:00Z", "uptime_secs": 958_000 });
        d["machine"]["security"] = json!({
            "firmware_mode": "bios",
            "tpm": { "present": true, "version": "1.2" },
            "disk_encryption": { "system_volume": "none_detected" }
        });
    }

    /// All three asked, none readable: `run.collect` and nothing else.
    pub(crate) fn optin_unreadable(d: &mut Value) {
        asked(d, &["uptime", "elevation", "security"]);
    }

    /// Values this build has never heard of in every vocabulary, an unknown
    /// category, unknown fields, and the list in another order and case.
    pub(crate) fn optin_future(d: &mut Value) {
        asked(d, &["Security", "network", "security"]);
        d["machine"]["security"] = json!({
            "firmware_mode": "coreboot", "secure_boot": true, "measured_boot": true,
            "tpm": { "present": true, "version": "3.0", "vendor_hidden": true },
            "disk_encryption": { "system_volume": "partial", "method": "zfs" }
        });
    }

    /// The blocks without `run.collect`: what no CLI writes, and which the
    /// index therefore records as not asked rather than guess from.
    pub(crate) fn optin_blocks_without_collect(d: &mut Value) {
        d["run"]["elevated"] = json!(false);
        d["machine"]["boot"] = json!({ "booted_at": "2026-09-09T10:41:28Z", "uptime_secs": 14492 });
        d["machine"]["security"] = json!({ "firmware_mode": "uefi", "secure_boot": false });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: &serde_json::Value) -> ResultFile {
        ResultFile::from_json(&v.to_string()).expect("parses")
    }

    #[test]
    fn a_document_without_the_new_blocks_reads_as_unknown() {
        let doc = parse(&samples::fixture("legacy-1.2.4.json"));
        assert!(doc.run.is_none());
        assert!(doc.machine.os_detail.is_none());
        assert!(doc.machine.memory.is_none());
    }

    #[test]
    fn the_new_blocks_are_read() {
        let mut v = samples::fixture("win-modern.json");
        samples::windows(&mut v);
        let doc = parse(&v);
        let run = doc.run.expect("run");
        assert_eq!(
            run.id.as_deref(),
            Some("0f8b5a52-7c1e-4c39-9d0e-5a1f3b2c4d6e")
        );
        assert_eq!(run.duration_secs, Some(472.318));
        let os = doc.machine.os_detail.expect("os_detail");
        assert_eq!(os.build.as_deref(), Some("26100.2314"));
        assert_eq!(os.distro, None);
        let mem = doc.machine.memory.expect("memory");
        assert_eq!((mem.slots, mem.populated), (Some(4), Some(2)));
        assert_eq!(mem.modules[0].kind.as_deref(), Some("DDR5"));
        assert_eq!(mem.modules[1].configured_speed_mts, Some(4800));
    }

    /// Empty blocks, nulls, and fields this build has never heard of — the
    /// contract says a new optional field is not a breaking change, so one
    /// inside a new block isn't either.
    #[test]
    fn empty_blocks_and_unknown_fields_inside_them_parse() {
        let mut v = samples::fixture("win-modern.json");
        v["run"] = json!({});
        v["machine"]["os_detail"] = json!({ "codename": "noble", "version": null });
        v["machine"]["memory"] = json!({
            "channels": 2,
            "modules": [{ "rank": 2, "voltage_mv": 1100 }, {}]
        });
        let doc = parse(&v);
        assert!(doc.run.expect("run").id.is_none());
        assert!(doc.machine.os_detail.expect("os").version.is_none());
        let mem = doc.machine.memory.expect("memory");
        assert_eq!(mem.slots, None, "absent, not zero");
        assert_eq!(mem.modules.len(), 2);
        assert!(mem.modules[0].size_bytes.is_none());
    }

    #[test]
    fn the_opt_in_blocks_are_read_and_absent_without_them() {
        let old = parse(&samples::fixture("legacy-1.2.4.json"));
        assert!(old.machine.boot.is_none() && old.machine.security.is_none());

        let mut v = samples::fixture("win-modern.json");
        samples::windows(&mut v);
        samples::optin_windows(&mut v);
        let doc = parse(&v);
        let run = doc.run.expect("run");
        assert_eq!(run.collect, ["uptime", "elevation", "security"]);
        assert_eq!(run.elevated, Some(false));
        assert_eq!(run.duration_secs, Some(472.318), "the 1.8.1 block is kept");
        let boot = doc.machine.boot.expect("boot");
        assert_eq!(boot.uptime_secs, Some(14492));
        let sec = doc.machine.security.expect("security");
        assert_eq!(sec.firmware_mode.as_deref(), Some("uefi"));
        assert_eq!(sec.secure_boot, Some(true));
        let tpm = sec.tpm.expect("tpm");
        assert_eq!(
            (tpm.present, tpm.version.as_deref()),
            (Some(true), Some("2.0"))
        );
        assert!(sec.disk_encryption.is_none(), "never on Windows");

        let mut v = samples::fixture("linux-throttled.json");
        samples::optin_legacy_bios(&mut v);
        let sec = parse(&v).machine.security.expect("security");
        assert_eq!(sec.firmware_mode.as_deref(), Some("bios"));
        assert_eq!(sec.secure_boot, None, "no Secure Boot to be off");
    }

    /// A value in a vocabulary this build has never heard of, an unknown
    /// category and unknown fields are carried, not refused.
    #[test]
    fn unknown_opt_in_values_and_fields_parse() {
        let mut v = samples::fixture("linux-throttled.json");
        samples::optin_future(&mut v);
        let doc = parse(&v);
        let sec = doc.machine.security.expect("security");
        assert_eq!(sec.firmware_mode.as_deref(), Some("coreboot"));
        assert_eq!(sec.tpm.expect("tpm").version.as_deref(), Some("3.0"));
        let disk = sec.disk_encryption.expect("disk");
        assert_eq!(disk.system_volume.as_deref(), Some("partial"));
        let asked = Collected::of(&doc.run.expect("run").collect);
        assert!(asked.has("security") && asked.has("network") && !asked.has("uptime"));
        assert_eq!(asked.text().as_deref(), Some("network,security"));

        // Empty blocks, too: asked, and nothing in them.
        v["machine"]["security"] = json!({ "tpm": {} });
        v["machine"]["boot"] = json!({});
        let doc = parse(&v);
        assert!(
            doc.machine
                .security
                .expect("s")
                .tpm
                .expect("t")
                .present
                .is_none()
        );
        assert!(doc.machine.boot.expect("b").uptime_secs.is_none());
        assert_eq!(Collected::of(&[]).text(), None);
    }
}
