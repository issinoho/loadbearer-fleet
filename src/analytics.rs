//! Cohort analytics and the red-flag engine.
//!
//! The reference baseline gives an absolute grade, and that is the right answer
//! to "is this machine any good". It is the wrong answer to "is this machine
//! broken", for a reason loadbearer's own baseline header states: the anchors
//! carry ±10–20% uncertainty and some rest on three machines. A 15% shortfall
//! against the baseline could be the baseline. A 15% shortfall against forty
//! identical machines in the same estate is that machine.
//!
//! So the primary comparison here is **a machine against its peers**, and the
//! secondary one is **a machine against its own history**, which is stronger
//! still because the hardware is held constant. The baseline grade is kept for
//! the executive view, where an absolute answer is what's wanted.
//!
//! ## What makes two runs comparable
//!
//! A cohort is not "machines with the same CPU". It is machines with the same
//! CPU *measured the same way*, because four things change the number without
//! anything changing about the machine:
//!
//! * `build_isa` — the vector instruction set the kernels were allowed to use.
//!   Adding vector-AES roughly doubled `aes_gcm` on hardware that supports it,
//!   so an `sse2` result and an `avx2` result of the same silicon are two
//!   different measurements.
//! * `duration_preset` — not a pure precision knob. Memory latency in
//!   particular reads differently across presets, which loadbearer documents.
//! * `profile` — changes the component weights, so it changes the overall.
//! * `baseline` — changes the scale the score is expressed on entirely.
//!
//! Grouping across any of those produces a spread that is an artefact of the
//! configuration, and it buries the machine that is genuinely slow. Installed
//! RAM is deliberately *not* part of the key: two machines with the same CPU
//! and different DIMM configurations really do perform differently, and
//! surfacing the single-channel one is the point rather than something to
//! excuse away.
//!
//! ## Median and MAD, not mean and standard deviation
//!
//! The statistic has to survive the thing it is looking for. One badly degraded
//! machine drags a mean down and inflates a standard deviation, so
//! outlier detection built on them partly hides its own target — and it gets
//! worse as the cohort gets smaller, which is exactly where estates live.
//! Median and median absolute deviation don't move. Where a cohort is genuinely
//! uniform MAD collapses towards zero and any z-score explodes, so the scale is
//! floored at a fraction of the median; on a fleet of clones that floor is what
//! actually binds, and it is set so the effective trigger lands around a 15%
//! shortfall — comfortably above loadbearer's own few-percent run-to-run
//! spread.
//!
//! ## Grouping by anything else
//!
//! A cohort is the only grouping the statistics are drawn from. Sites, models
//! and operating systems are groupings of the *machines in view* (`groups`),
//! and what they compare is each member's standing in its own cohort, never the
//! raw score — otherwise "which site is in the worst shape" is answered by
//! "whichever had its refresh longest ago".
//!
//! ## Over time
//!
//! Everything above describes the fleet *now*. `timeline` reconstructs it as it
//! was at earlier dates from the runs the index already holds, through the same
//! rules — see its module notes for why it is reconstructed rather than
//! recorded, and what that costs.
//!
//! ## What to do about it
//!
//! Findings say what is wrong; an estate owner's next question is which
//! decision follows, and the decisions differ in kind and in cost. So every
//! machine also gets a **lifecycle** verdict (`lifecycle`): *replace* it,
//! *repair* a part, *investigate* why it is slower than its own hardware should
//! be, *watch* it, or *keep* it. It is a short rule over evidence the snapshot
//! already holds — the findings `evaluate` raised and the best estimate of the
//! machine's age — never a score, and it carries the sentences that fired it.
//! First match wins:
//!
//! 1. **replace** — graded F; or old *and* either graded D or F or with a part
//!    problem.
//! 2. **repair** — a part problem on a machine that is neither: a part, not a
//!    platform.
//! 3. **investigate** — slower than identical machines or than its own past. It
//!    should be faster than it is, and finding out why is cheaper than buying.
//! 4. **watch** — old, or graded D.
//! 5. **keep** — none of those.
//!
//! Two consequences are deliberate. **Age alone is not a reason to buy**: an old
//! machine that still performs and whose parts are sound is *watch*, not
//! *replace*. And **an F is against the reference baseline**, so *replace* for
//! a grade means "inadequate against that reference", which an estate with
//! modest needs may reasonably disagree with — the reason says so in as many
//! words, rather than dressing it up as a fault.
//!
//! A verdict that leans on a performance figure is marked uncertain when the
//! same run carries a measurement caveat — throttled, on battery, held back,
//! partial, RAM-backed — because the measurement condemning the machine may be
//! the thing that is wrong. One resting only on age and on parts that report
//! their own condition is not; a weak component is read off the benchmark, so
//! it counts as a performance figure here.

mod components;
mod thresholds;
mod timeline;

pub use components::{
    Distribution, Encoding, HEATMAP_ROWS, HeatSort, Heatmap, Measure, Measures, distribution,
    heatmap,
};
pub use thresholds::{AppliedOverride, FIELDS, Override, Tuning, field};
pub use timeline::{
    BySeverity, CHANGE_WINDOWS, Changes, FindingChange, TIMELINE_WEEKS, Timeline, TimelineView,
    parse_weeks, parse_window,
};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Every number a rule turns on, in one place, so tuning the engine is a config
/// change rather than a code read: `[thresholds]` in the config file sets any of
/// them for the whole fleet, and `[[thresholds.overrides]]` sets the ones that
/// judge a single machine for the machines a rule picks — see `thresholds`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Thresholds {
    /// Below this many members a cohort median is not worth comparing against:
    /// with three machines, one bad machine is a third of the sample.
    pub min_cohort: usize,
    /// Modified z-score (Iglewicz–Hoaglin) at which a member is an outlier.
    pub cohort_z: f64,
    /// A shortfall smaller than this is never named however unusual it is
    /// statistically, because nobody can act on 4%.
    pub cohort_min_shortfall: f64,
    /// MAD is floored at this fraction of the cohort median. See the module
    /// note: on a cohort of clones this is the threshold that binds.
    pub mad_floor_frac: f64,
    /// Drop against the median of a machine's own earlier runs that counts as a
    /// regression.
    pub regression_frac: f64,
    /// After this long, a machine's last result is history rather than
    /// inventory.
    pub stale_days: f64,
    /// Battery capacity, as a percentage of design, below which the battery is
    /// itself the finding.
    pub battery_health_pct: f64,
    /// How many low-confidence **CPU** subtests before the run is too shaky to
    /// read.
    ///
    /// CPU-only, and the reason is in the data. Counting low confidence across
    /// all components would fire on every machine in the estate: the seven real
    /// runs this was built against carry three to six low-confidence subtests
    /// each, and they cluster in disk writes and network, where wide spread is
    /// the device and the far end behaving normally — an SLC cache filling, a
    /// link doing something else. Nobody's fault, and nothing to action.
    ///
    /// CPU kernels are deterministic, so spread *there* means something else
    /// was running on the machine. Restricted that way, the same seven runs
    /// give 0 or 1 on six of them — including both thermally limited ones, so
    /// this is not just throttling reported twice — and 8 on the seventh. The
    /// threshold sits in that gap.
    pub unstable_cpu_subtests: i64,
    /// A drive's `percentage_used` at or above which it is worn. That figure
    /// is the vendor's estimate of rated write endurance used, not a failure
    /// prediction — drives routinely outlive it — so this is a warning to
    /// plan for; 100, where the rating is spent, is critical regardless.
    pub drive_wear_pct: f64,
    /// Available spare below which a drive is worn. The spare is the pool of
    /// blocks worn cells are remapped to; the drive's own critical warning
    /// typically trips at 10%, so this is set to say so before it does.
    pub drive_spare_pct: f64,
    /// Free space, as a percentage of the fullest fixed volume, below which
    /// the disk is nearly full. A percentage rather than bytes on purpose: an
    /// SSD slows as it fills whatever its size, and slow is what this tool
    /// measures.
    pub disk_free_pct: f64,
    /// The same, below which nearly full is critical.
    pub disk_free_critical_pct: f64,
    /// How far behind the newest firmware seen on the same system model a
    /// machine's has to be before it's named. A vendor typically ships a
    /// handful of releases a year; less than a quarter apart is a rollout in
    /// progress rather than a machine that was missed.
    pub firmware_behind_days: f64,
    /// Firmware older than this at the time of the run is named even when no
    /// peer of the same model shows a newer one.
    pub firmware_old_years: f64,
    /// A machine at least this old, by the best estimate of its age today, is
    /// *old* for the lifecycle verdict. Five years is the usual depreciation and
    /// warranty horizon for a business PC — and old on its own only ever makes
    /// a machine one to watch; see the module notes.
    pub replace_age_years: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_cohort: 4,
            cohort_z: 3.5,
            cohort_min_shortfall: 0.10,
            mad_floor_frac: 0.03,
            regression_frac: 0.15,
            stale_days: 90.0,
            battery_health_pct: 70.0,
            unstable_cpu_subtests: 2,
            drive_wear_pct: 80.0,
            drive_spare_pct: 20.0,
            disk_free_pct: 10.0,
            disk_free_critical_pct: 5.0,
            firmware_behind_days: 90.0,
            firmware_old_years: 4.0,
            replace_age_years: 5.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

/// Which queue a flag belongs in.
///
/// Keeping these apart is the difference between a useful dashboard and a wall
/// of red. "This machine is slow" is a hardware decision — repair, replace,
/// reassign. "This measurement can't be trusted" is a collection problem and
/// says nothing about the machine until it's fixed. "We have no current data"
/// isn't about the machine at all. Mixed together, an estate owner reads a
/// thermal caveat as a failing asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagKind {
    /// Something about the hardware.
    Machine,
    /// Something about how the measurement was taken.
    Measurement,
    /// Something about the data we hold, or don't.
    Coverage,
}

#[derive(Debug, Clone, Serialize)]
pub struct Flag {
    pub severity: Severity,
    pub kind: FlagKind,
    /// Stable identifier for the rule — for filtering, and what a suppression
    /// names. Always one of `RULES`.
    pub code: &'static str,
    pub machine_key: String,
    pub hostname: Option<String>,
    pub headline: String,
    pub detail: String,
    /// What somebody has said about this finding — acknowledged, snoozed or
    /// suppressed — or `None` while it is live. Never set by the rules: it is
    /// attached where a snapshot is projected for a caller
    /// (`Snapshot::filtered_with`), so the finding itself, the lifecycle
    /// verdict and the history are the same whatever anybody has said.
    pub triage: Option<crate::state::Triage>,
}

impl Flag {
    /// Live: nobody has acknowledged, snoozed or suppressed it.
    pub fn is_live(&self) -> bool {
        self.triage.is_none()
    }
}

/// Every rule `evaluate` can raise, by code — the one authoritative list.
///
/// A suppression names a rule before any machine need be raising it, so the
/// set of codes is checked against something other than the current fleet;
/// a typo there would otherwise be a rule that silently suppresses nothing.
/// Two tests hold it to `evaluate`: one reads `evaluate`'s own source and
/// fails on a code missing from here or listed here and never raised, and
/// `push` asserts every code it is handed is in it.
pub const RULES: [&str; 21] = [
    "grade_low",
    "cohort_outlier",
    "regressed",
    "component_weak",
    "battery_worn",
    "drive_failing",
    "drive_media_errors",
    "drive_worn",
    "disk_nearly_full",
    "firmware_behind_peers",
    "firmware_old",
    "thermal_limited",
    "on_battery",
    "power_limited",
    "partial_run",
    "ram_backed_disk",
    "unstable_run",
    "stale",
    "weak_identity",
    "reimage_resets_identity",
    "no_peer_group",
];

/// The rule a code names, as the `'static` the flags carry, or `None` for a
/// code no rule raises.
pub fn rule(code: &str) -> Option<&'static str> {
    RULES.iter().copied().find(|r| *r == code)
}

/// The configuration a run's numbers are only comparable within.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Comparability {
    pub preset: String,
    pub profile: String,
    pub baseline: String,
    pub build_isa: String,
}

impl Comparability {
    pub fn label(&self) -> String {
        format!(
            "{} · {} · {} · {}",
            self.preset, self.profile, self.baseline, self.build_isa
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentView {
    pub id: String,
    pub score: f64,
    pub grade: String,
    /// False for a component measured but deliberately kept out of the overall
    /// grade — `network` and `gpu`. It must not drive a machine-level flag.
    pub graded: bool,
    /// Percentage difference from the median of the same component across the
    /// machine's cohort, or `None` when fewer than `min_cohort` of them
    /// measured it. Worked out across the whole index, like the cohort's own
    /// median — see `components`.
    pub cohort_delta_pct: Option<f64>,
}

/// One drive's health counters, as recorded with a run. Everything but the
/// hours is optional: SATA drives can't fill the NVMe-only fields and a hard
/// disk has no wear figure, and an absent figure is never read as zero.
#[derive(Debug, Clone, Serialize)]
pub struct DriveView {
    pub position: i64,
    pub device: String,
    pub interface: String,
    pub model: Option<String>,
    pub power_on_hours: i64,
    pub power_cycles: Option<i64>,
    /// Indexed, shown, and deliberately not a rule — see `evaluate`.
    pub unsafe_shutdowns: Option<i64>,
    pub percentage_used: Option<i64>,
    pub available_spare_pct: Option<i64>,
    pub data_written_tb: Option<f64>,
    pub media_errors: Option<i64>,
    pub critical_warning: Option<i64>,
}

impl DriveView {
    /// What to call it in a finding: the model if the drive reported one, the
    /// OS's device name otherwise.
    fn name(&self) -> &str {
        self.model.as_deref().unwrap_or(&self.device)
    }

    fn from_row(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Self> {
        Ok(DriveView {
            position: r.get(at)?,
            device: r.get(at + 1)?,
            interface: r.get(at + 2)?,
            model: r.get(at + 3)?,
            power_on_hours: r.get(at + 4)?,
            power_cycles: r.get(at + 5)?,
            unsafe_shutdowns: r.get(at + 6)?,
            percentage_used: r.get(at + 7)?,
            available_spare_pct: r.get(at + 8)?,
            data_written_tb: r.get(at + 9)?,
            media_errors: r.get(at + 10)?,
            critical_warning: r.get(at + 11)?,
        })
    }
}

/// The columns `DriveView::from_row` reads, in its order.
const DRIVE_COLUMNS: &str = "d.position, d.device, d.interface, d.model, d.power_on_hours, \
     d.power_cycles, d.unsafe_shutdowns, d.percentage_used, d.available_spare_pct, \
     d.data_written_tb, d.media_errors, d.critical_warning";

/// One memory module, as recorded with a run (a loadbearer release after
/// 1.8.1). Every field is optional: firmware reports junk often enough that the
/// CLI leaves out anything unknown, and absent is never read as zero.
#[derive(Debug, Clone, Serialize)]
pub struct ModuleView {
    pub position: i64,
    pub size_bytes: Option<i64>,
    /// `DDR4`, `DDR5`, `LPDDR5`, … — named as the document names it.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub form_factor: Option<String>,
    /// Rated, and what it runs at. Shown side by side and judged by no rule:
    /// memory held below its rating is common, and usually the platform's
    /// limit rather than a fault.
    pub speed_mts: Option<i64>,
    pub configured_speed_mts: Option<i64>,
}

impl ModuleView {
    fn from_row(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Self> {
        Ok(ModuleView {
            position: r.get(at)?,
            size_bytes: r.get(at + 1)?,
            kind: r.get(at + 2)?,
            form_factor: r.get(at + 3)?,
            speed_mts: r.get(at + 4)?,
            configured_speed_mts: r.get(at + 5)?,
        })
    }
}

/// The columns `ModuleView::from_row` reads, in its order.
const MODULE_COLUMNS: &str = "mm.position, mm.size_bytes, mm.type, mm.form_factor, mm.speed_mts, \
     mm.configured_speed_mts";

/// NVMe's critical-warning bits, in words. Bits past these are reserved or
/// newer than this table, and are reported by number rather than guessed at.
fn critical_warning_words(bits: i64) -> Vec<String> {
    const MEANING: [&str; 5] = [
        "spare capacity below the drive's own threshold",
        "temperature outside its safe range",
        "reliability degraded by media or internal errors",
        "media switched to read-only",
        "volatile-memory backup failed",
    ];
    (0..8)
        .filter(|b| bits & (1 << b) != 0)
        .map(|b| match MEANING.get(b) {
            Some(m) => (*m).to_string(),
            None => format!("bit {b}"),
        })
        .collect()
}

/// A `YYYY-MM-DD` date, or `None` for anything else — firmware dates come
/// from SMBIOS, where vendors have been known to write anything.
fn parse_day(s: &str) -> Option<time::Date> {
    time::Date::parse(
        s.trim(),
        time::macros::format_description!("[year]-[month]-[day]"),
    )
    .ok()
}

/// A threshold as a finding quotes it: `15`, `7.5`, never `15.000000000000002`
/// — which is what 0.15 × 100 is in floating point.
fn figure(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// "12.4 GiB" for small amounts, where "12 GiB" and "0 GiB" would hide the
/// difference that matters on a nearly full disk; whole GiB above that.
fn free_bytes(bytes: i64) -> String {
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib < 10.0 {
        format!("{gib:.1} GiB")
    } else {
        format!("{gib:.0} GiB")
    }
}

/// The newest firmware seen on one system model.
#[derive(Debug, Clone)]
struct NewestFirmware {
    date: time::Date,
    version: Option<String>,
    /// How many machines of the model run firmware of that date.
    on: usize,
    /// How many machines of the model have a readable firmware date at all.
    dated: usize,
}

/// The newest firmware per system model, over models with at least two
/// machines carrying a readable date — one machine has nothing to be behind.
///
/// Built from every machine in the index, the same as `build_cohorts`, and
/// for the same reason: what counts as "the newest firmware on this model"
/// must not change with what the viewer happens to be filtered to, or one
/// machine would be behind in one view and current in another. The flag that
/// results names a version and a date — like a cohort median, a property of
/// the model across the estate — and never the machine it was seen on.
fn newest_firmware(machines: &[MachineView]) -> HashMap<&str, NewestFirmware> {
    let mut groups: BTreeMap<&str, Vec<(time::Date, Option<&str>)>> = BTreeMap::new();
    for m in machines {
        if let (Some(model), Some(date)) = (
            m.system_model.as_deref(),
            m.bios_date.as_deref().and_then(parse_day),
        ) {
            groups
                .entry(model)
                .or_default()
                .push((date, m.bios_version.as_deref()));
        }
    }
    groups
        .into_iter()
        .filter(|(_, seen)| seen.len() >= 2)
        .map(|(model, seen)| {
            let date = seen.iter().map(|s| s.0).max().expect("at least two");
            // Machines arrive ordered by key, so which version names a date
            // shared by two releases is stable between refreshes.
            let version = seen
                .iter()
                .find(|s| s.0 == date)
                .and_then(|s| s.1)
                .map(str::to_string);
            let on = seen.iter().filter(|s| s.0 == date).count();
            (
                model,
                NewestFirmware {
                    date,
                    version,
                    on,
                    dated: seen.len(),
                },
            )
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    pub key: String,
    /// Which identifier the runs were attributed by — `serial` down to
    /// `hostname`. Determines how much the history can be trusted.
    pub key_kind: String,
    pub hostname: Option<String>,
    pub cpu_model: String,
    pub cpu_cores: i64,
    pub ram_bytes: i64,
    pub os: Option<String>,
    pub arch: String,
    pub serial: Option<String>,
    pub asset_tag: Option<String>,
    pub tool_version: String,
    pub comparability: Comparability,
    pub score: Option<f64>,
    pub grade: String,
    pub taken_at: String,
    pub age_days: Option<f64>,
    /// How many runs this machine has in the index, this one included.
    pub runs: usize,
    pub first_seen: String,
    pub partial: bool,
    pub caveats: Vec<String>,
    pub thermal_limited: Option<bool>,
    pub on_ac: Option<bool>,
    pub battery_health: Option<f64>,
    pub battery_cycles: Option<i64>,
    /// "Dell Inc. Latitude 7440", from the firmware.
    pub system_model: Option<String>,
    /// The machine-age evidence loadbearer recorded — see `schema::Age`. All
    /// `None` for a run from before loadbearer 1.6.0.
    pub cpu_family: Option<String>,
    /// `YYYY-MM`: the machine was built this month or later.
    pub cpu_launched: Option<String>,
    /// Where `cpu_launched` and `cpu_family` came from. `None` when there
    /// is no launch date at all.
    pub age_source: Option<AgeSource>,
    /// Upper bound on the machine's age **at the run**, in months. For its age
    /// today, count from `cpu_launched`.
    pub max_age_months: Option<i64>,
    pub firmware_dated: Option<String>,
    pub firmware_likely_original: Option<bool>,
    pub drive_power_on_hours: Option<i64>,
    pub drive_wear_pct: Option<i64>,
    /// The drive's hours look like time spent working, not time in service —
    /// see `schema::Age::drive_hours_undercounted`.
    pub drive_hours_undercounted: Option<bool>,
    /// Upper bound on the machine's age **today**, in months: `cpu_launched`
    /// to the snapshot. The field to rank or compare machines by — unlike
    /// `max_age_months` it doesn't depend on when each was last measured.
    pub max_age_months_now: Option<i64>,
    /// The best estimate of the machine's age today, and what kind of
    /// estimate it is — see `best_age`. `None` when nothing dates it.
    pub age_estimate: Option<AgeEstimate>,
    /// The power plan's name (Windows), in the machine's language.
    pub power_plan: Option<String>,
    /// The power mode for the source the run was on (Windows), or the
    /// platform profile (Linux).
    pub power_mode: Option<String>,
    /// The settings in force that held the run back, battery power aside.
    /// Empty for a run from before loadbearer 1.7.0.
    pub power_held_back: Vec<String>,
    /// The firmware's own version string and its `YYYY-MM-DD` date, from
    /// loadbearer 1.6.0's `machine.firmware`. `None` for older runs and for
    /// runs indexed before this build until `scan --reindex`.
    pub bios_version: Option<String>,
    pub bios_date: Option<String>,
    /// The fixed volume with the lowest free percentage, its free share and
    /// its free bytes — see `schema::Machine::fullest_fixed_volume`.
    pub disk_low_mount: Option<String>,
    pub disk_low_free_pct: Option<f64>,
    pub disk_low_free_bytes: Option<i64>,
    /// The OS release in parts, from a loadbearer release after 1.8.1's
    /// `machine.os_detail`: on Windows the display version, the build with its
    /// update revision and the edition; on Linux the os-release ID, version
    /// and variant. `None` where the run didn't record it.
    pub os_distro: Option<String>,
    pub os_version: Option<String>,
    pub os_build: Option<String>,
    pub os_edition: Option<String>,
    /// Memory slots for system RAM, and how many are filled, from
    /// `machine.memory`. `None` is unknown — Linux without root, or an older
    /// CLI — and never "no slots".
    pub memory_slots: Option<i64>,
    pub memory_populated: Option<i64>,
    /// The distinct module types of the latest run, in slot order: `["DDR5"]`.
    /// Empty when no module recorded one.
    pub memory_types: Vec<String>,
    /// Every module of the latest run. Not serialised, like `drives`: the
    /// drilldown gets them from `detail`.
    #[serde(skip)]
    pub memory_modules: Vec<ModuleView>,
    /// The latest run's random ID (`run.id`, not this index's `run_id`) and
    /// how long its benchmarks took.
    pub run_uuid: Option<String>,
    pub run_duration_secs: Option<f64>,
    /// What the latest run was asked to collect with `--collect` — `uptime`,
    /// `elevation`, `security`, sorted — and empty for a run that asked for
    /// nothing, which every run from an older loadbearer is. **This is what
    /// gives every field below its meaning**: one that is `None` is "not
    /// asked" when its category is missing here and "asked, could not be
    /// read" when it is listed. Neither is ever "off" or "no".
    pub run_collect: Vec<String>,
    /// Whether the latest run had administrator / root rights (`elevation`).
    pub run_elevated: Option<bool>,
    /// `uptime`: when the machine last booted and how long it had been up,
    /// **as of the run** — not now. On Windows with Fast Startup, since the
    /// last restart.
    pub booted_at: Option<String>,
    pub uptime_secs: Option<i64>,
    /// `security`: `uefi` / `bios` or a value this build has not heard of.
    pub firmware_mode: Option<String>,
    /// `None` with `firmware_mode` `bios` is a legacy-BIOS boot, where there
    /// is no Secure Boot to be on or off.
    pub secure_boot: Option<bool>,
    /// `false` is "the OS exposes no TPM": none fitted, off in firmware, or no
    /// driver. `tpm_version` is `2.0` / `1.2`, `None` where the OS didn't say.
    pub tpm_present: Option<bool>,
    pub tpm_version: Option<String>,
    /// Linux only, root filesystem only: `encrypted` (dm-crypt, with the
    /// cryptsetup type in `disk_encryption_method`) or `none_detected` — no
    /// dm-crypt under `/`, which is **not** "unencrypted". Never set for
    /// Windows, where the CLI cannot read BitLocker.
    pub disk_encryption: Option<String>,
    pub disk_encryption_method: Option<String>,
    /// Every drive of the latest run, for the rules. Not serialised: the
    /// snapshot ships every machine, and the drilldown, which wants these,
    /// gets them from `detail` instead.
    #[serde(skip)]
    pub drives: Vec<DriveView>,
    /// The highest `percentage_used` across every drive of the latest run, or
    /// the boot drive's from the age evidence where no drive table was
    /// recorded. The one drive figure the snapshot carries, since `drives`
    /// itself stays out of it.
    pub worst_drive_wear_pct: Option<i64>,
    pub low_confidence_cpu_subtests: i64,
    pub tags: BTreeMap<String, String>,
    pub components: Vec<ComponentView>,
    pub cohort: String,
    /// Percentage difference from the cohort median. Negative is below.
    pub cohort_delta_pct: Option<f64>,
    /// Percentage difference from the median of this machine's own earlier
    /// comparable runs. Negative is a regression.
    pub trend_pct: Option<f64>,
    /// What to do about the machine. Set by `assess`, from its findings.
    pub lifecycle: Lifecycle,
    /// The lines this machine is judged against: the fleet-wide thresholds
    /// with whatever overrides pick it applied. Set by `assess`, and read by
    /// every rule that judges one machine, so none can fall back to the
    /// fleet-wide value for a machine an override has changed. Not
    /// serialised: `overridden_by` says what differs, and the snapshot carries
    /// the fleet-wide values.
    #[serde(skip)]
    pub thresholds: Arc<Thresholds>,
    /// The overrides that changed how this machine was judged, each with the
    /// thresholds it supplied. Empty when the fleet-wide lines apply.
    pub overridden_by: Vec<AppliedOverride>,
    pub run_id: i64,
    pub source_path: String,
}

/// What a machine's age estimate rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgeBasis {
    /// The CPU's launch: the machine cannot be older, and may be years
    /// younger — a CPU stays on sale long after it launches.
    UpperBound,
    /// Firmware loadbearer judged to be as shipped: the machine dates from
    /// about then. Closer than the bound, and only ever inside it.
    Firmware,
}

/// The best estimate of how old a machine is today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AgeEstimate {
    pub months: i64,
    pub basis: AgeBasis,
}

impl AgeEstimate {
    pub fn years(&self) -> f64 {
        self.months as f64 / 12.0
    }

    /// "At most 6.2 years old, by its CPU's launch date" — the wording says
    /// which kind of figure it is, because a bound read as a dating makes a
    /// three-year-old machine sound six.
    pub fn sentence(&self) -> String {
        match self.basis {
            AgeBasis::UpperBound => format!(
                "At most {:.1} years old, by its CPU's launch date",
                self.years()
            ),
            AgeBasis::Firmware => format!(
                "About {:.1} years old, by its original firmware's date",
                self.years()
            ),
        }
    }
}

/// The best estimate of a machine's age at `now` that the evidence supports.
///
/// Two sources, and they are different kinds of answer. The CPU's launch is an
/// **upper bound**: the machine was built that month or later. Firmware that
/// loadbearer judged likely to be as shipped is a **dating**: the machine is
/// about that old. The dating wins when there is one — it is the closer of the
/// two — but only inside the bound, since a machine cannot predate its own CPU
/// and a firmware date that says otherwise is the firmware being wrong. With
/// neither, there is no estimate at all; nothing here guesses.
///
/// The firmware's month comes from the age evidence where loadbearer recorded
/// it, and from the BIOS date otherwise; "likely original" is only ever
/// recorded alongside the first, so in practice that is what dates a machine.
pub fn best_age(
    cpu_launched: Option<&str>,
    firmware_dated: Option<&str>,
    firmware_likely_original: Option<bool>,
    now: OffsetDateTime,
) -> Option<AgeEstimate> {
    let bound = cpu_launched.and_then(|l| months_since(l, now));
    let dated = firmware_dated
        .filter(|_| firmware_likely_original == Some(true))
        .and_then(|d| months_since(d, now))
        .filter(|d| bound.is_none_or(|b| *d <= b));
    match (dated, bound) {
        (Some(months), _) => Some(AgeEstimate {
            months,
            basis: AgeBasis::Firmware,
        }),
        (None, Some(months)) => Some(AgeEstimate {
            months,
            basis: AgeBasis::UpperBound,
        }),
        (None, None) => None,
    }
}

/// The decision a machine calls for. Ordered most to least urgent, which is
/// the order every list of them is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Replace,
    Repair,
    Investigate,
    Watch,
    #[default]
    Keep,
}

impl Verdict {
    pub const ALL: [Verdict; 5] = [
        Verdict::Replace,
        Verdict::Repair,
        Verdict::Investigate,
        Verdict::Watch,
        Verdict::Keep,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Replace => "replace",
            Verdict::Repair => "repair",
            Verdict::Investigate => "investigate",
            Verdict::Watch => "watch",
            Verdict::Keep => "keep",
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Verdict {
    type Err = String;

    /// Refused rather than read as "no filter": a misspelt decision answered
    /// with the whole fleet, or with nothing, would read as an answer.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Verdict::ALL
            .into_iter()
            .find(|v| v.as_str() == s.trim())
            .ok_or_else(|| {
                format!(
                    "{s:?} is not a lifecycle decision. Use replace, repair, investigate, watch \
                     or keep"
                )
            })
    }
}

/// One security fact a machine's latest run **read**, for `posture=` — the
/// question "which machines have Secure Boot off" asked of the estate.
///
/// Each value matches only machines where the fact was read: a machine whose
/// run did not ask for `security`, or asked and could not read that part,
/// matches neither `secure_boot_on` nor `secure_boot_off`. That is the point:
/// "not collected" counted as "off" would turn a fleet that never opted in
/// into a list of unprotected machines. A legacy-BIOS boot has no Secure Boot
/// at all and is its own value rather than either side.
///
/// One value at a time, and deliberately no findings or metrics drawn from
/// it — see the README's "Uptime, elevation and security posture".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Posture {
    SecureBootOn,
    SecureBootOff,
    LegacyBios,
    TpmPresent,
    TpmAbsent,
    Tpm12,
    DiskEncrypted,
    DiskNoneDetected,
}

impl Posture {
    pub const ALL: [Posture; 8] = [
        Posture::SecureBootOff,
        Posture::SecureBootOn,
        Posture::LegacyBios,
        Posture::TpmAbsent,
        Posture::TpmPresent,
        Posture::Tpm12,
        Posture::DiskNoneDetected,
        Posture::DiskEncrypted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Posture::SecureBootOn => "secure_boot_on",
            Posture::SecureBootOff => "secure_boot_off",
            Posture::LegacyBios => "legacy_bios",
            Posture::TpmPresent => "tpm_present",
            Posture::TpmAbsent => "tpm_absent",
            Posture::Tpm12 => "tpm_1.2",
            Posture::DiskEncrypted => "disk_encrypted",
            Posture::DiskNoneDetected => "disk_none_detected",
        }
    }

    /// Whether `m`'s latest run read this. Every arm needs a value the index
    /// holds only for a run that asked for `security`, so none can match a
    /// machine that was not asked.
    pub fn holds(self, m: &MachineView) -> bool {
        match self {
            Posture::SecureBootOn => m.secure_boot == Some(true),
            Posture::SecureBootOff => m.secure_boot == Some(false),
            Posture::LegacyBios => m.firmware_mode.as_deref() == Some("bios"),
            Posture::TpmPresent => m.tpm_present == Some(true),
            Posture::TpmAbsent => m.tpm_present == Some(false),
            Posture::Tpm12 => {
                m.tpm_present != Some(false) && m.tpm_version.as_deref() == Some("1.2")
            }
            Posture::DiskEncrypted => m.disk_encryption.as_deref() == Some("encrypted"),
            Posture::DiskNoneDetected => m.disk_encryption.as_deref() == Some("none_detected"),
        }
    }
}

impl std::fmt::Display for Posture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Posture {
    type Err = String;

    /// Refused rather than read as "no filter", as a lifecycle decision is:
    /// a misspelt `secure_boot_of` answered with an empty fleet would read as
    /// "every machine has Secure Boot on".
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Posture::ALL
            .into_iter()
            .find(|p| p.as_str() == s.trim())
            .ok_or_else(|| {
                format!(
                    "{s:?} is not a security posture. Use {}",
                    Posture::ALL.map(Posture::as_str).join(", ")
                )
            })
    }
}

/// A machine's lifecycle verdict and why.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Lifecycle {
    pub verdict: Verdict,
    /// One short sentence per piece of evidence, the ones that decided the
    /// verdict first. Empty for `keep`, which by construction has none.
    pub reasons: Vec<String>,
    /// The verdict leans on a performance figure from a run that carries a
    /// measurement caveat: re-measure before acting on it.
    pub uncertain: bool,
    /// Those caveats, by headline. Empty unless `uncertain`.
    pub caveats: Vec<String>,
}

/// Findings that name a part: something to fix or swap rather than a reason
/// to retire the platform. Not a nearly full disk, which is housekeeping, and
/// not firmware, which is an update.
const PART_FINDINGS: [&str; 5] = [
    "battery_worn",
    "drive_failing",
    "drive_media_errors",
    "drive_worn",
    "component_weak",
];

/// The one part finding that comes from a benchmark figure rather than from
/// the part reporting on itself — see `Evidence::verdict`.
const MEASURED_PART: &str = "component_weak";

/// Findings that say the machine is slower than its own hardware should be.
const UNDERPERFORMING: [&str; 2] = ["cohort_outlier", "regressed"];

/// What a lifecycle verdict is drawn from. The findings rather than the
/// figures behind them, so each threshold lives in one place — `evaluate` —
/// and a verdict can never disagree with the finding it quotes.
struct Evidence<'a> {
    /// `grade_low` at critical: an F.
    failing: Option<&'a Flag>,
    /// `grade_low` at warning: a D.
    low: Option<&'a Flag>,
    old: Option<AgeEstimate>,
    parts: Vec<&'a Flag>,
    under: Vec<&'a Flag>,
    /// Measurement-queue findings that need attention.
    caveats: Vec<&'a Flag>,
}

impl Evidence<'_> {
    /// The rule. `performance: false` judges the machine as if no performance
    /// figure existed, which is how `uncertain` asks whether one mattered.
    fn verdict(&self, performance: bool) -> Verdict {
        let failing = performance && self.failing.is_some();
        let slow = failing || (performance && self.low.is_some());
        let under = performance && !self.under.is_empty();
        let old = self.old.is_some();
        // A weak component is a part finding read off the benchmark: the
        // drive's own counters and the battery's capacity stand whatever the
        // run measured, but a disk graded F on a throttled or partial run is
        // the same doubtful figure as a low overall grade.
        let parts = self
            .parts
            .iter()
            .any(|f| performance || f.code != MEASURED_PART);
        if failing || (old && (slow || parts)) {
            Verdict::Replace
        } else if parts {
            Verdict::Repair
        } else if under {
            Verdict::Investigate
        } else if old || slow {
            Verdict::Watch
        } else {
            Verdict::Keep
        }
    }
}

/// A machine's lifecycle verdict, from its own findings and its age.
fn lifecycle(m: &MachineView, flags: &[&Flag]) -> Lifecycle {
    let th = &m.thresholds;
    let grade = flags.iter().copied().find(|f| f.code == "grade_low");
    let e = Evidence {
        failing: grade.filter(|f| f.severity == Severity::Critical),
        low: grade.filter(|f| f.severity != Severity::Critical),
        old: m
            .age_estimate
            .filter(|a| a.months as f64 >= th.replace_age_years * 12.0),
        parts: flags
            .iter()
            .copied()
            .filter(|f| PART_FINDINGS.contains(&f.code))
            .collect(),
        under: flags
            .iter()
            .copied()
            .filter(|f| UNDERPERFORMING.contains(&f.code))
            .collect(),
        caveats: flags
            .iter()
            .copied()
            .filter(|f| f.kind == FlagKind::Measurement && needs_attention(f.severity))
            .collect(),
    };
    let verdict = e.verdict(true);

    // Every piece of evidence present, in one fixed order, with whichever
    // decided the verdict moved to the front: the reader sees why first, and
    // then what else is true of the machine — a worn battery is worth knowing
    // about on a machine being investigated, if only so it is fixed at the
    // same visit.
    let headline = |f: &&Flag| f.headline.clone();
    let mut groups: Vec<(u8, Vec<String>)> = vec![
        (0, e.failing.iter().map(headline).collect()),
        (1, e.old.iter().map(AgeEstimate::sentence).collect()),
        (2, e.low.iter().map(headline).collect()),
        (3, e.parts.iter().map(headline).collect()),
        (4, e.under.iter().map(headline).collect()),
    ];
    let first = match verdict {
        Verdict::Replace if e.failing.is_some() => 0,
        Verdict::Replace | Verdict::Watch if e.old.is_some() => 1,
        Verdict::Watch => 2,
        Verdict::Repair => 3,
        Verdict::Investigate => 4,
        _ => 0,
    };
    groups.sort_by_key(|(g, _)| *g != first);
    let reasons = groups.into_iter().flat_map(|(_, r)| r).collect();

    let uncertain = !e.caveats.is_empty() && e.verdict(false) != verdict;
    Lifecycle {
        verdict,
        reasons,
        uncertain,
        caveats: if uncertain {
            e.caveats.iter().map(headline).collect()
        } else {
            Vec::new()
        },
    }
}

/// Machines per lifecycle verdict. Always adds up to the machines it was
/// counted over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LifecycleCounts {
    pub replace: usize,
    pub repair: usize,
    pub investigate: usize,
    pub watch: usize,
    pub keep: usize,
}

impl LifecycleCounts {
    fn of<'a>(machines: impl IntoIterator<Item = &'a MachineView>) -> Self {
        let mut c = Self::default();
        for m in machines {
            *match m.lifecycle.verdict {
                Verdict::Replace => &mut c.replace,
                Verdict::Repair => &mut c.repair,
                Verdict::Investigate => &mut c.investigate,
                Verdict::Watch => &mut c.watch,
                Verdict::Keep => &mut c.keep,
            } += 1;
        }
        c
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Cohort {
    pub id: String,
    pub cpu_model: String,
    pub comparability: Comparability,
    pub members: usize,
    /// How many of the cohort's members survive the current filter. The
    /// statistics beside it do **not** narrow with the filter — see
    /// `Snapshot::filtered`.
    pub in_view: usize,
    pub median: f64,
    pub mad: f64,
    /// The floored dispersion the z-score actually divides by.
    pub scale: f64,
    pub p10: f64,
    pub p90: f64,
    /// True when the cohort is large enough for its median to be worth
    /// comparing a member against.
    pub comparable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub machines: usize,
    pub runs: usize,
    /// Grade histogram, best first, with any grade this build doesn't
    /// recognise last.
    pub grades: Vec<(String, usize)>,
    pub median_score: Option<f64>,
    pub p10_score: Option<f64>,
    pub p90_score: Option<f64>,
    /// Live findings — nobody has acknowledged, snoozed or suppressed them —
    /// by severity. Quieted ones are counted in `quieted` instead.
    pub critical: usize,
    pub warnings: usize,
    pub info: usize,
    /// Machines carrying at least one live critical or warning flag. Always
    /// smaller than the flag count, and the more honest headline of the two.
    pub machines_flagged: usize,
    /// Findings somebody has quieted, by how. Still findings, still in
    /// `flags`, still behind every lifecycle verdict; only not news.
    pub quieted: QuietCounts,
    pub stale: usize,
    pub partial: usize,
    pub thermally_limited: usize,
    /// Machines keyed on an identifier that does not survive a reimage.
    pub weak_identity: usize,
    pub cohorts: usize,
    pub comparable_cohorts: usize,
    /// Machines with no peer group large enough to compare against.
    pub uncohorted: usize,
    pub newest_run: Option<String>,
    pub oldest_run: Option<String>,
    /// Configurations present across the fleet. More than one means part of the
    /// estate cannot be compared with the rest.
    pub configurations: Vec<(String, usize)>,
    /// Machines per lifecycle verdict; adds up to `machines`.
    pub lifecycle: LifecycleCounts,
}

/// Quieted findings by how they were quieted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct QuietCounts {
    pub acknowledged: usize,
    pub snoozed: usize,
    pub suppressed: usize,
}

impl QuietCounts {
    fn of<'a>(flags: impl IntoIterator<Item = &'a Flag>) -> Self {
        use crate::state::TriageState;
        let mut c = Self::default();
        for t in flags.into_iter().filter_map(|f| f.triage.as_ref()) {
            *match t.state {
                TriageState::Acknowledged => &mut c.acknowledged,
                TriageState::Snoozed => &mut c.snoozed,
                TriageState::Suppressed => &mut c.suppressed,
            } += 1;
        }
        c
    }

    pub fn total(&self) -> usize {
        self.acknowledged + self.snoozed + self.suppressed
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// Which build produced this. It travels with the snapshot rather than
    /// being asked for separately, so that a saved `report --json` — or one
    /// diffed against another server's — is attributable to a build, and so
    /// the dashboard can show it without a second request.
    pub version: &'static str,
    /// The commit this was built from, `-dirty` if the tree wasn't clean, or
    /// `unknown` outside a repository. `version` alone can't identify a build:
    /// every commit between two releases reports the same one.
    pub build: &'static str,
    pub generated_at: String,
    /// The fleet-wide thresholds: what a machine is judged against unless an
    /// override picks it.
    pub thresholds: Thresholds,
    /// The `[[thresholds.overrides]]` rules, in the order they are tried. Each
    /// machine's `overridden_by` names the ones that changed it.
    pub threshold_overrides: Vec<Override>,
    pub summary: Summary,
    pub machines: Vec<MachineView>,
    pub cohorts: Vec<Cohort>,
    pub flags: Vec<Flag>,
}

/// One row of the slim history query.
struct RunRow {
    id: i64,
    machine_key: String,
    taken_at: String,
    /// `taken_at` as an instant, for placing the run in time. `None` when it
    /// does not parse — and then it can't be shown to precede anything, so a
    /// reconstruction of the past leaves it out. See the module notes.
    at: Option<OffsetDateTime>,
    score: Option<f64>,
    cmp: Comparability,
}

const GRADES: [&str; 6] = ["S", "A", "B", "C", "D", "F"];

/// Rank a grade best-to-worst, or `None` for a grade this build has never heard
/// of. An unrecognised grade is reported as unrecognised rather than guessed
/// at — the same open-enumeration rule the schema reader follows.
fn grade_rank(g: &str) -> Option<usize> {
    GRADES.iter().position(|k| *k == g)
}

fn median(sorted: &[f64]) -> f64 {
    match sorted.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    }
}

/// Linear-interpolated quantile over an already-sorted slice.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = q * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

/// Finite values only, ascending. A `NaN` score in the index would otherwise
/// make the sort order undefined and the median arbitrary.
fn sorted_of(values: impl IntoIterator<Item = f64>) -> Vec<f64> {
    let mut v: Vec<f64> = values.into_iter().filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).expect("filtered to finite"));
    v
}

/// Median absolute deviation from the median.
fn mad(sorted: &[f64], med: f64) -> f64 {
    let devs = sorted_of(sorted.iter().map(|x| (x - med).abs()));
    median(&devs)
}

/// A run's timestamp as an instant: the one parser for "when was this
/// measured", so staleness and the reconstruction of the past can never
/// disagree about whether a timestamp is readable.
///
/// Parsed rather than compared as text. loadbearer writes UTC with a `Z` and
/// as many fractional digits as the clock gave — none, seven, nine — and
/// `"…:25Z"` sorts *after* `"…:25.5Z"` as a string, because `.` is below `Z`.
/// A document that arrived another way may carry an offset, and
/// `"…T10:00:00+05:00"` sorts five hours away from where it belongs.
fn instant(taken_at: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(taken_at, &Rfc3339).ok()
}

fn age_days(taken_at: &str, now: OffsetDateTime) -> Option<f64> {
    let then = instant(taken_at)?;
    Some((now - then).as_seconds_f64() / 86_400.0)
}

/// Where a machine's launch date came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgeSource {
    /// loadbearer recorded it in the run's `machine.age` (1.6.0+).
    Recorded,
    /// The run predates that, so this build worked it out from `cpu_model`
    /// with its copy of the launch table — see `launch`.
    Derived,
}

/// The CPU's launch month and family for a run: what loadbearer recorded if
/// it recorded anything, otherwise derived from the CPU model.
///
/// A run that recorded an age block but no launch (a CPU loadbearer didn't
/// know) is still derived from here, so a correction to this table reaches it
/// too.
pub fn resolve_launch(
    recorded: Option<String>,
    recorded_family: Option<String>,
    cpu_model: &str,
) -> (Option<String>, Option<String>, Option<AgeSource>) {
    if recorded.is_some() {
        return (recorded, recorded_family, Some(AgeSource::Recorded));
    }
    match crate::launch::cpu_launch(cpu_model) {
        Some(l) => (
            Some(l.year_month()),
            Some(l.family),
            Some(AgeSource::Derived),
        ),
        None => (None, None, None),
    }
}

/// Whole months from a `YYYY-MM` to `now`; `None` if unreadable or in the
/// future.
pub fn months_since(year_month: &str, now: OffsetDateTime) -> Option<i64> {
    let year: i64 = year_month.get(0..4)?.parse().ok()?;
    let month: i64 = year_month.get(5..7)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let months = (now.year() as i64 * 12 + u8::from(now.month()) as i64) - (year * 12 + month);
    (months >= 0).then_some(months)
}

fn pct_delta(value: f64, reference: f64) -> Option<f64> {
    (reference.abs() > f64::EPSILON).then(|| (value - reference) / reference * 100.0)
}

fn human_bytes(bytes: i64) -> String {
    format!("{:.0} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn cohort_id(cpu_model: &str, cmp: &Comparability) -> String {
    format!(
        "{cpu_model}|{}|{}|{}|{}",
        cmp.preset, cmp.profile, cmp.baseline, cmp.build_isa
    )
}

/// Compute the whole fleet view from the index.
///
/// `now` is a parameter rather than read from the clock so the staleness rules
/// are testable without waiting.
///
/// Every run the index holds is in scope, however it is dated — including one
/// stamped in the future by a machine whose clock is wrong. A reconstruction
/// of the past (`reconstruct`) only admits runs at or before its cutoff; the
/// view of *now* deliberately does not, because a machine that vanished from
/// the dashboard for having a bad clock would be a worse answer than one shown
/// with a date that looks odd.
pub fn snapshot(conn: &Connection, th: &Tuning, now: OffsetDateTime) -> Result<Snapshot> {
    let history = load_history(conn)?;
    let own = by_machine(&history);
    let mut machines = load_runs(conn, &Pick::Latest)?;
    for m in &mut machines {
        let runs: Vec<&RunRow> = own
            .get(m.key.as_str())
            .map(|rs| rs.iter().collect())
            .unwrap_or_default();
        place(m, &runs, now);
    }
    assess(machines, th, now)
}

/// The fleet as it was known at each of `cutoffs`, oldest first, handed to
/// `each` one at a time so only one reconstruction is ever held at once.
///
/// For each machine, its latest run taken at or before the cutoff, judged with
/// the cutoff as "now" and with only runs at or before it as its history — the
/// same `place` and `assess` the live snapshot goes through, so the past is
/// judged by exactly today's rules. A machine with no run by then is absent.
///
/// "Latest" here is chronological — the parsed instant, then the row id — not
/// the stored text the live snapshot orders by; see `instant` for why the two
/// can differ. On what loadbearer itself writes they agree.
///
/// The wide rows are loaded per cutoff but only for runs not already loaded
/// for the previous one: from one week to the next most machines are standing
/// on the same run, so the whole walk reads each run about once and holds at
/// most two cutoffs' worth.
fn reconstruct(
    conn: &Connection,
    th: &Tuning,
    cutoffs: &[OffsetDateTime],
    mut each: impl FnMut(OffsetDateTime, Snapshot) -> Result<()>,
) -> Result<()> {
    let history = load_history(conn)?;
    let own = by_machine(&history);
    let mut loaded: HashMap<i64, MachineView> = HashMap::new();

    for &cutoff in cutoffs {
        // Per machine, the runs known by the cutoff and the one standing for
        // it. Machines in key order, which is the order `load_runs` returns
        // them in and the order everything downstream expects.
        let known: Vec<(Vec<&RunRow>, i64)> = own
            .values()
            .filter_map(|runs| {
                let by_then: Vec<&RunRow> = runs
                    .iter()
                    .filter(|r| r.at.is_some_and(|at| at <= cutoff))
                    .collect();
                let pick = by_then.iter().max_by_key(|r| (r.at, r.id))?.id;
                Some((by_then, pick))
            })
            .collect();

        let missing: Vec<i64> = known
            .iter()
            .map(|(_, id)| *id)
            .filter(|id| !loaded.contains_key(id))
            .collect();
        if !missing.is_empty() {
            for m in load_runs(conn, &Pick::Runs(&missing))? {
                loaded.insert(m.run_id, m);
            }
        }

        let mut machines = Vec::with_capacity(known.len());
        for (runs, id) in &known {
            let mut m = loaded.get(id).expect("loaded above").clone();
            place(&mut m, runs, cutoff);
            machines.push(m);
        }
        let standing: HashSet<i64> = known.iter().map(|(_, id)| *id).collect();
        loaded.retain(|id, _| standing.contains(id));

        each(cutoff, assess(machines, th, cutoff)?)?;
    }
    Ok(())
}

/// Cohorts, deltas, findings and the summary, over machines already placed in
/// time. The half of a snapshot that is pure rules: everything here sees only
/// what it is handed, which is what lets a reconstruction of the past share it.
///
/// Each machine's own thresholds are resolved first, from the run being
/// judged, so the live snapshot and every week of the timeline choose a
/// machine's lines the same way. The cohort statistics take the fleet-wide
/// values, which no override can change.
fn assess(
    mut machines: Vec<MachineView>,
    tuning: &Tuning,
    now: OffsetDateTime,
) -> Result<Snapshot> {
    tuning.apply(&mut machines);
    let th = &tuning.fleet;
    let cohorts = build_cohorts(&machines, th);

    let by_id: HashMap<&str, &Cohort> = cohorts.iter().map(|c| (c.id.as_str(), c)).collect();
    for m in &mut machines {
        if let (Some(score), Some(c)) = (m.score, by_id.get(m.cohort.as_str()))
            && c.comparable
        {
            m.cohort_delta_pct = pct_delta(score, c.median);
        }
    }
    components::place_cohort_deltas(&mut machines, th.min_cohort);

    let firmware = newest_firmware(&machines);
    let flags = evaluate(&machines, &by_id, &firmware, th);

    let mut flags_of: HashMap<&str, Vec<&Flag>> = HashMap::new();
    for f in &flags {
        flags_of.entry(f.machine_key.as_str()).or_default().push(f);
    }
    for m in &mut machines {
        let own = flags_of.get(m.key.as_str()).map_or(&[][..], Vec::as_slice);
        m.lifecycle = lifecycle(m, own);
    }

    let summary = summarise(&machines, &cohorts, &flags);

    Ok(Snapshot {
        version: env!("CARGO_PKG_VERSION"),
        build: env!("FLEET_BUILD"),
        generated_at: now.format(&Rfc3339)?,
        thresholds: th.clone(),
        threshold_overrides: tuning.overrides.clone(),
        summary,
        machines,
        cohorts,
        flags,
    })
}

/// Every run, slim, for run counts and per-machine trend. The wide columns are
/// only fetched for the run standing for each machine.
fn load_history(conn: &Connection) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, machine_key, taken_at, overall_score, preset, profile, baseline, build_isa
           FROM run ORDER BY machine_key, taken_at, id",
    )?;
    let rows = stmt.query_map([], |r| {
        let taken_at: String = r.get(2)?;
        Ok(RunRow {
            id: r.get(0)?,
            machine_key: r.get(1)?,
            at: instant(&taken_at),
            taken_at,
            score: r.get(3)?,
            cmp: Comparability {
                preset: r.get(4)?,
                profile: r.get(5)?,
                baseline: r.get(6)?,
                build_isa: r.get(7)?,
            },
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A machine's runs, in the order `load_history` returns them — `taken_at` as
/// stored, then id. Keyed in machine-key order, which is the order `load_runs`
/// returns machines in.
fn by_machine(history: &[RunRow]) -> BTreeMap<&str, &[RunRow]> {
    history
        .chunk_by(|a, b| a.machine_key == b.machine_key)
        .map(|runs| (runs[0].machine_key.as_str(), runs))
        .collect()
}

/// Which run stands for each machine.
enum Pick<'a> {
    /// The newest per machine by `taken_at` as stored, ties broken on row id
    /// so two files written in the same instant resolve the same way every
    /// time rather than making the dashboard flicker between them. What the
    /// live snapshot has always shown.
    Latest,
    /// Exactly these runs, chosen by the caller — a reconstruction of the past.
    Runs(&'a [i64]),
}

impl Pick<'_> {
    /// A subquery yielding the chosen run ids as `id`, and what it binds.
    ///
    /// The ids go in as one JSON array rather than as a placeholder each: a
    /// fleet of thousands would pass SQLite's limit on bound parameters, and
    /// `json_each` keeps the statement the same text whatever the count.
    fn sql(&self) -> (&'static str, Vec<String>) {
        const LATEST: &str = "SELECT id FROM (SELECT id, ROW_NUMBER() OVER \
             (PARTITION BY machine_key ORDER BY taken_at DESC, id DESC) rn FROM run) WHERE rn = 1";
        match self {
            Pick::Latest => (LATEST, Vec::new()),
            Pick::Runs(ids) => (
                "SELECT value AS id FROM json_each(?1)",
                vec![format!(
                    "[{}]",
                    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
                )],
            ),
        }
    }
}

/// The chosen run per machine, with everything the views need that belongs to
/// the run itself. What depends on *when* it is being looked at from — age,
/// run count, trend — is left for `place`, so one loaded run can stand for its
/// machine at several points in time.
fn load_runs(conn: &Connection, pick: &Pick<'_>) -> Result<Vec<MachineView>> {
    let (chosen, args) = pick.sql();
    let bind = || rusqlite::params_from_iter(args.iter());

    let mut tags: HashMap<i64, BTreeMap<String, String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT t.run_id, t.key, t.value FROM tag t JOIN ({chosen}) l ON l.id = t.run_id"
        ))?;
        for row in stmt.query_map(bind(), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (id, k, v) = row?;
            tags.entry(id).or_default().insert(k, v);
        }
    }

    let mut components: HashMap<i64, Vec<ComponentView>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT c.run_id, c.id, c.score, c.grade, c.graded FROM component c \
             JOIN ({chosen}) l ON l.id = c.run_id ORDER BY c.id"
        ))?;
        for row in stmt.query_map(bind(), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                ComponentView {
                    id: r.get(1)?,
                    score: r.get(2)?,
                    grade: r.get(3)?,
                    graded: r.get(4)?,
                    // Set by `assess`, against the cohort.
                    cohort_delta_pct: None,
                },
            ))
        })? {
            let (id, c) = row?;
            components.entry(id).or_default().push(c);
        }
    }

    let mut drives: HashMap<i64, Vec<DriveView>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT d.run_id, {DRIVE_COLUMNS} FROM drive d \
             JOIN ({chosen}) l ON l.id = d.run_id ORDER BY d.run_id, d.position"
        ))?;
        for row in stmt.query_map(bind(), |r| {
            Ok((r.get::<_, i64>(0)?, DriveView::from_row(r, 1)?))
        })? {
            let (id, d) = row?;
            drives.entry(id).or_default().push(d);
        }
    }

    let mut modules: HashMap<i64, Vec<ModuleView>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT mm.run_id, {MODULE_COLUMNS} FROM memory_module mm \
             JOIN ({chosen}) l ON l.id = mm.run_id ORDER BY mm.run_id, mm.position"
        ))?;
        for row in stmt.query_map(bind(), |r| {
            Ok((r.get::<_, i64>(0)?, ModuleView::from_row(r, 1)?))
        })? {
            let (id, m) = row?;
            modules.entry(id).or_default().push(m);
        }
    }

    // loadbearer's own confidence verdict, not a cv threshold re-derived here:
    // it already folds in the thermal downgrade, so a run whose clocks fell
    // counts as shaky without this module having to decide that a second time.
    // Restricted to `cpu` — see `Thresholds::unstable_cpu_subtests`.
    let mut low_conf: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT s.run_id, COUNT(*) FROM subtest s JOIN ({chosen}) l ON l.id = s.run_id \
             WHERE s.confidence = 'low' AND s.component = 'cpu' GROUP BY s.run_id"
        ))?;
        for row in stmt.query_map(bind(), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (id, n) = row?;
            low_conf.insert(id, n);
        }
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT id, machine_key, key_kind, hostname, taken_at, tool_version, preset, profile,
                baseline, build_isa, overall_score, overall_grade, cpu_model, cpu_cores,
                ram_bytes, os, arch, serial, asset_tag, on_ac, thermal_limited,
                battery_health, battery_cycles, partial, caveats, source_path,
                system_model, cpu_family, cpu_launched, max_age_months, firmware_dated,
                firmware_likely_original, drive_power_on_hours, drive_wear_pct,
                drive_hours_undercounted, power_plan, power_mode, power_held_back,
                bios_version, bios_date, disk_low_mount, disk_low_free_pct,
                disk_low_free_bytes, os_distro, os_version, os_build, os_edition,
                memory_slots, memory_populated, run_uuid, run_duration_secs, run_collect,
                run_elevated, booted_at, uptime_secs, firmware_mode, secure_boot,
                tpm_present, tpm_version, disk_encryption, disk_encryption_method
           FROM run WHERE id IN ({chosen}) ORDER BY machine_key"
    ))?;

    let mut out = Vec::new();
    for row in stmt.query_map(bind(), |r| {
        let run_id: i64 = r.get(0)?;
        let key: String = r.get(1)?;
        let taken_at: String = r.get(4)?;
        let cmp = Comparability {
            preset: r.get(6)?,
            profile: r.get(7)?,
            baseline: r.get(8)?,
            build_isa: r.get(9)?,
        };
        let caveats: String = r.get(24)?;
        let cpu_model: String = r.get(12)?;
        let (cpu_launched, cpu_family, age_source) =
            resolve_launch(r.get(28)?, r.get(27)?, &cpu_model);
        // At the run, for a derived launch too, so the field means the same
        // thing whichever way the launch was found.
        let max_age_months: Option<i64> = match age_source {
            Some(AgeSource::Derived) => cpu_launched
                .as_deref()
                .and_then(|l| months_since(l, instant(&taken_at)?)),
            _ => r.get(29)?,
        };
        let drives = drives.remove(&run_id).unwrap_or_default();
        let memory_modules = modules.remove(&run_id).unwrap_or_default();
        let mut memory_types: Vec<String> = Vec::new();
        for t in memory_modules.iter().filter_map(|m| m.kind.as_ref()) {
            if !memory_types.contains(t) {
                memory_types.push(t.clone());
            }
        }
        let drive_wear_pct: Option<i64> = r.get(33)?;
        Ok(MachineView {
            // Set by `place`, against whichever "now" this run is seen from.
            max_age_months_now: None,
            age_estimate: None,
            age_days: None,
            // Set by `assess`, once the findings it rests on exist.
            lifecycle: Lifecycle::default(),
            thresholds: Arc::default(),
            overridden_by: Vec::new(),
            worst_drive_wear_pct: drives
                .iter()
                .filter_map(|d| d.percentage_used)
                .max()
                .or(drive_wear_pct),
            runs: 1,
            first_seen: taken_at.clone(),
            max_age_months,
            age_source,
            cpu_family,
            cohort: cohort_id(&r.get::<_, String>(12)?, &cmp),
            key,
            key_kind: r.get(2)?,
            hostname: r.get(3)?,
            taken_at,
            tool_version: r.get(5)?,
            comparability: cmp,
            score: r.get(10)?,
            grade: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
            cpu_model,
            cpu_cores: r.get(13)?,
            ram_bytes: r.get(14)?,
            os: r.get(15)?,
            arch: r.get(16)?,
            serial: r.get(17)?,
            asset_tag: r.get(18)?,
            on_ac: r.get(19)?,
            thermal_limited: r.get(20)?,
            battery_health: r.get(21)?,
            battery_cycles: r.get(22)?,
            partial: r.get(23)?,
            caveats: caveats
                .split(" | ")
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .collect(),
            source_path: r.get(25)?,
            system_model: r.get(26)?,
            cpu_launched,
            firmware_dated: r.get(30)?,
            firmware_likely_original: r.get(31)?,
            drive_power_on_hours: r.get(32)?,
            drive_wear_pct,
            drive_hours_undercounted: r.get(34)?,
            power_plan: r.get(35)?,
            power_mode: r.get(36)?,
            power_held_back: r
                .get::<_, Option<String>>(37)?
                .unwrap_or_default()
                .split(" | ")
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .collect(),
            bios_version: r.get(38)?,
            bios_date: r.get(39)?,
            disk_low_mount: r.get(40)?,
            disk_low_free_pct: r.get(41)?,
            disk_low_free_bytes: r.get(42)?,
            os_distro: r.get(43)?,
            os_version: r.get(44)?,
            os_build: r.get(45)?,
            os_edition: r.get(46)?,
            memory_slots: r.get(47)?,
            memory_populated: r.get(48)?,
            memory_types,
            memory_modules,
            run_uuid: r.get(49)?,
            run_duration_secs: r.get(50)?,
            run_collect: r
                .get::<_, Option<String>>(51)?
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .collect(),
            run_elevated: r.get(52)?,
            booted_at: r.get(53)?,
            uptime_secs: r.get(54)?,
            firmware_mode: r.get(55)?,
            secure_boot: r.get(56)?,
            tpm_present: r.get(57)?,
            tpm_version: r.get(58)?,
            disk_encryption: r.get(59)?,
            disk_encryption_method: r.get(60)?,
            drives,
            low_confidence_cpu_subtests: low_conf.get(&run_id).copied().unwrap_or(0),
            tags: tags.remove(&run_id).unwrap_or_default(),
            components: components.remove(&run_id).unwrap_or_default(),
            cohort_delta_pct: None,
            trend_pct: None,
            run_id,
        })
    })? {
        out.push(row?);
    }
    Ok(out)
}

/// Put a loaded run in time: its age as of `now`, and its run count, first
/// sighting and trend from `runs` — the machine's runs known by then, in
/// `load_history`'s order. The live snapshot passes every run the machine has;
/// a reconstruction passes only those at or before its cutoff, which is what
/// keeps a later run from leaking into the past as part of a trend.
fn place(m: &mut MachineView, runs: &[&RunRow], now: OffsetDateTime) {
    m.age_days = age_days(&m.taken_at, now);
    m.max_age_months_now = m.cpu_launched.as_deref().and_then(|l| months_since(l, now));
    m.age_estimate = best_age(
        m.cpu_launched.as_deref(),
        m.firmware_dated.as_deref().or(m.bios_date.as_deref()),
        m.firmware_likely_original,
        now,
    );
    if let Some(first) = runs.first() {
        m.runs = runs.len();
        m.first_seen = first.taken_at.clone();
    }
    m.trend_pct = trend(runs, m);
}

/// A machine against its own past.
///
/// Only comparable earlier runs count — a machine re-measured under a different
/// preset has not regressed, it has been measured differently. The reference is
/// the median of the priors rather than the best of them, because one unusually
/// good run would otherwise make every later run look like a regression.
fn trend(runs: &[&RunRow], m: &MachineView) -> Option<f64> {
    let score = m.score?;
    let priors = sorted_of(
        runs.iter()
            .filter(|r| r.machine_key == m.key && r.id != m.run_id && r.cmp == m.comparability)
            .filter_map(|r| r.score),
    );
    if priors.is_empty() {
        return None;
    }
    pct_delta(score, median(&priors))
}

fn build_cohorts(machines: &[MachineView], th: &Thresholds) -> Vec<Cohort> {
    let mut groups: BTreeMap<&str, Vec<&MachineView>> = BTreeMap::new();
    for m in machines {
        groups.entry(m.cohort.as_str()).or_default().push(m);
    }

    groups
        .into_iter()
        .map(|(id, members)| {
            let scores = sorted_of(members.iter().filter_map(|m| m.score));
            let med = median(&scores);
            let spread = mad(&scores, med);
            let head = members[0];
            Cohort {
                id: id.to_string(),
                cpu_model: head.cpu_model.clone(),
                comparability: head.comparability.clone(),
                members: members.len(),
                in_view: members.len(),
                median: med,
                mad: spread,
                scale: spread.max(med.abs() * th.mad_floor_frac),
                p10: quantile(&scores, 0.10),
                p90: quantile(&scores, 0.90),
                comparable: scores.len() >= th.min_cohort,
            }
        })
        .collect()
}

/// Every finding, machine by machine.
///
/// `fleet` is the fleet-wide thresholds, and only the cohort statistics read
/// it; everything that judges one machine against a line reads that machine's
/// own, `th` below, which an override may have changed.
fn evaluate(
    machines: &[MachineView],
    cohorts: &HashMap<&str, &Cohort>,
    firmware: &HashMap<&str, NewestFirmware>,
    fleet: &Thresholds,
) -> Vec<Flag> {
    let mut flags = Vec::new();
    for m in machines {
        let th = &*m.thresholds;
        let score = m.score.unwrap_or(0.0);
        let mut push = |severity: Severity,
                        kind: FlagKind,
                        code: &'static str,
                        headline: String,
                        detail: String| {
            debug_assert!(
                RULES.contains(&code),
                "{code} is raised by evaluate but missing from RULES"
            );
            flags.push(Flag {
                severity,
                kind,
                code,
                machine_key: m.key.clone(),
                hostname: m.hostname.clone(),
                headline,
                detail,
                triage: None,
            });
        };

        // --- The machine ----------------------------------------------------

        // D and F only. C is the middle of the scale, not a fault.
        if let Some(rank) = grade_rank(&m.grade)
            && rank >= 4
        {
            let sev = if rank == 5 {
                Severity::Critical
            } else {
                Severity::Warning
            };
            push(
                sev,
                FlagKind::Machine,
                "grade_low",
                format!("Graded {} against the reference baseline", m.grade),
                format!(
                    "{} · {} · {} — scored {score:.0}. This is an absolute grade, so it \
                     reflects how old the hardware is as much as what condition it is in. \
                     Check the cohort and trend figures before reading it as a fault.",
                    m.cpu_model,
                    human_bytes(m.ram_bytes),
                    m.os.as_deref().unwrap_or("unknown OS"),
                ),
            );
        }

        if let (Some(delta), Some(cohort)) = (m.cohort_delta_pct, cohorts.get(m.cohort.as_str()))
            && delta < 0.0
        {
            let shortfall = -delta / 100.0;
            let z = 0.6745 * (cohort.median - score) / cohort.scale;
            if shortfall >= fleet.cohort_min_shortfall && z >= fleet.cohort_z {
                let sev = if shortfall >= 0.25 {
                    Severity::Critical
                } else {
                    Severity::Warning
                };
                push(
                    sev,
                    FlagKind::Machine,
                    "cohort_outlier",
                    format!("{:.0}% slower than identical machines", shortfall * 100.0),
                    format!(
                        "Scored {score:.0} against a median of {:.0} across {} machines on {} \
                         measured with {}. The comparison holds both the hardware and the \
                         measurement configuration constant, so it doesn't inherit the \
                         reference baseline's uncertainty. Usual causes, cheapest first: a \
                         power plan or a firmware power limit, dust or dried thermal paste, a \
                         failing or nearly-full SSD, or single-channel memory where the peers \
                         are dual.",
                        cohort.median,
                        cohort.members,
                        m.cpu_model,
                        cohort.comparability.label(),
                    ),
                );
            }
        }

        if let Some(trend) = m.trend_pct
            && trend <= -th.regression_frac * 100.0
        {
            // A run that was throttled or on battery is expected to come in
            // low, so the drop is evidence about the run before it is evidence
            // about the machine.
            let explained = m.thermal_limited == Some(true)
                || m.on_ac == Some(false)
                || !m.power_held_back.is_empty();
            push(
                if explained {
                    Severity::Warning
                } else {
                    Severity::Critical
                },
                FlagKind::Machine,
                "regressed",
                format!("{:.0}% slower than its own earlier runs", -trend),
                format!(
                    "Now {score:.0}, against a median of {:.0} over {} earlier comparable \
                     run(s); a drop of {}% or more is a regression. Comparing a machine with \
                     itself holds the hardware constant, which makes this the strongest signal \
                     in the fleet: something has changed.{}",
                    score / (1.0 + trend / 100.0),
                    m.runs.saturating_sub(1),
                    figure(th.regression_frac * 100.0),
                    if explained {
                        " This run was thermally limited, taken on battery or held back by its \
                         power settings, which may account for some of it — re-measure before \
                         acting."
                    } else {
                        ""
                    }
                ),
            );
        }

        // A component well below the rest of the machine is the actionable
        // shape of "slow": a failing SSD in an otherwise healthy laptop is a
        // part to replace, not a machine to retire. Ungraded components
        // (network, gpu) are excluded because loadbearer deliberately keeps
        // them out of the grade — they measure the OS and the cooling.
        //
        // The rule itself is `components::is_weak`, which the heatmap reads
        // too, so a cell can never disagree with this finding.
        for c in m.components.iter().filter(|c| c.graded) {
            if components::is_weak(&m.grade, c) {
                push(
                    Severity::Warning,
                    FlagKind::Machine,
                    "component_weak",
                    format!(
                        "{} graded {} on an otherwise {} machine",
                        c.id, c.grade, m.grade
                    ),
                    format!(
                        "{} scored {:.0} where the machine overall scored {score:.0}. One weak \
                         component on an otherwise sound machine is usually a part rather than \
                         the platform.",
                        c.id, c.score,
                    ),
                );
            }
        }

        if let Some(health) = m.battery_health
            && health < th.battery_health_pct
        {
            push(
                Severity::Warning,
                FlagKind::Machine,
                "battery_worn",
                format!("Battery at {health:.0}% of design capacity"),
                format!(
                    "{}Under {}% a battery counts as worn. Nothing to do with performance, but it \
                     is the part of a laptop that fails first and the one its user notices.",
                    m.battery_cycles
                        .map(|c| format!("{c} charge cycles. "))
                        .unwrap_or_default(),
                    figure(th.battery_health_pct),
                ),
            );
        }

        // Drives are judged across every drive the run recorded, not just the
        // first — a workstation's second drive is as much the machine's as its
        // boot drive. One flag per rule per machine, naming the worst drive
        // and counting the rest, so a machine with three bad drives is one
        // line in the queue rather than three.
        //
        // Deliberately no rule on `unsafe_shutdowns`: a healthy laptop
        // collects hundreds from lid closes and flat batteries, so a rule on
        // it would fire on the estate rather than on a fault.
        let also = |n: usize| match n {
            0 | 1 => String::new(),
            2 => " One other drive in this machine reports the same.".to_string(),
            n => format!(" {} other drives in this machine report the same.", n - 1),
        };

        // The drive's own verdict, so nothing here is inferred: the most
        // conditions raised at once is the worst.
        let failing: Vec<(&DriveView, i64)> = m
            .drives
            .iter()
            .filter_map(|d| d.critical_warning.filter(|w| *w != 0).map(|w| (d, w)))
            .collect();
        if let Some(&(d, bits)) = failing
            .iter()
            .min_by_key(|(d, w)| (std::cmp::Reverse(w.count_ones()), d.position))
        {
            push(
                Severity::Critical,
                FlagKind::Machine,
                "drive_failing",
                format!("{} is reporting a critical warning", d.name()),
                format!(
                    "The drive's own health log has its critical-warning flags set (0x{bits:02x}: \
                     {}). That is the drive saying it is in trouble, not an inference from a slow \
                     benchmark. Back up what is on it now and replace it.{}",
                    critical_warning_words(bits).join("; "),
                    also(failing.len()),
                ),
            );
        }

        let erroring: Vec<(&DriveView, i64)> = m
            .drives
            .iter()
            .filter_map(|d| d.media_errors.filter(|n| *n > 0).map(|n| (d, n)))
            .collect();
        if let Some(&(d, n)) = erroring
            .iter()
            .min_by_key(|(d, n)| (std::cmp::Reverse(*n), d.position))
        {
            push(
                Severity::Warning,
                FlagKind::Machine,
                "drive_media_errors",
                format!("{} has recorded {n} media error(s)", d.name()),
                format!(
                    "Media and data-integrity errors are reads the drive could not correct — data \
                     it stored and could not give back intact. A healthy drive's count is zero and \
                     stays there. Back up what is on it; if the count grows between runs, replace \
                     it.{}",
                    also(erroring.len()),
                ),
            );
        }

        // An absent wear or spare figure — SATA, a hard disk — is no flag at
        // all, never a guess.
        let worn: Vec<(&DriveView, Severity)> = m
            .drives
            .iter()
            .filter_map(|d| {
                let wear = d.percentage_used.map(|w| w as f64);
                let spare = d.available_spare_pct.map(|s| s as f64);
                if wear.is_some_and(|w| w >= 100.0) {
                    Some((d, Severity::Critical))
                } else if wear.is_some_and(|w| w >= th.drive_wear_pct)
                    || spare.is_some_and(|s| s < th.drive_spare_pct)
                {
                    Some((d, Severity::Warning))
                } else {
                    None
                }
            })
            .collect();
        if let Some(&(d, sev)) = worn.iter().min_by_key(|(d, sev)| {
            (
                *sev,
                std::cmp::Reverse(d.percentage_used.unwrap_or(-1)),
                d.available_spare_pct.unwrap_or(i64::MAX),
                d.position,
            )
        }) {
            let wear = d
                .percentage_used
                .filter(|w| *w as f64 >= th.drive_wear_pct || *w >= 100);
            let spare = d
                .available_spare_pct
                .filter(|s| (*s as f64) < th.drive_spare_pct);
            let mut why = Vec::new();
            if let Some(w) = wear {
                why.push(format!(
                    "The drive estimates it has used {w}% of the writes it is rated for. That is \
                     the vendor's endurance estimate, not a prediction of failure — it can pass \
                     100, and drives often run well beyond it — but {}",
                    if w >= 100 {
                        "the rating, and usually the warranty's write allowance, is spent. Keep \
                         its data backed up and replace it."
                            .to_string()
                    } else {
                        format!(
                            "it is past the {}% at which a drive counts as worn. Plan a \
                             replacement before it reaches 100.",
                            figure(th.drive_wear_pct)
                        )
                    }
                ));
            }
            if let Some(s) = spare {
                why.push(format!(
                    "It has {s}% of its spare blocks left, under the {}% line — the pool \
                     worn-out cells are remapped to. When that runs out, a drive typically makes \
                     itself read-only.",
                    figure(th.drive_spare_pct)
                ));
            }
            push(
                sev,
                FlagKind::Machine,
                "drive_worn",
                match (wear, spare) {
                    (Some(w), _) => format!("{} has used {w}% of its rated endurance", d.name()),
                    (None, Some(s)) => format!("{} is down to {s}% spare capacity", d.name()),
                    // Unreachable by construction, but a headline is cheaper
                    // than a panic in the snapshot.
                    (None, None) => format!("{} is worn", d.name()),
                },
                format!("{}{}", why.join(" "), also(worn.len())),
            );
        }

        // Percentage-only by design: an SSD slows down as it fills whatever
        // its size, and slow is what this tool is about. A byte threshold
        // would excuse a nearly full 2 TB drive that performs like a nearly
        // full 256 GB one.
        if let Some(pct) = m.disk_low_free_pct
            && pct < th.disk_free_pct
        {
            let mount = m.disk_low_mount.as_deref().unwrap_or("a fixed volume");
            push(
                if pct < th.disk_free_critical_pct {
                    Severity::Critical
                } else {
                    Severity::Warning
                },
                FlagKind::Machine,
                "disk_nearly_full",
                format!("Only {pct:.1}% free on {mount}"),
                format!(
                    "{}Under {}% free is nearly full, and under {}% critical. An SSD slows down \
                     as it fills — it has fewer free blocks to write into and spends more time \
                     reclaiming them — so this is judged as a share of the volume rather than in \
                     gigabytes: a nearly full large drive is as slow as a nearly full small one. \
                     Updates need room to install, too. Clear space, then re-measure; the disk \
                     figures from this run may understate the drive.",
                    m.disk_low_free_bytes
                        .map(|b| format!("{} free. ", free_bytes(b)))
                        .unwrap_or_default(),
                    figure(th.disk_free_pct),
                    figure(th.disk_free_critical_pct),
                ),
            );
        }

        // Against the newest firmware seen on the same model, which is the
        // strong form of "out of date": the update exists, and other machines
        // here already run it.
        let bios_day = m.bios_date.as_deref().and_then(parse_day);
        let bios_version = m.bios_version.as_deref().unwrap_or("an unrecorded version");
        let mut behind = false;
        if let (Some(day), Some(model)) = (bios_day, m.system_model.as_deref())
            && let Some(newest) = firmware.get(model)
            && ((newest.date - day).whole_days() as f64) >= th.firmware_behind_days
        {
            behind = true;
            let others = if newest.on == 1 {
                "one machine of that model here already runs it".to_string()
            } else {
                format!("{} machines of that model here already run it", newest.on)
            };
            push(
                Severity::Info,
                FlagKind::Machine,
                "firmware_behind_peers",
                "Firmware older than on other machines of this model".to_string(),
                format!(
                    "This machine runs BIOS {bios_version}, dated {day}. The newest seen on a \
                     {model} in this estate is {}, dated {}, {} days newer, where {} days is \
                     the line — {others}, of {} with a recorded firmware date. So the update \
                     exists and has been installed elsewhere. Firmware updates carry security \
                     and stability fixes, and sometimes power or thermal changes that move \
                     benchmark results, which also makes this machine's figures less comparable \
                     with its peers' until it is updated.",
                    newest.version.as_deref().unwrap_or("an unrecorded version"),
                    newest.date,
                    (newest.date - day).whole_days(),
                    figure(th.firmware_behind_days),
                    newest.dated,
                ),
            );
        }

        // Against the run's date rather than today, so the finding is
        // deterministic and describes the machine as it was measured. The
        // weaker form, so it gives way to the one above.
        if !behind
            && let (Some(day), Some(run_day)) =
                (bios_day, m.taken_at.get(0..10).and_then(parse_day))
        {
            let years = (run_day - day).whole_days() as f64 / 365.25;
            if years > th.firmware_old_years {
                push(
                    Severity::Info,
                    FlagKind::Machine,
                    "firmware_old",
                    format!("Firmware {years:.1} years old when measured"),
                    format!(
                        "BIOS {bios_version}, dated {day}, was {years:.1} years old when this \
                         machine was measured on {run_day}, past the {} years after which \
                         firmware is named as old. Firmware updates carry security and stability \
                         fixes, and sometimes power or thermal changes that move benchmark \
                         results. No machine of the same model here is far enough ahead to \
                         compare against, so check the vendor's support site for a later \
                         release.",
                        figure(th.firmware_old_years)
                    ),
                );
            }
        }

        // --- The measurement ------------------------------------------------

        if m.thermal_limited == Some(true) {
            push(
                Severity::Warning,
                FlagKind::Measurement,
                "thermal_limited",
                "Clocks fell during the run".to_string(),
                "loadbearer watched the busiest core and saw it drop, so these figures are what \
                 the machine sustains rather than what it can do. That is a real property of the \
                 chassis and its cooling — and it also means the peak-reported subtests never \
                 reached boost, so cohort and trend comparisons against unthrottled runs \
                 understate this machine."
                    .to_string(),
            );
        }

        if m.on_ac == Some(false) {
            push(
                Severity::Warning,
                FlagKind::Measurement,
                "on_battery",
                "Measured on battery".to_string(),
                "Clocks are usually capped on battery, so this run isn't comparable with \
                 mains-powered ones. The unattended-run gates exist to stop this; a run that \
                 got through them anyway was forced past them."
                    .to_string(),
            );
        }

        if !m.power_held_back.is_empty() {
            push(
                Severity::Warning,
                FlagKind::Measurement,
                "power_limited",
                "Held back by its power settings".to_string(),
                format!(
                    "The run had {} in force, which caps clocks, so it isn't comparable with \
                     runs at full power. Worth changing on the machine if that's how it's \
                     normally used — and re-measuring either way.",
                    m.power_held_back.join(", ")
                ),
            );
        }

        if m.partial {
            push(
                Severity::Warning,
                FlagKind::Measurement,
                "partial_run",
                "Not everything ran".to_string(),
                format!(
                    "The run finished but reported: {}. Usable for what did run, misleading if \
                     read as a full sweep.",
                    m.caveats.join("; ")
                ),
            );
        }

        if m.caveats.iter().any(|c| c.contains("RAM-backed")) {
            push(
                Severity::Critical,
                FlagKind::Measurement,
                "ram_backed_disk",
                "Disk figures were measured against memory".to_string(),
                "The target directory turned out to be RAM-backed, so the disk component \
                 measured the memory subsystem instead. The numbers are enormous and \
                 meaningless — point the collector at real storage and re-run."
                    .to_string(),
            );
        }

        if m.low_confidence_cpu_subtests >= th.unstable_cpu_subtests {
            push(
                Severity::Info,
                FlagKind::Measurement,
                "unstable_run",
                format!(
                    "{} CPU subtests measured with low confidence",
                    m.low_confidence_cpu_subtests
                ),
                format!(
                    "At {} or more the run is too shaky to read. The CPU kernels are \
                     deterministic, so iteration-to-iteration spread there means something else \
                     was running on the machine — unlike spread on disk writes or on the \
                     network, which is the hardware behaving normally. Re-measure it idle before \
                     drawing anything from this run.",
                    th.unstable_cpu_subtests
                ),
            );
        }

        // --- The data we hold -----------------------------------------------

        if let Some(age) = m.age_days
            && age > th.stale_days
        {
            push(
                Severity::Info,
                FlagKind::Coverage,
                "stale",
                format!("Last measured {age:.0} days ago"),
                format!(
                    "Taken {} with loadbearer {}. After {} days a result describes the machine \
                     as it was rather than as it is.",
                    m.taken_at,
                    m.tool_version,
                    figure(th.stale_days)
                ),
            );
        }

        match m.key_kind.as_str() {
            "hostname" => push(
                Severity::Warning,
                FlagKind::Coverage,
                "weak_identity",
                "Attributed by hostname only".to_string(),
                "The result carries no firmware or OS identifier, so runs are grouped by a name \
                 that can be changed and gets reissued between machines. This machine's history \
                 may be two machines, or may be sitting under an old name. Pre-1.3.0 loadbearer \
                 records no identity at all, which is the usual reason — upgrading the client \
                 fixes it."
                    .to_string(),
            ),
            "machine_id" => push(
                Severity::Info,
                FlagKind::Coverage,
                "reimage_resets_identity",
                "Attributed by OS install ID".to_string(),
                "Readable without privilege, but reset by a reimage — so this machine's history \
                 will restart when it is rebuilt, and it can't be matched against an asset or \
                 warranty record. On Linux the firmware serial is root-only, which is the usual \
                 reason for falling back to this."
                    .to_string(),
            ),
            _ => {}
        }

        if let Some(cohort) = cohorts.get(m.cohort.as_str())
            && !cohort.comparable
        {
            push(
                Severity::Info,
                FlagKind::Coverage,
                "no_peer_group",
                "No peer group to compare against".to_string(),
                format!(
                    "Only {} machine(s) in the index run {} measured with {}, so there is no \
                     cohort median to test this against and the absolute grade is all there is. \
                     Cohort comparison starts at {} machines.",
                    cohort.members,
                    m.cpu_model,
                    m.comparability.label(),
                    fleet.min_cohort
                ),
            );
        }
    }

    // Deterministic: severity, then rule, then machine. The UI reads top-down.
    flags.sort_by(|a, b| {
        (a.severity, a.code, &a.machine_key).cmp(&(b.severity, b.code, &b.machine_key))
    });
    flags
}

/// Grade histogram, best first, with any grade this build doesn't recognise
/// last. Shared by the summary and by each group, so a group's histogram is
/// the summary's restricted to its members and the two always add up.
fn grade_histogram<'a>(grades: impl IntoIterator<Item = &'a str>) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut unrecognised = 0usize;
    for g in grades {
        match grade_rank(g) {
            Some(_) => *counts.entry(g).or_default() += 1,
            None => unrecognised += 1,
        }
    }
    let mut grades: Vec<(String, usize)> = GRADES
        .iter()
        .map(|g| ((*g).to_string(), counts.get(*g).copied().unwrap_or(0)))
        .collect();
    if unrecognised > 0 {
        grades.push(("unrecognised".to_string(), unrecognised));
    }
    grades
}

/// The machines that need attention: those carrying at least one live
/// critical or warning flag.
///
/// Info flags don't make a machine "flagged" — every machine in a small estate
/// carries one, and a headline that says all of them need attention is a
/// headline nobody reads twice. Nor does a finding somebody has acknowledged,
/// snoozed or suppressed: that one is in hand, which is the whole of what
/// triage is for. The one definition, so the overview's headline and a group's
/// count cannot come to disagree about what "flagged" means.
fn flagged_keys<'a>(flags: impl IntoIterator<Item = &'a Flag>) -> HashSet<&'a str> {
    flags
        .into_iter()
        .filter(|f| f.is_live() && needs_attention(f.severity))
        .map(|f| f.machine_key.as_str())
        .collect()
}

/// Whether a finding of this severity makes its machine one that needs
/// attention. The rule behind `flagged_keys`, on its own so the timeline —
/// which keeps findings without the `Flag` around them — applies the same one.
fn needs_attention(s: Severity) -> bool {
    s != Severity::Info
}

/// Whether a machine's latest result is history rather than inventory, by the
/// machine's own `stale_days`.
fn is_stale(m: &MachineView) -> bool {
    m.age_days.is_some_and(|a| a > m.thresholds.stale_days)
}

/// Everything in the summary is derived from the machines in view, never from a
/// separate query, so a filtered view's headline always agrees with the rows
/// underneath it.
fn summarise(machines: &[MachineView], cohorts: &[Cohort], flags: &[Flag]) -> Summary {
    let scores = sorted_of(machines.iter().filter_map(|m| m.score));
    let grades = grade_histogram(machines.iter().map(|m| m.grade.as_str()));

    let mut configs: BTreeMap<String, usize> = BTreeMap::new();
    for m in machines {
        *configs.entry(m.comparability.label()).or_default() += 1;
    }
    let mut configurations: Vec<(String, usize)> = configs.into_iter().collect();
    configurations.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let flagged = flagged_keys(flags);
    let live = |sev: Severity| {
        flags
            .iter()
            .filter(|f| f.is_live() && f.severity == sev)
            .count()
    };

    Summary {
        machines: machines.len(),
        runs: machines.iter().map(|m| m.runs).sum(),
        grades,
        median_score: (!scores.is_empty()).then(|| median(&scores)),
        p10_score: (!scores.is_empty()).then(|| quantile(&scores, 0.10)),
        p90_score: (!scores.is_empty()).then(|| quantile(&scores, 0.90)),
        critical: live(Severity::Critical),
        warnings: live(Severity::Warning),
        info: live(Severity::Info),
        machines_flagged: flagged.len(),
        quieted: QuietCounts::of(flags),
        stale: machines.iter().filter(|m| is_stale(m)).count(),
        partial: machines.iter().filter(|m| m.partial).count(),
        thermally_limited: machines
            .iter()
            .filter(|m| m.thermal_limited == Some(true))
            .count(),
        weak_identity: machines
            .iter()
            .filter(|m| matches!(m.key_kind.as_str(), "hostname" | "machine_id"))
            .count(),
        cohorts: cohorts.len(),
        comparable_cohorts: cohorts.iter().filter(|c| c.comparable).count(),
        uncohorted: machines
            .iter()
            .filter(|m| m.cohort_delta_pct.is_none())
            .count(),
        newest_run: machines
            .iter()
            .map(|m| m.taken_at.as_str())
            .max()
            .map(str::to_string),
        oldest_run: machines
            .iter()
            .map(|m| m.first_seen.as_str())
            .min()
            .map(str::to_string),
        configurations,
        lifecycle: LifecycleCounts::of(machines),
    }
}

/// What the viewer has narrowed the fleet to.
///
/// Applied as a projection over an already-computed snapshot rather than as a
/// `WHERE` clause, and that is the important part: **cohort statistics never
/// narrow with the filter.** A peer group's job is to be the largest set of
/// identically-configured machines available, so if filtering to one site
/// recomputed the medians, the same machine's shortfall would change depending
/// on what the viewer happened to be looking at — and a number that moves when
/// you look at it sideways is a number nobody trusts. The filter selects which
/// machines are *shown*; each cohort reports how many of its members are in
/// view beside statistics drawn from all of them.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Tag key to value. Every entry must match.
    pub tags: BTreeMap<String, String>,
    /// Only machines measured within this many days.
    pub max_age_days: Option<f64>,
    pub cohort: Option<String>,
    /// Exact system model, CPU model and OS — what a group on the Groups view
    /// drills into. Exact rather than a search, because "Latitude 7440"
    /// must not also find the "Latitude 7440 2-in-1" next to it, and a group's
    /// members are exactly the machines whose value equals the group's.
    pub system_model: Option<String>,
    pub cpu_model: Option<String>,
    pub os: Option<String>,
    /// Case-insensitive substring of hostname, key, serial, asset tag, system
    /// model, CPU, power plan, power mode, OS edition, version or build,
    /// memory type, or any tag value.
    pub search: Option<String>,
    /// Only machines carrying a flag with this code.
    pub flag: Option<String>,
    /// Only machines with this lifecycle verdict — what the Groups view's
    /// replace and repair counts drill into.
    pub lifecycle: Option<Verdict>,
    /// Only machines whose latest run **read** this security fact — see
    /// `Posture`. A machine that was not asked, or could not say, matches no
    /// value: "not collected" is never "off".
    pub posture: Option<Posture>,
    /// **Authorization**, not a preference: tag sets, any one of which a
    /// machine may match, or empty for the whole fleet. Set from the caller's
    /// session and never from the query string, which is what makes it
    /// something a caller cannot widen by editing a URL. ANDed with everything
    /// above, so a viewer scoped to one site who filters to another sees
    /// nothing rather than the other site.
    pub scopes: Vec<BTreeMap<String, String>>,
}

/// Whether a set of tags falls inside any of these scopes.
///
/// The one place this question is answered. A caller's scope decides what they
/// may *read* — it is ANDed into the `Filter` a snapshot is projected through —
/// and the same rule decides what they may *write*, so a contributor scoped to
/// one site cannot submit a result claiming to be from another. Two
/// implementations of that sentence would eventually disagree, and the
/// disagreement would be a security bug rather than a rendering one.
///
/// Empty scopes mean the whole fleet, matching every other use of `scopes` in
/// the codebase. A scoped caller and an *untagged* machine never match: if
/// authority is defined by a tag, something carrying no tags cannot be shown to
/// be theirs.
pub fn scopes_allow(scopes: &[BTreeMap<String, String>], tags: &BTreeMap<String, String>) -> bool {
    scopes.is_empty()
        || scopes
            .iter()
            .any(|scope| scope.iter().all(|(k, v)| tags.get(k) == Some(v)))
}

impl Filter {
    fn matches(&self, m: &MachineView, flagged: &HashSet<&str>) -> bool {
        if let Some(days) = self.max_age_days
            && m.age_days.is_none_or(|a| a > days)
        {
            return false;
        }
        if let Some(c) = &self.cohort
            && &m.cohort != c
        {
            return false;
        }
        if self.flag.is_some() && !flagged.contains(m.key.as_str()) {
            return false;
        }
        if let Some(v) = self.lifecycle
            && m.lifecycle.verdict != v
        {
            return false;
        }
        if let Some(p) = self.posture
            && !p.holds(m)
        {
            return false;
        }
        if let Some(v) = &self.system_model
            && m.system_model.as_ref() != Some(v)
        {
            return false;
        }
        if let Some(v) = &self.cpu_model
            && &m.cpu_model != v
        {
            return false;
        }
        if let Some(v) = &self.os
            && m.os.as_ref() != Some(v)
        {
            return false;
        }
        for (k, v) in &self.tags {
            if m.tags.get(k).map(String::as_str) != Some(v.as_str()) {
                return false;
            }
        }
        if !scopes_allow(&self.scopes, &m.tags) {
            return false;
        }
        if let Some(q) = &self.search {
            let q = q.to_lowercase();
            // Labelled the way the machine page words them, so "balanced
            // plan" and "balanced mode" tell apart what "balanced" alone
            // cannot. A mode with no plan beside it is a Linux profile.
            let plan = m.power_plan.as_ref().map(|p| format!("{p} plan"));
            let mode = m.power_mode.as_ref().map(|p| {
                if plan.is_some() {
                    format!("{p} mode")
                } else {
                    format!("{p} profile")
                }
            });
            let hit = [
                Some(m.key.as_str()),
                m.hostname.as_deref(),
                m.serial.as_deref(),
                m.asset_tag.as_deref(),
                Some(m.cpu_model.as_str()),
                m.system_model.as_deref(),
                plan.as_deref(),
                mode.as_deref(),
                // The parts of the OS release `os` does not reliably carry —
                // `24H2`, a build, `Enterprise` — and the memory type, so
                // "which machines are still on DDR4" is a search.
                m.os_edition.as_deref(),
                m.os_version.as_deref(),
                m.os_build.as_deref(),
            ]
            .into_iter()
            .flatten()
            .chain(m.memory_types.iter().map(String::as_str))
            // Tag *values* as well, so whatever the deployment tool labels a
            // machine with is findable by typing it. Values only, not keys: a
            // search for "site" should not return the whole estate because
            // every machine carries that key.
            .chain(m.tags.values().map(String::as_str))
            .any(|f| f.to_lowercase().contains(&q));
            if !hit {
                return false;
            }
        }
        true
    }
}

impl Snapshot {
    /// Narrow a snapshot to the machines a viewer asked for, recomputing the
    /// summary so the headline always agrees with the rows beneath it. Every
    /// finding comes out live; see `filtered_with` for the view with triage.
    /// Only the tests want a view without it.
    #[cfg(test)]
    pub fn filtered(&self, f: &Filter) -> Snapshot {
        self.filtered_with(
            f,
            &crate::state::Book::default(),
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    /// `filtered`, with what people have said about the findings attached.
    ///
    /// **This is where triage is applied, and the only place.** Every view a
    /// caller can be shown — the snapshot, a machine's page, the groups,
    /// `report` — is a projection through here, so they cannot disagree about
    /// which findings are quieted, and the counts the summary re-derives below
    /// see the same answer the findings carry. The snapshot cached between
    /// scans is never touched: triage changes on a click, the analysis only on
    /// a scan, and keeping them apart is what lets an acknowledgement show on
    /// the very next request without rebuilding anything.
    ///
    /// `now` is the clock a snooze is judged against — a parameter, like
    /// `snapshot`'s, so expiry is tested rather than waited for, and so a
    /// snooze ends at midnight UTC without a rescan.
    ///
    /// The filter still matches on every finding, quieted or not: `flag=`
    /// finds a machine whose battery is worn whether or not somebody has
    /// acknowledged it, because "which machines have this" is a question about
    /// the machines.
    pub fn filtered_with(
        &self,
        f: &Filter,
        book: &crate::state::Book,
        now: OffsetDateTime,
    ) -> Snapshot {
        let flagged: HashSet<&str> = match &f.flag {
            Some(code) => self
                .flags
                .iter()
                .filter(|fl| fl.code == code)
                .map(|fl| fl.machine_key.as_str())
                .collect(),
            None => HashSet::new(),
        };
        let machines: Vec<MachineView> = self
            .machines
            .iter()
            .filter(|m| f.matches(m, &flagged))
            .cloned()
            .collect();
        let tags_of: HashMap<&str, &BTreeMap<String, String>> =
            machines.iter().map(|m| (m.key.as_str(), &m.tags)).collect();
        // Over every finding rather than those in view, though the two agree
        // for any machine in view: a machine's findings travel together.
        let worst = if book.is_empty() {
            HashMap::new()
        } else {
            crate::state::worst_by_finding(self)
        };
        let today = now.date();
        let flags: Vec<Flag> = self
            .flags
            .iter()
            .filter_map(|fl| {
                let tags = tags_of.get(fl.machine_key.as_str())?;
                let mut fl = fl.clone();
                if !book.is_empty() {
                    let w = worst
                        .get(&(fl.machine_key.as_str(), fl.code))
                        .copied()
                        .unwrap_or(fl.severity);
                    fl.triage = book.triage_for(&fl.machine_key, fl.code, w, tags, today);
                }
                Some(fl)
            })
            .collect();

        let mut in_view: HashMap<&str, usize> = HashMap::new();
        for m in &machines {
            *in_view.entry(m.cohort.as_str()).or_default() += 1;
        }
        let cohorts: Vec<Cohort> = self
            .cohorts
            .iter()
            .map(|c| Cohort {
                in_view: in_view.get(c.id.as_str()).copied().unwrap_or(0),
                ..c.clone()
            })
            .collect();

        Snapshot {
            version: self.version,
            build: self.build,
            generated_at: self.generated_at.clone(),
            thresholds: self.thresholds.clone(),
            threshold_overrides: self.threshold_overrides.clone(),
            summary: summarise(&machines, &cohorts, &flags),
            machines,
            cohorts,
            flags,
        }
    }
}

/// What to group the machines in view by, for the Groups view and
/// `report --by`.
///
/// Spelled the same everywhere it is accepted — `model`, `cpu`, `os` or
/// `tag:<key>` — so a URL, a command line and the JSON that echoes it back all
/// say the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupBy {
    /// The value of one tag: a site, a department, a ring.
    Tag(String),
    /// The system model from the firmware — "Dell Inc. Latitude 7440".
    Model,
    Cpu,
    Os,
}

impl std::str::FromStr for GroupBy {
    type Err = String;

    /// An unknown dimension is refused rather than answered with an empty
    /// result: "no groups" would read as an answer about the fleet when it is
    /// really a typo.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "model" => Ok(Self::Model),
            "cpu" => Ok(Self::Cpu),
            "os" => Ok(Self::Os),
            other => match other.strip_prefix("tag:").map(str::trim) {
                Some(key) if !key.is_empty() => Ok(Self::Tag(key.to_string())),
                Some(_) => Err("tag: needs a key, as in tag:site".to_string()),
                None => Err(format!(
                    "{other:?} is not something machines can be grouped by. Use model, cpu, os, \
                     or tag:<key> for one of the tags the deployment tool passed (tag:site)"
                )),
            },
        }
    }
}

impl std::fmt::Display for GroupBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tag(key) => write!(f, "tag:{key}"),
            Self::Model => f.write_str("model"),
            Self::Cpu => f.write_str("cpu"),
            Self::Os => f.write_str("os"),
        }
    }
}

impl GroupBy {
    /// The group a machine falls in, or `None` when it has no value for this
    /// dimension. A blank value is no value: it would otherwise be a group
    /// whose label is nothing at all.
    fn value_of<'a>(&self, m: &'a MachineView) -> Option<&'a str> {
        let v = match self {
            Self::Tag(key) => m.tags.get(key).map(String::as_str),
            Self::Model => m.system_model.as_deref(),
            Self::Cpu => Some(m.cpu_model.as_str()),
            Self::Os => m.os.as_deref(),
        };
        v.filter(|s| !s.trim().is_empty())
    }

    /// What to group by when nobody has said: the system model if anything in
    /// view recorded one, since "which laptop model" is the usual first
    /// question; otherwise the first tag, which is whatever the estate labels
    /// itself by; otherwise the CPU, which every result carries.
    pub fn default_for(snap: &Snapshot) -> Self {
        if snap
            .machines
            .iter()
            .any(|m| Self::Model.value_of(m).is_some())
        {
            return Self::Model;
        }
        match tag_keys(snap).into_iter().next() {
            Some(key) => Self::Tag(key.to_string()),
            None => Self::Cpu,
        }
    }
}

fn tag_keys(snap: &Snapshot) -> std::collections::BTreeSet<&str> {
    snap.machines
        .iter()
        .flat_map(|m| m.tags.keys().map(String::as_str))
        .collect()
}

/// Every dimension the machines in view can be grouped by: the three every
/// result can carry, and one per tag key present. Worked out from the same
/// projection the groups are, so a scoped viewer is never offered a tag key
/// that only exists on machines they cannot see.
pub fn dimensions(snap: &Snapshot) -> Vec<String> {
    [GroupBy::Model, GroupBy::Cpu, GroupBy::Os]
        .iter()
        .map(GroupBy::to_string)
        .chain(tag_keys(snap).into_iter().map(|k| format!("tag:{k}")))
        .collect()
}

/// Findings per queue.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueueCounts {
    pub machine: usize,
    pub measurement: usize,
    pub coverage: usize,
}

impl QueueCounts {
    fn add(&mut self, kind: FlagKind) {
        *match kind {
            FlagKind::Machine => &mut self.machine,
            FlagKind::Measurement => &mut self.measurement,
            FlagKind::Coverage => &mut self.coverage,
        } += 1;
    }
}

/// One finding's code and how many machines carry it.
#[derive(Debug, Clone, Serialize)]
pub struct FlagCount {
    pub code: &'static str,
    pub machines: usize,
}

/// The machines in view that share one value of a dimension.
#[derive(Debug, Clone, Serialize)]
pub struct Group {
    /// `None` for the machines with no value at all — untagged, or no system
    /// model recorded. Kept as a group rather than dropped, so the groups
    /// always add up to the machines in view.
    pub value: Option<String>,
    pub machines: usize,
    /// Same shape and order as `Summary::grades`.
    pub grades: Vec<(String, usize)>,
    pub median_score: Option<f64>,
    pub p10_score: Option<f64>,
    pub p90_score: Option<f64>,
    /// The median, over members that have one, of each machine's difference
    /// from its own cohort median — the group's headline. See `groups`.
    pub median_cohort_delta_pct: Option<f64>,
    /// How many members that median is drawn from.
    pub with_cohort: usize,
    /// True when `with_cohort` is below `min_cohort`: the same reason a cohort
    /// that small is not compared against — with two machines, one bad one is
    /// half the median.
    pub thin: bool,
    pub median_trend_pct: Option<f64>,
    pub with_trend: usize,
    /// Members with at least one live critical or warning finding, by the
    /// same rule as `Summary::machines_flagged`.
    pub flagged: usize,
    /// Members with at least one live critical finding.
    pub machines_critical: usize,
    /// Live findings, not machines. The queues and the top findings count
    /// live ones too.
    pub critical: usize,
    pub warnings: usize,
    pub info: usize,
    /// Findings somebody has quieted, as `Summary::quieted`.
    pub quieted: QuietCounts,
    pub queues: QueueCounts,
    /// The commonest critical and warning findings among the members, most
    /// machines first and then by code, at most three.
    pub top_flags: Vec<FlagCount>,
    pub stale: usize,
    /// Median of the members' upper-bound age today (`max_age_months_now`),
    /// over the members whose CPU the launch table knows.
    pub median_max_age_months: Option<f64>,
    pub with_age: usize,
    /// Members whose lifecycle verdict is replace, and repair: the two that
    /// cost money, so "which site needs budget" is a sort on these.
    pub replace: usize,
    pub repair: usize,
}

/// Group the machines in a snapshot by one dimension.
///
/// **The headline is the median cohort delta, not the median score.** Raw
/// score by site mostly measures how old each site's hardware is: the site
/// that got its refresh last year comes out on top whatever state its machines
/// are in. Each machine's `cohort_delta_pct` already holds the hardware and the
/// measurement configuration constant, so the median of those across a group
/// asks the question that is left — whether machines *here* run slower than
/// identical machines elsewhere. That points at something about the place:
/// power policy, the image, dust, a bad batch. The score and the grade mix are
/// reported beside it as description, because they are what people ask for
/// first, and they are not the comparison.
///
/// Takes an already-projected snapshot, so it sees exactly what the caller's
/// filter and authorization scope let through and needs no scope handling of
/// its own. Cohort deltas come with the machines and were worked out against
/// the whole index, so grouping a filtered view does not move them.
pub fn groups(snap: &Snapshot, by: &GroupBy) -> Vec<Group> {
    let th = &snap.thresholds;
    let mut members: BTreeMap<Option<&str>, Vec<&MachineView>> = BTreeMap::new();
    for m in &snap.machines {
        members.entry(by.value_of(m)).or_default().push(m);
    }
    let mut flags_of: HashMap<&str, Vec<&Flag>> = HashMap::new();
    for f in &snap.flags {
        flags_of.entry(f.machine_key.as_str()).or_default().push(f);
    }

    let mut out: Vec<Group> = members
        .into_iter()
        .map(|(value, ms)| {
            let all: Vec<&Flag> = ms
                .iter()
                .flat_map(|m| flags_of.get(m.key.as_str()).into_iter().flatten().copied())
                .collect();
            // Live findings only, for every count below: a site whose owner
            // has acknowledged its drives is not the site that needs looking
            // at. What was quieted is counted on its own.
            let quieted = QuietCounts::of(all.iter().copied());
            let flags: Vec<&Flag> = all.into_iter().filter(|f| f.is_live()).collect();
            let count = |sev: Severity| flags.iter().filter(|f| f.severity == sev).count();
            let mut queues = QueueCounts::default();
            for f in &flags {
                queues.add(f.kind);
            }

            // Machines rather than findings, so one machine raising the same
            // rule on three drives doesn't outvote three machines raising it
            // once each.
            let mut by_code: BTreeMap<&'static str, HashSet<&str>> = BTreeMap::new();
            for f in flags.iter().filter(|f| needs_attention(f.severity)) {
                by_code
                    .entry(f.code)
                    .or_default()
                    .insert(f.machine_key.as_str());
            }
            let mut top_flags: Vec<FlagCount> = by_code
                .into_iter()
                .map(|(code, ms)| FlagCount {
                    code,
                    machines: ms.len(),
                })
                .collect();
            top_flags.sort_by(|a, b| b.machines.cmp(&a.machines).then(a.code.cmp(b.code)));
            top_flags.truncate(3);

            let scores = sorted_of(ms.iter().filter_map(|m| m.score));
            let deltas = sorted_of(ms.iter().filter_map(|m| m.cohort_delta_pct));
            let trends = sorted_of(ms.iter().filter_map(|m| m.trend_pct));
            let ages = sorted_of(
                ms.iter()
                    .filter_map(|m| m.max_age_months_now)
                    .map(|a| a as f64),
            );
            let med = |v: &[f64]| (!v.is_empty()).then(|| median(v));
            let decided = LifecycleCounts::of(ms.iter().copied());

            Group {
                value: value.map(str::to_string),
                machines: ms.len(),
                grades: grade_histogram(ms.iter().map(|m| m.grade.as_str())),
                median_score: med(&scores),
                p10_score: (!scores.is_empty()).then(|| quantile(&scores, 0.10)),
                p90_score: (!scores.is_empty()).then(|| quantile(&scores, 0.90)),
                median_cohort_delta_pct: med(&deltas),
                with_cohort: deltas.len(),
                thin: deltas.len() < th.min_cohort,
                median_trend_pct: med(&trends),
                with_trend: trends.len(),
                flagged: flagged_keys(flags.iter().copied()).len(),
                machines_critical: flags
                    .iter()
                    .filter(|f| f.severity == Severity::Critical)
                    .map(|f| f.machine_key.as_str())
                    .collect::<HashSet<_>>()
                    .len(),
                critical: count(Severity::Critical),
                warnings: count(Severity::Warning),
                info: count(Severity::Info),
                quieted,
                queues,
                top_flags,
                stale: ms.iter().filter(|m| is_stale(m)).count(),
                median_max_age_months: med(&ages),
                with_age: ages.len(),
                replace: decided.replace,
                repair: decided.repair,
            }
        })
        .collect();

    // Worst first by the share flagged rather than the count, so a big group
    // doesn't top the list for being big; compared as a cross-product so the
    // order is exact rather than at the mercy of float rounding. The group of
    // machines with no value goes last whatever its share: it is not a place,
    // a model or an image, and putting it first would make "not recorded" the
    // headline.
    out.sort_by(|a, b| {
        a.value
            .is_none()
            .cmp(&b.value.is_none())
            .then_with(|| (b.flagged * a.machines).cmp(&(a.flagged * b.machines)))
            .then_with(|| b.machines.cmp(&a.machines))
            .then_with(|| a.value.cmp(&b.value))
    });
    out
}

/// The whole answer to "group by this", as `/api/groups` and
/// `report --by --json` both send it.
#[derive(Debug, Clone, Serialize)]
pub struct Grouping {
    /// The dimension used — echoed back, and chosen by `GroupBy::default_for`
    /// when the caller didn't name one.
    pub by: String,
    /// Everything the machines in view could have been grouped by, so a UI
    /// can build its selector from the same projection as the groups.
    pub dimensions: Vec<String>,
    /// Machines in view. The groups' `machines` always add up to this.
    pub machines: usize,
    pub thresholds: Thresholds,
    pub groups: Vec<Group>,
}

pub fn grouping(snap: &Snapshot, by: Option<GroupBy>) -> Grouping {
    let by = by.unwrap_or_else(|| GroupBy::default_for(snap));
    Grouping {
        by: by.to_string(),
        dimensions: dimensions(snap),
        machines: snap.machines.len(),
        thresholds: snap.thresholds.clone(),
        groups: groups(snap, &by),
    }
}

/// One machine on the lifecycle list.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub machine_key: String,
    pub hostname: Option<String>,
    #[serde(flatten)]
    pub lifecycle: Lifecycle,
    pub age_estimate: Option<AgeEstimate>,
    pub grade: String,
    pub score: Option<f64>,
    pub system_model: Option<String>,
    pub tags: BTreeMap<String, String>,
    /// The overrides that changed how this machine was judged, by name — one
    /// may have moved where "old" starts for it.
    pub overridden_by: Vec<String>,
}

/// The lifecycle list, as `report --lifecycle --json` prints it.
#[derive(Debug, Clone, Serialize)]
pub struct LifecycleReport {
    pub generated_at: String,
    /// Machines in view; `counts` adds up to it.
    pub machines: usize,
    pub counts: LifecycleCounts,
    /// Fleet-wide. A candidate an override picks may be judged against
    /// another; its `overridden_by` says so.
    pub replace_age_years: f64,
    /// Every machine with a verdict other than keep, most urgent first and
    /// then by name. Keep is only counted: it is most of a healthy estate, and
    /// a list of machines with nothing to say about them is not a list anyone
    /// reads.
    pub candidates: Vec<Candidate>,
}

pub fn lifecycle_report(snap: &Snapshot) -> LifecycleReport {
    let mut candidates: Vec<Candidate> = snap
        .machines
        .iter()
        .filter(|m| m.lifecycle.verdict != Verdict::Keep)
        .map(|m| Candidate {
            machine_key: m.key.clone(),
            hostname: m.hostname.clone(),
            lifecycle: m.lifecycle.clone(),
            age_estimate: m.age_estimate,
            grade: m.grade.clone(),
            score: m.score,
            system_model: m.system_model.clone(),
            tags: m.tags.clone(),
            overridden_by: m.overridden_by.iter().map(|o| o.name.clone()).collect(),
        })
        .collect();
    candidates.sort_by(|a, b| {
        a.lifecycle.verdict.cmp(&b.lifecycle.verdict).then_with(|| {
            let name = |c: &Candidate| {
                c.hostname
                    .as_deref()
                    .unwrap_or(&c.machine_key)
                    .to_lowercase()
            };
            name(a).cmp(&name(b))
        })
    });
    LifecycleReport {
        generated_at: snap.generated_at.clone(),
        machines: snap.summary.machines,
        counts: snap.summary.lifecycle,
        replace_age_years: snap.thresholds.replace_age_years,
        candidates,
    }
}

/// One earlier run of a machine, for the history chart.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryPoint {
    pub run_id: i64,
    pub taken_at: String,
    pub score: Option<f64>,
    pub grade: String,
    pub tool_version: String,
    pub comparability: Comparability,
    pub thermal_limited: Option<bool>,
    pub on_ac: Option<bool>,
    pub partial: bool,
    pub source_path: String,
    /// How long the run's benchmarks took, and its own random ID — `run.id`
    /// in the document, which is not this index's `run_id`. `None` for a run
    /// from a loadbearer that didn't record them.
    pub duration_secs: Option<f64>,
    pub run_uuid: Option<String>,
}

/// A single measurement, for the drilldown table.
#[derive(Debug, Clone, Serialize)]
pub struct SubtestRow {
    pub component: String,
    pub id: String,
    pub value: f64,
    pub unit: String,
    pub score: Option<f64>,
    pub ratio: Option<f64>,
    pub cv: Option<f64>,
    pub confidence: String,
    /// `median` or `peak`. Comparing one against the other is meaningless, so
    /// the drilldown says which each figure is.
    pub representative: Option<String>,
    pub scored: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Detail {
    pub history: Vec<HistoryPoint>,
    pub subtests: Vec<SubtestRow>,
    /// Every drive of the latest run, in the order the document listed them.
    /// Empty for a run from before loadbearer 1.6.0 or without admin / root,
    /// and for one indexed before this build until `scan --reindex`.
    pub drives: Vec<DriveView>,
    /// Every memory module of the latest run, in the document's order. Empty
    /// for a run from a loadbearer that didn't record them, for Linux without
    /// root, and for one indexed before this build until `scan --reindex`.
    pub memory_modules: Vec<ModuleView>,
}

/// The per-machine detail the drilldown needs, which the fleet snapshot
/// deliberately doesn't carry: every run of one machine, and every subtest of
/// its latest run. Held back from the snapshot because an estate of ten
/// thousand machines would otherwise ship a hundred megabytes of measurements
/// to draw one summary.
pub fn detail(conn: &Connection, machine_key: &str, latest_run: i64) -> Result<Detail> {
    let mut stmt = conn.prepare(
        "SELECT id, taken_at, overall_score, overall_grade, tool_version, preset, profile,
                baseline, build_isa, thermal_limited, on_ac, partial, source_path,
                run_duration_secs, run_uuid
           FROM run WHERE machine_key = ?1 ORDER BY taken_at, id",
    )?;
    let history = stmt
        .query_map([machine_key], |r| {
            Ok(HistoryPoint {
                run_id: r.get(0)?,
                taken_at: r.get(1)?,
                score: r.get(2)?,
                grade: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                tool_version: r.get(4)?,
                comparability: Comparability {
                    preset: r.get(5)?,
                    profile: r.get(6)?,
                    baseline: r.get(7)?,
                    build_isa: r.get(8)?,
                },
                thermal_limited: r.get(9)?,
                on_ac: r.get(10)?,
                partial: r.get(11)?,
                source_path: r.get(12)?,
                duration_secs: r.get(13)?,
                run_uuid: r.get(14)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare(
        "SELECT component, id, value, unit, score, ratio, cv, confidence, representative, scored
           FROM subtest WHERE run_id = ?1 ORDER BY component, id",
    )?;
    let subtests = stmt
        .query_map([latest_run], |r| {
            Ok(SubtestRow {
                component: r.get(0)?,
                id: r.get(1)?,
                value: r.get(2)?,
                unit: r.get(3)?,
                score: r.get(4)?,
                ratio: r.get(5)?,
                cv: r.get(6)?,
                confidence: r.get(7)?,
                representative: r.get(8)?,
                scored: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare(&format!(
        "SELECT {DRIVE_COLUMNS} FROM drive d WHERE d.run_id = ?1 ORDER BY d.position"
    ))?;
    let drives = stmt
        .query_map([latest_run], |r| DriveView::from_row(r, 0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare(&format!(
        "SELECT {MODULE_COLUMNS} FROM memory_module mm WHERE mm.run_id = ?1 ORDER BY mm.position"
    ))?;
    let memory_modules = stmt
        .query_map([latest_run], |r| ModuleView::from_row(r, 0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Detail {
        history,
        subtests,
        drives,
        memory_modules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
    use serde_json::{Value, json};
    use std::path::Path;
    use time::macros::datetime;

    /// `snapshot` takes its clock as an argument, so every test pins it and the
    /// staleness rules get exercised rather than tiptoed around.
    pub(super) fn now() -> OffsetDateTime {
        datetime!(2026-09-09 12:00:00 UTC)
    }

    /// loadbearer's own grade boundaries. Synthetic runs have to be internally
    /// consistent: a document claiming "scored 700, graded S" would let a rule
    /// pass here that would fail in the field.
    fn grade_for(score: f64) -> &'static str {
        match score {
            s if s >= 1400.0 => "S",
            s if s >= 1150.0 => "A",
            s if s >= 850.0 => "B",
            s if s >= 600.0 => "C",
            s if s >= 400.0 => "D",
            _ => "F",
        }
    }

    fn fixture_path(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// A synthetic estate, built by mutating a real result document and pushing
    /// it through the real ingest path — so these tests cover the schema
    /// reader and the SQL as well as the rules. Nothing here fabricates a
    /// result from nothing: the shape, the subtests and the spread are all
    /// measured, and only the identity and the headline figures are moved.
    pub(super) struct Estate {
        pub(super) idx: Index,
        base: Value,
    }

    impl Estate {
        pub(super) fn new() -> Self {
            let text = std::fs::read_to_string(fixture_path("win-modern.json")).expect("fixture");
            Estate {
                idx: Index::open_in_memory().unwrap(),
                base: serde_json::from_str(&text).expect("json"),
            }
        }

        pub(super) fn add(&mut self, host: &str, score: f64, taken: &str) {
            self.add_with(host, score, taken, |_| {});
        }

        pub(super) fn add_with(
            &mut self,
            host: &str,
            score: f64,
            taken: &str,
            mutate: impl FnOnce(&mut Value),
        ) {
            let mut d = self.base.clone();
            d["timestamp"] = json!(taken);
            d["machine"]["hostname"] = json!(host);
            d["machine"]["identity"]["serial"] = json!(format!("SN-{host}"));
            d["machine"]["identity"]["smbios_uuid"] = json!(format!("uuid-{host}"));
            d["machine"]["identity"]["machine_id"] = json!(format!("mid-{host}"));
            d["overall"]["score"] = json!(score);
            d["overall"]["grade"] = json!(grade_for(score));
            mutate(&mut d);
            assert!(
                self.idx
                    .ingest_text(&d.to_string(), Path::new("synthetic.json"))
                    .unwrap(),
                "each synthetic run must be a document the index has not seen"
            );
        }

        pub(super) fn ingest(&mut self, name: &str) {
            assert!(self.idx.ingest_file(&fixture_path(name)).unwrap());
        }

        pub(super) fn snap(&self) -> Snapshot {
            self.snap_at(now())
        }

        fn snap_at(&self, at: OffsetDateTime) -> Snapshot {
            snapshot(self.idx.conn(), &Tuning::default(), at).unwrap()
        }

        /// The fleet as it was known at `at`: only runs taken by then, judged
        /// with `at` as now. What the timeline holds for each of its weeks.
        fn snap_as_of(&self, at: OffsetDateTime) -> Snapshot {
            let mut out = None;
            reconstruct(self.idx.conn(), &Tuning::default(), &[at], |_, s| {
                out = Some(s);
                Ok(())
            })
            .unwrap();
            out.expect("one cutoff, one snapshot")
        }

        /// The timeline as the server builds it, `weeks` back from `now()`.
        fn timeline(&self, weeks: usize) -> (Snapshot, Timeline) {
            let th = Tuning::default();
            let snap = snapshot(self.idx.conn(), &th, now()).unwrap();
            let t = Timeline::build(self.idx.conn(), &th, &snap, now(), weeks).unwrap();
            (snap, t)
        }
    }

    fn codes(s: &Snapshot) -> Vec<&str> {
        s.flags.iter().map(|f| f.code).collect()
    }

    fn flags_for<'a>(s: &'a Snapshot, code: &str) -> Vec<&'a Flag> {
        s.flags.iter().filter(|f| f.code == code).collect()
    }

    fn machine<'a>(s: &'a Snapshot, host: &str) -> &'a MachineView {
        s.machines
            .iter()
            .find(|m| m.hostname.as_deref() == Some(host))
            .expect("machine should be in the snapshot")
    }

    /// Set the confidence of every CPU subtest, in both places a result records
    /// it. The scored copy is what the index keeps, so a test that only touched
    /// `raw` would be testing nothing.
    fn make_cpu_noisy(d: &mut Value) {
        for section in ["components", "raw"] {
            for c in d[section].as_array_mut().expect("array") {
                if c["id"] == json!("cpu") {
                    for s in c["subtests"].as_array_mut().expect("array") {
                        s["confidence"] = json!("low");
                    }
                }
            }
        }
    }

    #[test]
    fn a_cohort_is_not_compared_against_until_it_has_peers() {
        let mut e = Estate::new();
        for i in 0..3 {
            e.add(&format!("PC-{i}"), 1000.0, "2026-09-08T10:00:00Z");
        }
        let s = e.snap();
        assert_eq!(s.summary.comparable_cohorts, 0);
        assert_eq!(s.summary.uncohorted, 3);
        assert!(s.machines.iter().all(|m| m.cohort_delta_pct.is_none()));
        assert!(codes(&s).contains(&"no_peer_group"));

        e.add("PC-3", 1000.0, "2026-09-08T10:00:00Z");
        let s = e.snap();
        assert_eq!(s.summary.comparable_cohorts, 1);
        assert!(s.machines.iter().all(|m| m.cohort_delta_pct.is_some()));
        assert!(!codes(&s).contains(&"no_peer_group"));
    }

    /// Without the floor under the dispersion, a cohort this tight would have a
    /// MAD near zero, every z-score would explode, and whichever member came
    /// lowest would be named however healthy it was.
    #[test]
    fn a_uniform_cohort_does_not_manufacture_an_outlier() {
        let mut e = Estate::new();
        for (i, score) in [988.0, 995.0, 1000.0, 1002.0, 1008.0, 1015.0]
            .into_iter()
            .enumerate()
        {
            e.add(&format!("PC-{i}"), score, "2026-09-08T10:00:00Z");
        }
        let s = e.snap();
        assert!(
            flags_for(&s, "cohort_outlier").is_empty(),
            "{:?}",
            codes(&s)
        );
    }

    #[test]
    fn a_machine_well_below_its_clones_is_named() {
        let mut e = Estate::new();
        for i in 0..5 {
            e.add(&format!("PC-{i}"), 1000.0, "2026-09-08T10:00:00Z");
        }
        e.add("PC-SLOW", 700.0, "2026-09-08T10:00:00Z");

        let s = e.snap();
        let out = flags_for(&s, "cohort_outlier");
        assert_eq!(out.len(), 1, "only the slow one: {out:?}");
        assert_eq!(out[0].hostname.as_deref(), Some("PC-SLOW"));
        assert_eq!(out[0].kind, FlagKind::Machine);
        assert_eq!(
            out[0].severity,
            Severity::Critical,
            "30% below identical hardware is not a warning"
        );
        assert!((machine(&s, "PC-SLOW").cohort_delta_pct.unwrap() + 30.0).abs() < 1e-9);
        // All six graded C against the reference baseline, so the cohort
        // comparison is doing this on its own.
        assert!(flags_for(&s, "grade_low").is_empty());
    }

    /// The reason for median and MAD. Six machines at 1000 and two at 720 give
    /// a mean of 930 and a standard deviation of about 118, so those two sit
    /// 1.8 sigma out and a 3.5-sigma rule built on mean and standard deviation
    /// names neither: each one's damage is partly absorbed into the spread the
    /// other is measured against. The median stays at 1000 regardless.
    #[test]
    fn two_degraded_machines_do_not_hide_each_other() {
        let mut e = Estate::new();
        for i in 0..6 {
            e.add(&format!("PC-{i}"), 1000.0, "2026-09-08T10:00:00Z");
        }
        e.add("PC-BAD-1", 720.0, "2026-09-08T10:00:00Z");
        e.add("PC-BAD-2", 720.0, "2026-09-08T10:00:00Z");

        let s = e.snap();
        let named: Vec<&str> = flags_for(&s, "cohort_outlier")
            .iter()
            .filter_map(|f| f.hostname.as_deref())
            .collect();
        assert_eq!(named, vec!["PC-BAD-1", "PC-BAD-2"]);
    }

    #[test]
    fn a_machine_slower_than_its_own_history_is_the_strongest_signal() {
        let mut e = Estate::new();
        e.add("PC-1", 1000.0, "2026-06-01T10:00:00Z");
        e.add("PC-1", 1010.0, "2026-07-01T10:00:00Z");
        e.add("PC-1", 995.0, "2026-08-01T10:00:00Z");
        e.add("PC-1", 800.0, "2026-09-01T10:00:00Z");

        let s = e.snap();
        assert_eq!(s.summary.machines, 1, "four runs of one machine");
        assert_eq!(s.summary.runs, 4);
        let m = machine(&s, "PC-1");
        assert_eq!(m.runs, 4);
        assert_eq!(
            m.taken_at, "2026-09-01T10:00:00Z",
            "the latest run is the one shown"
        );
        assert_eq!(m.first_seen, "2026-06-01T10:00:00Z");
        assert!((m.trend_pct.unwrap() + 20.0).abs() < 1e-9);

        let r = flags_for(&s, "regressed");
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].severity,
            Severity::Critical,
            "nothing about this run excuses the drop"
        );
    }

    /// A machine re-measured under another preset has not got slower, it has
    /// been measured another way — memory latency in particular reads
    /// differently across presets. Calling that a regression would put every
    /// reconfigured machine on the critical list.
    #[test]
    fn a_run_measured_differently_is_not_a_regression() {
        let mut e = Estate::new();
        e.add("PC-1", 1000.0, "2026-08-01T10:00:00Z");
        e.add_with("PC-1", 700.0, "2026-09-01T10:00:00Z", |d| {
            d["config"]["duration_preset"] = json!("quick");
        });

        let s = e.snap();
        assert!(machine(&s, "PC-1").trend_pct.is_none());
        assert!(flags_for(&s, "regressed").is_empty());
    }

    /// Vector AES roughly doubled `aes_gcm` where the hardware supports it.
    /// Pooling sse2 and avx2 results of one CPU would give a bimodal "cohort"
    /// whose median describes neither half of it.
    #[test]
    fn the_same_silicon_measured_with_a_different_isa_is_a_different_cohort() {
        let mut e = Estate::new();
        for i in 0..4 {
            e.add(&format!("SSE-{i}"), 1000.0, "2026-09-01T10:00:00Z");
        }
        for i in 0..4 {
            e.add_with(&format!("AVX-{i}"), 1300.0, "2026-09-01T10:00:00Z", |d| {
                d["config"]["build_isa"] = json!("avx2");
            });
        }

        let s = e.snap();
        assert_eq!(s.cohorts.len(), 2, "one CPU model, two measurements");
        assert_eq!(s.summary.comparable_cohorts, 2);
        assert_eq!(s.summary.configurations.len(), 2);
        assert!(
            flags_for(&s, "cohort_outlier").is_empty(),
            "neither group is an outlier within the other: {:?}",
            codes(&s)
        );
    }

    #[test]
    fn staleness_is_measured_from_the_clock_it_is_given() {
        let mut e = Estate::new();
        e.add("PC-1", 1000.0, "2026-01-01T10:00:00Z");

        let s = e.snap();
        assert_eq!(flags_for(&s, "stale").len(), 1);
        assert_eq!(s.summary.stale, 1);
        assert!(machine(&s, "PC-1").age_days.unwrap() > 250.0);

        let fresh = e.snap_at(datetime!(2026-01-02 10:00:00 UTC));
        assert!(flags_for(&fresh, "stale").is_empty());
        assert_eq!(fresh.summary.stale, 0);
    }

    #[test]
    fn a_throttled_run_is_a_measurement_finding_not_a_hardware_one() {
        let mut e = Estate::new();
        e.ingest("linux-throttled.json");

        let s = e.snap();
        let t = flags_for(&s, "thermal_limited");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, FlagKind::Measurement);
        assert_eq!(s.summary.thermally_limited, 1);
        assert!(
            s.machines[0].partial,
            "the run carried loadbearer's own note about lowered confidence"
        );
    }

    /// The failure that silently turns the disk component into a memory
    /// benchmark. It happened for real on a Linux box writing to /tmp, and
    /// loadbearer caught it — 1.18M IOPS at queue depth 1, 144 times the
    /// anchor. Left unflagged it would drag a cohort median up and make every
    /// honest machine beside it look broken.
    #[test]
    fn a_ram_backed_target_directory_is_critical() {
        let mut e = Estate::new();
        e.add_with("PC-1", 1000.0, "2026-09-01T10:00:00Z", |d| {
            for c in d["raw"].as_array_mut().expect("array") {
                if c["id"] == json!("disk") {
                    c["notes"] = json!(["target directory is RAM-backed (tmpfs); measures memory"]);
                }
            }
        });

        let s = e.snap();
        let f = flags_for(&s, "ram_backed_disk");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
        assert_eq!(f[0].kind, FlagKind::Measurement);
    }

    #[test]
    fn attribution_by_hostname_alone_is_reported() {
        let mut e = Estate::new();
        e.ingest("legacy-1.2.4.json");

        let s = e.snap();
        let f = flags_for(&s, "weak_identity");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FlagKind::Coverage);
        assert_eq!(f[0].severity, Severity::Warning);
        assert_eq!(s.summary.weak_identity, 1);
    }

    #[test]
    fn a_grade_this_build_does_not_know_is_counted_as_unrecognised() {
        let mut e = Estate::new();
        e.add_with("PC-1", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["overall"]["grade"] = json!("A+");
        });

        let s = e.snap();
        assert!(
            s.summary
                .grades
                .iter()
                .any(|(g, n)| g == "unrecognised" && *n == 1)
        );
        assert!(
            flags_for(&s, "grade_low").is_empty(),
            "an unknown grade is reported as unknown, never guessed at"
        );
    }

    #[test]
    fn one_weak_component_is_named_but_an_ungraded_one_is_not() {
        let mut e = Estate::new();
        e.add_with("PC-1", 1000.0, "2026-09-01T10:00:00Z", |d| {
            for c in d["components"].as_array_mut().expect("array") {
                if c["id"] == json!("disk") {
                    c["grade"] = json!("F");
                    c["score"] = json!(180.0);
                }
                // network is measured but deliberately kept out of the grade:
                // it describes the office link, not the machine.
                if c["id"] == json!("network") {
                    c["grade"] = json!("F");
                    c["score"] = json!(90.0);
                }
            }
        });

        let s = e.snap();
        let f = flags_for(&s, "component_weak");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].headline.starts_with("disk"), "{}", f[0].headline);
    }

    #[test]
    fn instability_is_judged_on_the_cpu_alone() {
        let mut e = Estate::new();
        // The clean run is real output, and it already carries low-confidence
        // disk-write and network subtests. Those must not count.
        e.add("PC-CLEAN", 1000.0, "2026-09-01T10:00:00Z");
        e.add_with("PC-BUSY", 1000.0, "2026-09-01T10:00:00Z", make_cpu_noisy);

        let s = e.snap();
        assert!(machine(&s, "PC-CLEAN").low_confidence_cpu_subtests < 2);
        assert!(machine(&s, "PC-BUSY").low_confidence_cpu_subtests >= 2);
        let f = flags_for(&s, "unstable_run");
        assert_eq!(f.len(), 1, "{:?}", codes(&s));
        assert_eq!(f[0].hostname.as_deref(), Some("PC-BUSY"));
        assert_eq!(f[0].severity, Severity::Info);
    }

    /// One machine can raise several findings at once. A headline that counted
    /// findings would make a small estate look like a crisis, so it counts
    /// machines — and informational flags don't count towards it at all,
    /// because on a varied estate every machine carries one.
    #[test]
    fn the_headline_counts_machines_not_findings() {
        let mut e = Estate::new();
        e.add("PC-OK", 1000.0, "2026-09-01T10:00:00Z");
        e.add_with("PC-SICK", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["telemetry"] = json!({
                "source": "test", "sample_count": 12, "thermal_limited": true
            });
            d["gates"] = json!({"on_ac": false});
            d["machine"]["battery"] = json!({
                "charge_pct": 55.0, "state": "discharging",
                "health_pct": 61.0, "cycle_count": 812
            });
            d["notes"] = json!(["gpu component skipped: no adapter available"]);
        });

        let s = e.snap();
        assert!(s.summary.warnings >= 4, "{:?}", codes(&s));
        assert_eq!(
            s.summary.machines_flagged,
            1,
            "one machine, several findings: {:?}",
            codes(&s)
        );
    }

    /// loadbearer 1.6.0's age evidence reaches the machine view; a document
    /// from before it leaves every field empty rather than guessing.
    #[test]
    fn age_evidence_is_carried_through_to_the_machine() {
        let mut e = Estate::new();
        e.add("PC-OLD-CLIENT", 1000.0, "2026-09-01T10:00:00Z");
        e.add_with("PC-NEW-CLIENT", 1000.0, "2026-10-28T10:00:00Z", |d| {
            d["machine"]["firmware"] = json!({
                "system_vendor": "LENOVO", "system_model": "20XW004BUK",
                "system_version": "ThinkPad X1 Carbon Gen 9",
                "bios_version": "1.15.0", "bios_date": "2021-06-02"
            });
            d["machine"]["drives"] = json!([{
                "device": "nvme0", "interface": "nvme", "power_on_hours": 12881, "power_cycles": 667,
                "unsafe_shutdowns": 195, "percentage_used": 19, "available_spare_pct": 100,
                "data_written_tb": 60.11, "media_errors": 0, "critical_warning": 0
            }]);
            d["machine"]["age"] = json!({
                "as_of": "2026-10",
                "cpu_family": "11th-gen Intel Core (mobile)",
                "cpu_launched": "2020-09",
                "max_age_months": 73,
                "firmware_dated": "2021-06",
                "firmware_after_launch_months": 9,
                "firmware_likely_original": true,
                "drive_power_on_hours": 12881,
                "some_future_field": "ignored"
            });
        });

        let s = e.snap();
        let m = machine(&s, "PC-NEW-CLIENT");
        assert_eq!(
            m.system_model.as_deref(),
            Some("LENOVO 20XW004BUK (ThinkPad X1 Carbon Gen 9)")
        );
        assert_eq!(
            m.cpu_family.as_deref(),
            Some("11th-gen Intel Core (mobile)")
        );
        assert_eq!(m.cpu_launched.as_deref(), Some("2020-09"));
        assert_eq!(m.max_age_months, Some(73));
        assert_eq!(m.firmware_dated.as_deref(), Some("2021-06"));
        assert_eq!(m.firmware_likely_original, Some(true));
        assert_eq!(m.drive_power_on_hours, Some(12881));
        assert_eq!(m.drive_wear_pct, Some(19));
        // Counted to the snapshot, not the run: older for every month that
        // passes without a new result.
        let later = e.snap_at(OffsetDateTime::parse("2027-01-15T00:00:00Z", &Rfc3339).unwrap());
        assert_eq!(
            machine(&later, "PC-NEW-CLIENT").max_age_months_now,
            Some(76)
        );

        assert_eq!(m.age_source, Some(AgeSource::Recorded));

        // A SATA drive: hours, but none of the NVMe-only fields, and no wear
        // figure on a hard disk.
        e.add_with("PC-SATA", 1000.0, "2026-10-28T10:00:00Z", |d| {
            d["machine"]["drives"] = json!([{
                "device": "sda", "interface": "sata", "power_on_hours": 40000,
                "power_cycles": 3100
            }]);
        });
        let s = e.snap();
        let sata = machine(&s, "PC-SATA");
        assert_eq!(sata.drive_power_on_hours, Some(40000));
        assert_eq!(sata.drive_wear_pct, None);
        assert_eq!(sata.drive_hours_undercounted, None);

        // A drive that only counts working time keeps its hours, flagged.
        e.add_with("PC-IDLE-CLOCK", 1000.0, "2026-10-28T10:00:00Z", |d| {
            d["machine"]["drives"] = json!([{
                "device": "sda", "interface": "sata", "power_on_hours": 175,
                "power_cycles": 4314, "percentage_used": 7
            }]);
            d["machine"]["age"] = json!({
                "as_of": "2026-10", "drive_power_on_hours": 175,
                "drive_hours_undercounted": true
            });
        });
        let s = e.snap();
        let idle = machine(&s, "PC-IDLE-CLOCK");
        assert_eq!(idle.drive_power_on_hours, Some(175));
        assert_eq!(idle.drive_hours_undercounted, Some(true));

        // An older client recorded nothing, so the launch is derived from its
        // CPU (the fixture's i7-1370P); what only the client can see — the
        // firmware, the drive — stays empty rather than guessed.
        let old = machine(&s, "PC-OLD-CLIENT");
        assert_eq!(old.age_source, Some(AgeSource::Derived));
        assert_eq!(old.cpu_launched.as_deref(), Some("2023-01"));
        assert_eq!(
            old.cpu_family.as_deref(),
            Some("13th-gen Intel Core (mobile)")
        );
        assert_eq!(old.max_age_months, Some(44)); // to the run, 2026-09
        assert!(old.max_age_months_now.is_some());
        assert_eq!(old.firmware_dated, None);
        assert_eq!(old.drive_power_on_hours, None);
        assert_eq!(old.system_model, None);
    }

    /// loadbearer 1.7.0's power settings reach the machine view: the mode for
    /// the source the run was on, battery power without a gate having been
    /// asked for, and a flag for settings that held the run back.
    #[test]
    fn power_settings_are_carried_through_to_the_machine() {
        let mut e = Estate::new();
        e.add("PC-OLD-CLIENT", 1000.0, "2026-09-01T10:00:00Z");
        e.add_with("PC-FULL-POWER", 1000.0, "2026-10-29T10:00:00Z", |d| {
            d["machine"]["power"] = json!({
                "source": "mains", "energy_saver": false, "plan": "Balanced",
                "plan_guid": "381b4222-f694-41f0-9685-ff5bb260df2e",
                "plugged_in": {"power_mode": "best performance", "max_processor_pct": 100},
                "on_battery": {"power_mode": "best power efficiency"}
            });
        });
        e.add_with("PC-SAVER", 1000.0, "2026-10-29T10:00:00Z", |d| {
            d["machine"]["power"] = json!({
                "source": "battery", "plan": "Balanced",
                "plugged_in": {"power_mode": "balanced"},
                "on_battery": {"power_mode": "best power efficiency"},
                "held_back": ["energy saver on", "Best power efficiency mode"]
            });
        });
        e.add_with("PC-LINUX", 1000.0, "2026-10-29T10:00:00Z", |d| {
            d["machine"]["power"] = json!({
                "source": "mains", "platform_profile": "low-power",
                "governor": "powersave", "energy_preference": "power", "turbo": true,
                "held_back": ["low-power platform profile", "energy preference \"power\""]
            });
        });

        let s = e.snap();
        let full = machine(&s, "PC-FULL-POWER");
        assert_eq!(full.on_ac, Some(true));
        assert_eq!(full.power_plan.as_deref(), Some("Balanced"));
        assert_eq!(full.power_mode.as_deref(), Some("best performance"));
        assert!(full.power_held_back.is_empty());

        // On battery, so the battery column's mode; and on_ac comes from
        // `machine.power` with no gate in the document.
        let saver = machine(&s, "PC-SAVER");
        assert_eq!(saver.on_ac, Some(false));
        assert_eq!(saver.power_mode.as_deref(), Some("best power efficiency"));
        assert_eq!(
            saver.power_held_back,
            ["energy saver on", "Best power efficiency mode"]
        );

        let linux = machine(&s, "PC-LINUX");
        assert_eq!(linux.power_plan, None);
        assert_eq!(linux.power_mode.as_deref(), Some("low-power"));

        let flagged: Vec<_> = flags_for(&s, "power_limited")
            .iter()
            .filter_map(|f| f.hostname.clone())
            .collect();
        assert_eq!(flagged, ["PC-LINUX", "PC-SAVER"]);
        assert_eq!(flags_for(&s, "on_battery").len(), 1);

        let old = machine(&s, "PC-OLD-CLIENT");
        assert_eq!(old.power_plan, None);
        assert_eq!(old.power_mode, None);
        assert!(old.power_held_back.is_empty());
    }

    #[test]
    fn an_unknown_cpu_gets_no_age_rather_than_a_guess() {
        let mut e = Estate::new();
        e.add_with("PC-VM", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["machine"]["cpu_model"] = json!("QEMU Virtual CPU version 2.5+");
        });
        let s = e.snap();
        let m = machine(&s, "PC-VM");
        assert_eq!(m.age_source, None);
        assert_eq!(m.cpu_launched, None);
        assert_eq!(m.max_age_months_now, None);
    }

    #[test]
    fn months_since_counts_whole_months_and_refuses_the_future() {
        let now = OffsetDateTime::parse("2026-10-28T00:00:00Z", &Rfc3339).unwrap();
        assert_eq!(months_since("2023-01", now), Some(45));
        assert_eq!(months_since("2026-10", now), Some(0));
        assert_eq!(months_since("2026-11", now), None);
        assert_eq!(months_since("garbage", now), None);
        assert_eq!(months_since("2026-13", now), None);
    }

    #[test]
    fn median_and_quantiles_interpolate() {
        assert_eq!(median(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert!(median(&[]).is_nan());
        assert_eq!(quantile(&[10.0, 20.0, 30.0], 0.5), 20.0);
        assert_eq!(quantile(&[10.0, 20.0], 0.1), 11.0);
        assert_eq!(quantile(&[42.0], 0.9), 42.0);
        assert!(quantile(&[], 0.5).is_nan());
    }

    #[test]
    fn mad_ignores_the_outlier_a_standard_deviation_would_absorb() {
        let v = sorted_of([10.0, 10.0, 10.0, 10.0, 100.0]);
        let med = median(&v);
        assert_eq!(med, 10.0);
        assert_eq!(mad(&v, med), 0.0);
    }

    #[test]
    fn a_nan_score_cannot_leave_the_sort_order_undefined() {
        let v = sorted_of([3.0, f64::NAN, 1.0, 2.0]);
        assert_eq!(v, vec![1.0, 2.0, 3.0]);
    }

    fn tagged(site: &str) -> impl FnOnce(&mut Value) + use<'_> {
        move |d: &mut Value| {
            d["tags"] = json!({ "site": site });
        }
    }

    /// The rule the whole filter design turns on. A machine's shortfall must
    /// not depend on what the viewer happened to be looking at, so narrowing
    /// the view narrows the rows and leaves the peer group alone.
    #[test]
    fn filtering_narrows_what_is_shown_without_moving_a_cohort_median() {
        let mut e = Estate::new();
        for i in 0..5 {
            e.add_with(
                &format!("GLA-{i}"),
                1000.0,
                "2026-09-01T10:00:00Z",
                tagged("glasgow"),
            );
        }
        // Three slower machines at the other site, enough to drag a median.
        for i in 0..3 {
            e.add_with(
                &format!("EDI-{i}"),
                700.0,
                "2026-09-01T10:00:00Z",
                tagged("edinburgh"),
            );
        }

        let all = e.snap();
        assert_eq!(all.cohorts.len(), 1, "identical hardware and configuration");
        let fleet_median = all.cohorts[0].median;

        let mut tags = BTreeMap::new();
        tags.insert("site".to_string(), "glasgow".to_string());
        let view = all.filtered(&Filter {
            tags,
            ..Default::default()
        });

        assert_eq!(view.summary.machines, 5);
        assert_eq!(
            view.cohorts[0].median, fleet_median,
            "the peer group did not shrink"
        );
        assert_eq!(view.cohorts[0].members, 8, "still eight machines like this");
        assert_eq!(view.cohorts[0].in_view, 5, "five of them are on screen");

        let before = all
            .machines
            .iter()
            .find(|m| m.hostname.as_deref() == Some("GLA-0"))
            .and_then(|m| m.cohort_delta_pct);
        let after = machine(&view, "GLA-0").cohort_delta_pct;
        assert_eq!(before, after, "the same machine, the same shortfall");
    }

    #[test]
    fn a_filter_hides_the_findings_of_the_machines_it_hides() {
        let mut e = Estate::new();
        e.add_with("PC-OK", 1000.0, "2026-09-01T10:00:00Z", tagged("glasgow"));
        e.add_with("PC-HOT", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["tags"] = json!({ "site": "edinburgh" });
            d["telemetry"] = json!({"source": "test", "sample_count": 9, "thermal_limited": true});
        });

        let all = e.snap();
        assert_eq!(flags_for(&all, "thermal_limited").len(), 1);

        let mut tags = BTreeMap::new();
        tags.insert("site".to_string(), "glasgow".to_string());
        let view = all.filtered(&Filter {
            tags,
            ..Default::default()
        });
        assert_eq!(view.summary.machines, 1);
        assert!(
            flags_for(&view, "thermal_limited").is_empty(),
            "a finding about a machine that isn't shown must not be counted"
        );
        assert_eq!(view.summary.thermally_limited, 0);
    }

    #[test]
    fn a_filter_can_select_by_age_by_text_and_by_finding() {
        let mut e = Estate::new();
        e.add("FRESH-1", 1000.0, "2026-09-08T10:00:00Z");
        e.add("OLD-1", 1000.0, "2026-01-01T10:00:00Z");
        let all = e.snap();

        let recent = all.filtered(&Filter {
            max_age_days: Some(30.0),
            ..Default::default()
        });
        assert_eq!(recent.summary.machines, 1);
        assert_eq!(recent.machines[0].hostname.as_deref(), Some("FRESH-1"));

        let searched = all.filtered(&Filter {
            search: Some("old".into()),
            ..Default::default()
        });
        assert_eq!(searched.summary.machines, 1, "search is case-insensitive");
        assert_eq!(searched.machines[0].hostname.as_deref(), Some("OLD-1"));

        let stale = all.filtered(&Filter {
            flag: Some("stale".into()),
            ..Default::default()
        });
        assert_eq!(stale.summary.machines, 1);
        assert_eq!(stale.machines[0].hostname.as_deref(), Some("OLD-1"));
    }

    /// Whatever the deployment tool labels a machine with has to be findable by
    /// typing it. An estate that tags machines with their owner is the case
    /// this exists for: an IT department asking "which are Jane's" should not
    /// have to know that Search covered every identifier *except* the one they
    /// put there themselves.
    #[test]
    fn search_finds_a_machine_by_a_tag_value() {
        let mut e = Estate::new();
        e.add_with("PC-1", 1000.0, "2026-09-09T10:00:00Z", |d| {
            d["tags"] = json!({ "owner": "jsmith", "site": "glasgow" });
        });
        e.add_with("PC-2", 1000.0, "2026-09-09T10:00:00Z", |d| {
            d["tags"] = json!({ "owner": "jsmith", "site": "edinburgh" });
        });
        e.add_with("PC-3", 1000.0, "2026-09-09T10:00:00Z", |d| {
            d["tags"] = json!({ "owner": "akhan", "site": "glasgow" });
        });
        let all = e.snap();

        let mine = all.filtered(&Filter {
            search: Some("JSmith".into()),
            ..Default::default()
        });
        assert_eq!(
            mine.summary.machines, 2,
            "one person can own several machines, and searching them should return all of them"
        );

        // Keys are deliberately not matched: every machine carries `site`, so
        // searching for it returning the whole estate would be useless.
        let by_key = all.filtered(&Filter {
            search: Some("site".into()),
            ..Default::default()
        });
        assert_eq!(
            by_key.summary.machines, 0,
            "a tag key is not a value and must not match every machine that has one"
        );
    }

    /// The OS detail and memory of the latest run reach the view as recorded,
    /// partial blocks included, and a run without them reads as unknown —
    /// never as zero slots or no edition.
    #[test]
    fn os_detail_and_memory_reach_the_view_as_recorded() {
        let mut e = Estate::new();
        e.add_with(
            "PC-WIN",
            1000.0,
            "2026-09-01T10:00:00Z",
            crate::schema::samples::windows,
        );
        e.add_with(
            "PC-ROOT",
            1000.0,
            "2026-09-01T10:00:00Z",
            crate::schema::samples::linux_root,
        );
        e.add_with(
            "PC-USER",
            1000.0,
            "2026-09-01T10:00:00Z",
            crate::schema::samples::linux_user,
        );
        e.add("PC-OLD", 1000.0, "2026-09-01T10:00:00Z");
        let s = e.snap();

        let win = machine(&s, "PC-WIN");
        assert_eq!(win.os_edition.as_deref(), Some("Enterprise"));
        assert_eq!(win.os_version.as_deref(), Some("24H2"));
        assert_eq!(win.os_build.as_deref(), Some("26100.2314"));
        assert_eq!(win.os_distro, None);
        assert_eq!((win.memory_slots, win.memory_populated), (Some(4), Some(2)));
        assert_eq!(win.memory_types, ["DDR5"], "two modules, one type");
        assert_eq!(win.memory_modules.len(), 2);
        assert_eq!(win.run_duration_secs, Some(472.318));
        assert!(win.run_uuid.is_some());

        let root = machine(&s, "PC-ROOT");
        assert_eq!(root.os_distro.as_deref(), Some("ubuntu"));
        assert_eq!(root.os_edition.as_deref(), Some("server"));
        assert_eq!(root.os_build, None);
        assert_eq!(
            (root.memory_slots, root.memory_populated),
            (Some(2), Some(1))
        );
        let only = &root.memory_modules[0];
        assert_eq!((only.speed_mts, only.configured_speed_mts), (None, None));
        assert_eq!(root.run_uuid, None, "a run without an ID has none");
        assert_eq!(root.run_duration_secs, Some(95.5));

        let user = machine(&s, "PC-USER");
        assert_eq!(user.os_version.as_deref(), Some("13"));
        assert_eq!((user.memory_slots, user.memory_populated), (None, None));
        assert!(user.memory_types.is_empty() && user.memory_modules.is_empty());

        let old = machine(&s, "PC-OLD");
        assert_eq!(
            (&old.os_edition, old.memory_slots, old.run_duration_secs),
            (&None, None, None)
        );

        // The history carries each run's duration and ID; the drilldown the
        // modules of the latest.
        let d = detail(e.idx.conn(), &win.key, win.run_id).unwrap();
        assert_eq!(d.history[0].duration_secs, Some(472.318));
        assert_eq!(
            d.history[0].run_uuid.as_deref(),
            Some("0f8b5a52-7c1e-4c39-9d0e-5a1f3b2c4d6e")
        );
        assert_eq!(d.memory_modules.len(), 2);
        let d = detail(e.idx.conn(), &old.key, old.run_id).unwrap();
        assert_eq!(d.history[0].duration_secs, None);
        assert!(d.memory_modules.is_empty());
    }

    /// An estate with every shape of opt-in run, for the tests below: each
    /// shape the CLI writes, on its own machine.
    fn opted_in_estate() -> Estate {
        use crate::schema::samples;
        let mut e = Estate::new();
        let at = "2026-09-01T10:00:00Z";
        e.add_with("PC-WIN", 1000.0, at, samples::optin_windows);
        e.add_with("PC-LUKS", 1000.0, at, samples::optin_linux_luks);
        e.add_with("PC-NONE", 1000.0, at, samples::optin_linux_none_detected);
        e.add_with("PC-BIOS", 1000.0, at, samples::optin_legacy_bios);
        e.add_with("PC-UNREAD", 1000.0, at, samples::optin_unreadable);
        e.add_with("PC-FUTURE", 1000.0, at, samples::optin_future);
        e.add("PC-OLD", 1000.0, at);
        e.add_with(
            "PC-STRAY",
            1000.0,
            at,
            samples::optin_blocks_without_collect,
        );
        e
    }

    /// What was asked, what was read and what was not, kept apart in the
    /// view: a field is `None` both for "not asked" and for "asked, could
    /// not be read", and `run_collect` is what tells the two apart.
    #[test]
    fn the_opt_in_facts_reach_the_view_in_their_three_states() {
        let s = opted_in_estate().snap();
        let all = ["elevation", "security", "uptime"];

        let win = machine(&s, "PC-WIN");
        assert_eq!(win.run_collect, all);
        assert_eq!(win.run_elevated, Some(false));
        assert_eq!(
            (win.booted_at.as_deref(), win.uptime_secs),
            (Some("2026-09-09T10:41:28Z"), Some(14492))
        );
        assert_eq!(win.firmware_mode.as_deref(), Some("uefi"));
        assert_eq!((win.secure_boot, win.tpm_present), (Some(true), Some(true)));
        assert_eq!(win.tpm_version.as_deref(), Some("2.0"));
        assert_eq!(win.disk_encryption, None, "never read on Windows");

        let luks = machine(&s, "PC-LUKS");
        assert_eq!(luks.secure_boot, Some(false));
        assert_eq!(
            (
                luks.disk_encryption.as_deref(),
                luks.disk_encryption_method.as_deref()
            ),
            (Some("encrypted"), Some("luks2"))
        );
        assert_eq!(luks.run_elevated, Some(true));

        let none = machine(&s, "PC-NONE");
        assert_eq!(none.run_collect, ["security"]);
        assert_eq!(none.disk_encryption.as_deref(), Some("none_detected"));
        assert_eq!(none.tpm_present, Some(false));
        assert_eq!((none.uptime_secs, none.run_elevated), (None, None));

        let bios = machine(&s, "PC-BIOS");
        assert_eq!(bios.firmware_mode.as_deref(), Some("bios"));
        assert_eq!(bios.secure_boot, None);

        let unread = machine(&s, "PC-UNREAD");
        assert_eq!(unread.run_collect, all, "asked");
        assert_eq!(
            (unread.secure_boot, unread.tpm_present, unread.uptime_secs),
            (None, None, None),
            "and nothing read"
        );

        let future = machine(&s, "PC-FUTURE");
        assert_eq!(future.run_collect, ["network", "security"]);
        assert_eq!(future.firmware_mode.as_deref(), Some("coreboot"));

        for host in ["PC-OLD", "PC-STRAY"] {
            let m = machine(&s, host);
            assert!(m.run_collect.is_empty(), "{host} was not asked");
            assert_eq!(
                (m.secure_boot, m.run_elevated, &m.booted_at),
                (None, None, &None)
            );
        }

        // No finding is drawn from any of it: every machine here is the same
        // fixture, so each carries exactly the codes the one not asked does.
        let codes = |host: &str| -> Vec<&str> {
            let key = &machine(&s, host).key;
            let mut c: Vec<&str> = s
                .flags
                .iter()
                .filter(|f| &f.machine_key == key)
                .map(|f| f.code)
                .collect();
            c.sort();
            c
        };
        for host in ["PC-WIN", "PC-LUKS", "PC-NONE", "PC-BIOS", "PC-UNREAD"] {
            assert_eq!(codes(host), codes("PC-OLD"), "{host}");
        }
    }

    /// `posture=` matches only machines where the fact was read: neither
    /// side of a question takes in a machine that was not asked, or could
    /// not say — and a legacy-BIOS boot is on neither side of Secure Boot.
    #[test]
    fn the_posture_filter_matches_only_what_was_read() {
        let all = opted_in_estate().snap();
        let hosts = |p: Posture, scopes: Vec<BTreeMap<String, String>>| -> Vec<String> {
            let mut h: Vec<_> = all
                .filtered(&Filter {
                    posture: Some(p),
                    scopes,
                    ..Default::default()
                })
                .machines
                .iter()
                .filter_map(|m| m.hostname.clone())
                .collect();
            h.sort();
            h
        };
        let of = |p| hosts(p, Vec::new());
        assert_eq!(of(Posture::SecureBootOff), ["PC-LUKS"]);
        assert_eq!(
            of(Posture::SecureBootOn),
            ["PC-FUTURE", "PC-NONE", "PC-WIN"]
        );
        assert_eq!(of(Posture::LegacyBios), ["PC-BIOS"]);
        assert_eq!(of(Posture::TpmAbsent), ["PC-NONE"]);
        assert_eq!(
            of(Posture::TpmPresent),
            ["PC-BIOS", "PC-FUTURE", "PC-LUKS", "PC-WIN"]
        );
        assert_eq!(of(Posture::Tpm12), ["PC-BIOS"]);
        assert_eq!(of(Posture::DiskEncrypted), ["PC-LUKS"]);
        assert_eq!(of(Posture::DiskNoneDetected), ["PC-BIOS", "PC-NONE"]);
        // The machines that were not asked or could not say are on neither
        // side of any question, and Windows on neither side of encryption.
        for p in Posture::ALL {
            for host in ["PC-OLD", "PC-STRAY", "PC-UNREAD"] {
                assert!(!of(p).iter().any(|h| h == host), "{host} matched {p}");
            }
        }
        for p in [Posture::DiskEncrypted, Posture::DiskNoneDetected] {
            assert!(!of(p).iter().any(|h| h == "PC-WIN"), "{p}");
        }
        // The summary agrees with the rows.
        let filtered = all.filtered(&Filter {
            posture: Some(Posture::SecureBootOff),
            ..Default::default()
        });
        assert_eq!(filtered.summary.machines, 1);

        // ANDed with the scope like everything else: a scope that takes in
        // nothing here sees nothing, whatever the posture.
        let elsewhere = vec![BTreeMap::from([(
            "site".to_string(),
            "nowhere".to_string(),
        )])];
        for p in Posture::ALL {
            assert!(hosts(p, elsewhere.clone()).is_empty(), "{p}");
        }

        // Every value parses as it is spelled, and anything else is refused.
        for p in Posture::ALL {
            assert_eq!(p.as_str().parse::<Posture>(), Ok(p));
        }
        for bad in [
            "secure_boot",
            "SECURE_BOOT_OFF",
            "off",
            "unknown",
            "tpm_2.0",
        ] {
            assert!(bad.parse::<Posture>().is_err(), "{bad}");
        }
    }

    /// The posture is not searchable: a substring such as "off" or "none"
    /// would read as an answer it is not. `posture=` is the way to ask.
    #[test]
    fn search_does_not_match_the_posture() {
        let all = opted_in_estate().snap();
        for q in ["luks2", "uefi", "coreboot", "none_detected", "encrypted"] {
            let found = all.filtered(&Filter {
                search: Some(q.into()),
                ..Default::default()
            });
            assert_eq!(found.summary.machines, 0, "{q}");
        }
    }

    /// `24H2`, `Enterprise`, a build or `DDR4` typed into the search box finds
    /// the machines that recorded it, and nothing that didn't.
    #[test]
    fn search_finds_a_machine_by_edition_build_or_memory_type() {
        let mut e = Estate::new();
        e.add_with(
            "PC-WIN",
            1000.0,
            "2026-09-01T10:00:00Z",
            crate::schema::samples::windows,
        );
        e.add_with(
            "PC-ROOT",
            1000.0,
            "2026-09-01T10:00:00Z",
            crate::schema::samples::linux_root,
        );
        // `os` says "Windows 11 Enterprise" here too, and `os` is not
        // searched: an edition the run did not record is not found.
        e.add("PC-OLD", 1000.0, "2026-09-01T10:00:00Z");
        let all = e.snap();
        let hosts = |q: &str| -> Vec<String> {
            let mut h: Vec<_> = all
                .filtered(&Filter {
                    search: Some(q.into()),
                    ..Default::default()
                })
                .machines
                .iter()
                .filter_map(|m| m.hostname.clone())
                .collect();
            h.sort();
            h
        };
        assert_eq!(hosts("24h2"), ["PC-WIN"]);
        assert_eq!(hosts("Enterprise"), ["PC-WIN"]);
        assert_eq!(hosts("26100.2314"), ["PC-WIN"]);
        assert_eq!(hosts("DDR4"), ["PC-ROOT"]);
        assert_eq!(hosts("ddr5"), ["PC-WIN"]);
        assert_eq!(hosts("server"), ["PC-ROOT"]);
    }

    /// "Which machines are on Best power efficiency" should be answerable from
    /// the search box. It matches the mode in force, the one the index holds:
    /// a machine plugged in on Best performance is not found by its battery
    /// setting. "plan", "mode" and "profile" are searchable after the name.
    #[test]
    fn search_finds_a_machine_by_its_power_mode() {
        let mut e = Estate::new();
        e.add("PC-OLD-CLIENT", 1000.0, "2026-09-01T10:00:00Z");
        e.add_with("PC-FULL-POWER", 1000.0, "2026-10-29T10:00:00Z", |d| {
            d["machine"]["power"] = json!({
                "source": "mains", "plan": "Balanced",
                "plugged_in": {"power_mode": "best performance"},
                "on_battery": {"power_mode": "best power efficiency"}
            });
        });
        e.add_with("PC-SAVER", 1000.0, "2026-10-29T10:00:00Z", |d| {
            d["machine"]["power"] = json!({
                "source": "battery", "plan": "Balanced",
                "plugged_in": {"power_mode": "balanced"},
                "on_battery": {"power_mode": "best power efficiency"}
            });
        });
        e.add_with("PC-BALANCED", 1000.0, "2026-10-29T10:00:00Z", |d| {
            d["machine"]["power"] = json!({
                "source": "mains", "plan": "High performance",
                "plugged_in": {"power_mode": "balanced"}
            });
        });
        e.add_with("PC-LINUX", 1000.0, "2026-10-29T10:00:00Z", |d| {
            d["machine"]["power"] = json!({ "source": "mains", "platform_profile": "low-power" });
        });
        let all = e.snap();
        let hosts = |q: &str| -> Vec<String> {
            let s = all.filtered(&Filter {
                search: Some(q.into()),
                ..Default::default()
            });
            let mut h: Vec<_> = s
                .machines
                .iter()
                .filter_map(|m| m.hostname.clone())
                .collect();
            h.sort();
            h
        };

        assert_eq!(hosts("Power Efficiency"), ["PC-SAVER"]);
        assert_eq!(hosts("low-power"), ["PC-LINUX"]);
        assert_eq!(hosts("best"), ["PC-FULL-POWER", "PC-SAVER"]);

        // "Balanced" is both a plan and a mode. Alone it finds either; the
        // word after it says which was meant.
        assert_eq!(
            hosts("balanced"),
            ["PC-BALANCED", "PC-FULL-POWER", "PC-SAVER"]
        );
        assert_eq!(hosts("Balanced plan"), ["PC-FULL-POWER", "PC-SAVER"]);
        assert_eq!(hosts("balanced mode"), ["PC-BALANCED"]);
        // Linux has a profile, worded as the machine page words it.
        assert_eq!(hosts("low-power profile"), ["PC-LINUX"]);
        assert!(hosts("low-power mode").is_empty());
    }

    /// The property tag-scoped authorization rests on. The viewer's own filter
    /// and their scope are ANDed, so asking for a site you are not scoped to
    /// returns nothing — never that site.
    #[test]
    fn an_authorization_scope_cannot_be_widened_by_a_filter() {
        let mut e = Estate::new();
        for i in 0..2 {
            e.add_with(
                &format!("GLA-{i}"),
                1000.0,
                "2026-09-01T10:00:00Z",
                tagged("glasgow"),
            );
        }
        for i in 0..2 {
            e.add_with(
                &format!("EDI-{i}"),
                1000.0,
                "2026-09-01T10:00:00Z",
                tagged("edinburgh"),
            );
        }
        let all = e.snap();
        let glasgow_only = vec![BTreeMap::from([(
            "site".to_string(),
            "glasgow".to_string(),
        )])];

        let scoped = all.filtered(&Filter {
            scopes: glasgow_only.clone(),
            ..Default::default()
        });
        assert_eq!(scoped.summary.machines, 2);
        assert!(scoped.machines.iter().all(|m| m.tags["site"] == "glasgow"));

        // The interesting case: a scoped viewer asking for the other site.
        let mut tags = BTreeMap::new();
        tags.insert("site".to_string(), "edinburgh".to_string());
        let overreach = all.filtered(&Filter {
            scopes: glasgow_only,
            tags,
            ..Default::default()
        });
        assert_eq!(
            overreach.summary.machines, 0,
            "asking outside your scope returns nothing, not the other site"
        );
        assert!(
            overreach.flags.is_empty(),
            "and none of its findings either"
        );
    }

    #[test]
    fn two_scopes_are_a_union_and_a_two_tag_scope_needs_both() {
        let mut e = Estate::new();
        e.add_with("GLA-1", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["tags"] = json!({"site": "glasgow", "ring": "canary"});
        });
        e.add_with("GLA-2", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["tags"] = json!({"site": "glasgow", "ring": "broad"});
        });
        e.add_with("EDI-1", 1000.0, "2026-09-01T10:00:00Z", tagged("edinburgh"));
        let all = e.snap();

        let both_sites = all.filtered(&Filter {
            scopes: vec![
                BTreeMap::from([("site".to_string(), "glasgow".to_string())]),
                BTreeMap::from([("site".to_string(), "edinburgh".to_string())]),
            ],
            ..Default::default()
        });
        assert_eq!(both_sites.summary.machines, 3);

        let narrow = all.filtered(&Filter {
            scopes: vec![BTreeMap::from([
                ("site".to_string(), "glasgow".to_string()),
                ("ring".to_string(), "canary".to_string()),
            ])],
            ..Default::default()
        });
        assert_eq!(
            narrow.summary.machines, 1,
            "every tag in a scope must match"
        );
        assert_eq!(narrow.machines[0].hostname.as_deref(), Some("GLA-1"));
    }

    /// A consequence worth being explicit about: a scope is a positive match,
    /// so a machine whose result carried no tags is invisible to a scoped
    /// viewer. If the deployment tool doesn't set tags, scoped viewers see an
    /// empty fleet — the safe direction to fail, but it needs saying out loud.
    #[test]
    fn an_untagged_machine_is_invisible_to_a_scoped_viewer() {
        let mut e = Estate::new();
        e.add_with("TAGGED", 1000.0, "2026-09-01T10:00:00Z", tagged("glasgow"));
        e.add_with("UNTAGGED", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["tags"] = json!({});
        });
        let all = e.snap();
        assert_eq!(all.summary.machines, 2, "both are in the fleet");

        let scoped = all.filtered(&Filter {
            scopes: vec![BTreeMap::from([(
                "site".to_string(),
                "glasgow".to_string(),
            )])],
            ..Default::default()
        });
        assert_eq!(scoped.summary.machines, 1);
        assert_eq!(scoped.machines[0].hostname.as_deref(), Some("TAGGED"));
    }

    /// An unscoped caller — an admin, or a viewer granted the whole fleet — is
    /// not restricted by an empty scope list.
    #[test]
    fn an_empty_scope_list_means_the_whole_fleet() {
        let mut e = Estate::new();
        e.add_with("GLA-1", 1000.0, "2026-09-01T10:00:00Z", tagged("glasgow"));
        e.add_with("UNTAGGED", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["tags"] = json!({});
        });
        let view = e.snap().filtered(&Filter::default());
        assert_eq!(view.summary.machines, 2);
    }

    /// The codes the drive, disk and firmware rules raise.
    const HEALTH_CODES: [&str; 6] = [
        "drive_failing",
        "drive_media_errors",
        "drive_worn",
        "disk_nearly_full",
        "firmware_behind_peers",
        "firmware_old",
    ];

    /// A healthy NVMe drive as loadbearer 1.6.0 records one — including a
    /// laptop's ordinary pile of unsafe shutdowns, which must not be a finding.
    fn nvme(model: &str) -> Value {
        json!({
            "device": "nvme0", "interface": "nvme", "model": model,
            "power_on_hours": 5120, "power_cycles": 910, "unsafe_shutdowns": 240,
            "percentage_used": 10, "available_spare_pct": 100, "data_written_tb": 20.5,
            "media_errors": 0, "critical_warning": 0
        })
    }

    fn nvme_with(model: &str, field: &str, value: Value) -> Value {
        let mut d = nvme(model);
        d[field] = value;
        d
    }

    fn with_drives(drives: Vec<Value>) -> impl FnOnce(&mut Value) {
        move |d: &mut Value| {
            d["machine"]["drives"] = Value::Array(drives);
        }
    }

    fn flag_on<'a>(s: &'a Snapshot, code: &str, host: &str) -> Option<&'a Flag> {
        s.flags
            .iter()
            .find(|f| f.code == code && f.hostname.as_deref() == Some(host))
    }

    #[test]
    fn a_drive_raising_its_own_critical_warning_is_critical() {
        let mut e = Estate::new();
        e.add_with(
            "PC-OK",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![nvme("Boot SSD"), nvme("Data SSD")]),
        );
        // The second drive is the bad one: a rule that read only the first,
        // as the age line does, would miss it.
        e.add_with(
            "PC-SECOND",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![
                nvme("Boot SSD"),
                nvme_with("Data SSD", "critical_warning", json!(4)),
            ]),
        );
        // Two failing drives: one flag, naming the one raising more.
        e.add_with(
            "PC-BOTH",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![
                nvme_with("Hot SSD", "critical_warning", json!(2)),
                nvme_with("Dying SSD", "critical_warning", json!(0x0c)),
            ]),
        );

        let s = e.snap();
        assert_eq!(flags_for(&s, "drive_failing").len(), 2, "{:?}", codes(&s));
        assert!(flag_on(&s, "drive_failing", "PC-OK").is_none());

        let f = flag_on(&s, "drive_failing", "PC-SECOND").expect("the second drive");
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.kind, FlagKind::Machine);
        assert!(f.headline.contains("Data SSD"), "{}", f.headline);
        assert!(f.detail.contains("reliability degraded"), "{}", f.detail);
        assert!(f.detail.contains("replace"), "{}", f.detail);

        let f = flag_on(&s, "drive_failing", "PC-BOTH").expect("one flag for two drives");
        assert!(f.headline.contains("Dying SSD"), "{}", f.headline);
        assert!(f.detail.contains("read-only"), "{}", f.detail);
        assert!(f.detail.contains("One other drive"), "{}", f.detail);

        // Hundreds of unsafe shutdowns on every drive, and not one finding
        // about them.
        assert!(
            s.flags
                .iter()
                .all(|f| !f.detail.to_lowercase().contains("unsafe")),
            "{:?}",
            codes(&s)
        );
    }

    #[test]
    fn critical_warning_bits_are_decoded_and_unknown_ones_reported_by_number() {
        assert_eq!(
            critical_warning_words(0x01),
            ["spare capacity below the drive's own threshold"]
        );
        assert_eq!(critical_warning_words(0x0c).len(), 2);
        assert_eq!(critical_warning_words(0x40), ["bit 6"]);
    }

    #[test]
    fn a_media_error_is_a_warning_from_the_first_one() {
        let mut e = Estate::new();
        e.add_with(
            "PC-ZERO",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![nvme("SSD")]),
        );
        e.add_with(
            "PC-ONE",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![
                nvme("Boot SSD"),
                nvme_with("Data SSD", "media_errors", json!(1)),
            ]),
        );
        // SATA reports no media-error count at all; absent is not zero and
        // is not a flag either.
        e.add_with(
            "PC-SATA",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![
                json!({"device": "sda", "interface": "sata", "power_on_hours": 40000}),
            ]),
        );

        let s = e.snap();
        let f = flags_for(&s, "drive_media_errors");
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].hostname.as_deref(), Some("PC-ONE"));
        assert_eq!(f[0].severity, Severity::Warning);
        assert!(f[0].headline.contains("Data SSD"), "{}", f[0].headline);
    }

    #[test]
    fn wear_and_spare_are_judged_at_their_boundaries() {
        let mut e = Estate::new();
        let cases: [(&str, &str, Value); 7] = [
            ("WEAR-79", "percentage_used", json!(79)),
            ("WEAR-80", "percentage_used", json!(80)),
            ("WEAR-99", "percentage_used", json!(99)),
            ("WEAR-100", "percentage_used", json!(100)),
            // It can pass 100, and the reader must not choke on it.
            ("WEAR-163", "percentage_used", json!(163)),
            ("SPARE-20", "available_spare_pct", json!(20)),
            ("SPARE-19", "available_spare_pct", json!(19)),
        ];
        for (host, field, value) in cases {
            e.add_with(
                host,
                1000.0,
                "2026-09-01T10:00:00Z",
                with_drives(vec![nvme_with("SSD", field, value)]),
            );
        }
        // A hard disk has no wear figure: nothing to say, nothing guessed.
        e.add_with(
            "HDD",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![json!({
                "device": "sdb", "interface": "sata", "model": "Spinning Rust",
                "power_on_hours": 61000, "power_cycles": 3100
            })]),
        );

        let s = e.snap();
        let sev = |host: &str| flag_on(&s, "drive_worn", host).map(|f| f.severity);
        assert_eq!(sev("WEAR-79"), None);
        assert_eq!(sev("WEAR-80"), Some(Severity::Warning));
        assert_eq!(sev("WEAR-99"), Some(Severity::Warning));
        assert_eq!(sev("WEAR-100"), Some(Severity::Critical));
        assert_eq!(sev("WEAR-163"), Some(Severity::Critical));
        assert_eq!(sev("SPARE-20"), None);
        assert_eq!(sev("SPARE-19"), Some(Severity::Warning));
        assert_eq!(sev("HDD"), None);

        let f = flag_on(&s, "drive_worn", "WEAR-163").unwrap();
        assert!(f.headline.contains("163%"), "{}", f.headline);
        assert!(
            f.detail.contains("not a prediction of failure"),
            "{}",
            f.detail
        );
        let f = flag_on(&s, "drive_worn", "SPARE-19").unwrap();
        assert!(f.headline.contains("19% spare"), "{}", f.headline);
    }

    #[test]
    fn of_several_worn_drives_the_worst_is_named() {
        let mut e = Estate::new();
        e.add_with(
            "PC-1",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![
                nvme_with("Tired SSD", "percentage_used", json!(85)),
                nvme_with("Spent SSD", "percentage_used", json!(104)),
                nvme("Fresh SSD"),
            ]),
        );
        let s = e.snap();
        let f = flags_for(&s, "drive_worn");
        assert_eq!(f.len(), 1, "one flag per machine, however many drives");
        assert_eq!(f[0].severity, Severity::Critical);
        assert!(f[0].headline.contains("Spent SSD"), "{}", f[0].headline);
        assert!(f[0].detail.contains("One other drive"), "{}", f[0].detail);
    }

    fn with_disks(disks: Value) -> impl FnOnce(&mut Value) {
        move |d: &mut Value| {
            d["machine"]["disks"] = disks;
        }
    }

    /// A fixed volume of `total_gib`, `free_pct` free.
    fn volume(mount: &str, total_gib: u64, free_pct: f64) -> Value {
        let total = total_gib * 1024 * 1024 * 1024;
        json!({
            "name": mount, "mount_point": mount, "file_system": "NTFS", "kind": "SSD",
            "total_bytes": total, "available_bytes": (total as f64 * free_pct / 100.0) as u64,
            "removable": false
        })
    }

    #[test]
    fn a_nearly_full_disk_is_judged_on_its_fullest_fixed_volume() {
        let mut e = Estate::new();
        for (host, pct) in [
            ("FREE-10", 10.0),
            ("FREE-9.9", 9.9),
            ("FREE-5", 5.0),
            ("FREE-4.9", 4.9),
        ] {
            e.add_with(
                host,
                1000.0,
                "2026-09-01T10:00:00Z",
                with_disks(json!([volume("C:\\", 100, 40.0), volume("D:\\", 100, pct)])),
            );
        }
        // Nearly full, but on volumes that run that way by design: a USB
        // stick, an EFI partition and a recovery partition. The fixed volume
        // that matters is fine. The last entry has no `removable` at all, as
        // a document from a reader-tolerant future might not.
        let mut stick = volume("E:\\", 64, 1.0);
        stick["removable"] = json!(true);
        let mut no_flag_field = volume("F:\\", 200, 60.0);
        no_flag_field
            .as_object_mut()
            .expect("object")
            .remove("removable");
        e.add_with(
            "SMALL-AND-REMOVABLE",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_disks(json!([
                volume("/", 250, 30.0),
                stick,
                {
                    "name": "efi", "mount_point": "/boot/efi", "file_system": "vfat",
                    "kind": "SSD", "total_bytes": 536_870_912u64,
                    "available_bytes": 1_048_576u64, "removable": false
                },
                volume("/recovery", 7, 0.5),
                no_flag_field
            ])),
        );

        let s = e.snap();
        let sev = |host: &str| flag_on(&s, "disk_nearly_full", host).map(|f| f.severity);
        assert_eq!(sev("FREE-10"), None);
        assert_eq!(sev("FREE-9.9"), Some(Severity::Warning));
        assert_eq!(sev("FREE-5"), Some(Severity::Warning));
        assert_eq!(sev("FREE-4.9"), Some(Severity::Critical));
        assert_eq!(sev("SMALL-AND-REMOVABLE"), None);

        let f = flag_on(&s, "disk_nearly_full", "FREE-4.9").unwrap();
        assert!(f.headline.contains("D:\\"), "{}", f.headline);
        assert!(f.detail.contains("4.9 GiB free"), "{}", f.detail);

        let m = machine(&s, "SMALL-AND-REMOVABLE");
        assert_eq!(m.disk_low_mount.as_deref(), Some("/"));
        assert!((m.disk_low_free_pct.unwrap() - 30.0).abs() < 0.01);
    }

    /// A Lenovo X1 Carbon, as loadbearer records its firmware.
    fn firmware(bios_version: &str, bios_date: &str) -> impl FnOnce(&mut Value) {
        let (v, d) = (bios_version.to_string(), bios_date.to_string());
        move |doc: &mut Value| {
            doc["machine"]["firmware"] = json!({
                "system_vendor": "LENOVO", "system_model": "20XW004BUK",
                "system_version": "ThinkPad X1 Carbon Gen 9",
                "bios_version": v, "bios_date": d
            });
        }
    }

    #[test]
    fn firmware_behind_its_peers_needs_two_dated_machines_and_the_day_gap() {
        // One machine of the model: nothing to be behind.
        let mut e = Estate::new();
        e.add_with(
            "ALONE",
            1000.0,
            "2026-09-01T10:00:00Z",
            firmware("N32ET80W (1.56)", "2025-03-03"),
        );
        assert!(flags_for(&e.snap(), "firmware_behind_peers").is_empty());

        // A second machine whose date can't be read doesn't make a group.
        e.add_with(
            "GARBLED",
            1000.0,
            "2026-09-01T10:00:00Z",
            firmware("N32ET91W (1.67)", "06/01/2025"),
        );
        assert!(flags_for(&e.snap(), "firmware_behind_peers").is_empty());

        // Now a dated peer exactly 90 days newer than ALONE, and a third
        // machine one day short of that gap.
        e.add_with(
            "NEWEST",
            1000.0,
            "2026-09-01T10:00:00Z",
            firmware("N32ET91W (1.67)", "2025-06-01"),
        );
        e.add_with(
            "NEARLY",
            1000.0,
            "2026-09-01T10:00:00Z",
            firmware("N32ET85W (1.61)", "2025-03-04"),
        );
        // The same dates on another model are not this model's peers.
        e.add_with("OTHER-MODEL", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["machine"]["firmware"] = json!({
                "system_vendor": "Dell Inc.", "system_model": "Latitude 7440",
                "bios_version": "1.2.0", "bios_date": "2024-01-01"
            });
        });

        let s = e.snap();
        let behind: Vec<_> = flags_for(&s, "firmware_behind_peers")
            .iter()
            .filter_map(|f| f.hostname.as_deref())
            .collect();
        assert_eq!(behind, ["ALONE"], "{:?}", codes(&s));

        let f = flag_on(&s, "firmware_behind_peers", "ALONE").unwrap();
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.kind, FlagKind::Machine);
        for needle in [
            "N32ET80W (1.56)",
            "2025-03-03",
            "N32ET91W (1.67)",
            "2025-06-01",
            "90 days",
        ] {
            assert!(f.detail.contains(needle), "{needle} missing: {}", f.detail);
        }
        assert_eq!(
            machine(&s, "ALONE").bios_version.as_deref(),
            Some("N32ET80W (1.56)")
        );
    }

    #[test]
    fn old_firmware_is_measured_against_the_run_and_gives_way_to_a_peer_comparison() {
        let mut e = Estate::new();
        // Exactly four years before the run is not *more* than four.
        e.add_with("FOUR-YEARS", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["machine"]["firmware"] = json!({
                "system_vendor": "HP", "system_model": "EliteBook 840 G9",
                "bios_version": "U70 01.02.00", "bios_date": "2022-09-01"
            });
        });
        e.add_with(
            "FOUR-YEARS-AND-A-DAY",
            1000.0,
            "2026-09-01T10:00:00Z",
            |d| {
                d["machine"]["firmware"] = json!({
                    "system_vendor": "HP", "system_model": "EliteBook 850 G9",
                    "bios_version": "U70 01.01.00", "bios_date": "2022-08-31"
                });
            },
        );
        // Old *and* behind a peer: the peer comparison is the stronger
        // statement, so it is the only one made.
        e.add_with(
            "BEHIND",
            1000.0,
            "2026-09-01T10:00:00Z",
            firmware("N32ET40W (1.10)", "2020-01-15"),
        );
        e.add_with(
            "CURRENT",
            1000.0,
            "2026-09-01T10:00:00Z",
            firmware("N32ET91W (1.67)", "2026-06-01"),
        );

        let s = e.snap();
        assert!(flag_on(&s, "firmware_old", "FOUR-YEARS").is_none());
        let f = flag_on(&s, "firmware_old", "FOUR-YEARS-AND-A-DAY").expect("past four years");
        assert_eq!(f.severity, Severity::Info);
        assert!(f.detail.contains("security"), "{}", f.detail);

        assert!(flag_on(&s, "firmware_behind_peers", "BEHIND").is_some());
        assert!(
            flag_on(&s, "firmware_old", "BEHIND").is_none(),
            "{:?}",
            codes(&s)
        );

        // Measured against the run, not the clock: years later, the same
        // run says the same thing.
        let later = e.snap_at(datetime!(2031-01-01 00:00:00 UTC));
        assert!(flag_on(&later, "firmware_old", "FOUR-YEARS").is_none());
    }

    /// The newest firmware per model is a fleet-wide fact, like a cohort
    /// median: narrowing the view to the machine that is behind doesn't make
    /// it current.
    #[test]
    fn a_filter_does_not_move_the_newest_firmware_on_a_model() {
        let mut e = Estate::new();
        e.add_with("GLA-OLD", 1000.0, "2026-09-01T10:00:00Z", |d| {
            firmware("N32ET40W (1.10)", "2025-01-01")(d);
            tagged("glasgow")(d);
        });
        e.add_with("EDI-NEW", 1000.0, "2026-09-01T10:00:00Z", |d| {
            firmware("N32ET91W (1.67)", "2026-06-01")(d);
            tagged("edinburgh")(d);
        });
        let view = e.snap().filtered(&Filter {
            scopes: vec![BTreeMap::from([(
                "site".to_string(),
                "glasgow".to_string(),
            )])],
            ..Default::default()
        });
        assert_eq!(view.summary.machines, 1);
        assert!(flag_on(&view, "firmware_behind_peers", "GLA-OLD").is_some());
    }

    /// A document from before any of this — no drives, no firmware — raises
    /// none of it and fails nothing. Its one disk is a quarter free.
    #[test]
    fn the_legacy_document_raises_none_of_the_health_findings() {
        let mut e = Estate::new();
        e.ingest("legacy-1.2.4.json");
        e.ingest("linux-throttled.json");
        e.ingest("low-end.json");
        e.add("PC-1", 1000.0, "2026-09-01T10:00:00Z");
        let s = e.snap();
        for code in HEALTH_CODES {
            assert!(flags_for(&s, code).is_empty(), "{code}: {:?}", codes(&s));
        }
        let legacy = s
            .machines
            .iter()
            .find(|m| m.tool_version == "1.2.4")
            .expect("legacy machine");
        assert!(legacy.drives.is_empty());
        assert_eq!(legacy.bios_date, None);
        // Disks are in every version, so this is filled even here.
        assert_eq!(legacy.disk_low_mount.as_deref(), Some("C:\\"));
        let d = detail(e.idx.conn(), &legacy.key, legacy.run_id).unwrap();
        assert!(d.drives.is_empty());
    }

    #[test]
    fn the_drilldown_carries_every_drive_in_document_order() {
        let mut e = Estate::new();
        e.add_with(
            "PC-1",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_drives(vec![
                nvme("Boot SSD"),
                json!({"device": "sda", "interface": "sata", "power_on_hours": 40000}),
            ]),
        );
        let s = e.snap();
        let m = machine(&s, "PC-1");
        let d = detail(e.idx.conn(), &m.key, m.run_id).unwrap();
        assert_eq!(d.drives.len(), 2);
        assert_eq!(d.drives[0].model.as_deref(), Some("Boot SSD"));
        assert_eq!(d.drives[0].unsafe_shutdowns, Some(240));
        assert_eq!(d.drives[1].name(), "sda", "no model, so the device");
        assert_eq!(d.drives[1].percentage_used, None);
        // Kept out of the snapshot, which ships every machine.
        let json = serde_json::to_value(m).unwrap();
        assert!(json.get("drives").is_none());
    }

    /// The drilldown's own query: every run of one machine, and the subtests of
    /// its latest run only.
    #[test]
    fn the_drilldown_returns_the_whole_series_and_one_runs_measurements() {
        let mut e = Estate::new();
        e.add("PC-1", 1000.0, "2026-07-01T10:00:00Z");
        e.add("PC-1", 1010.0, "2026-08-01T10:00:00Z");
        e.add("PC-2", 900.0, "2026-08-01T10:00:00Z");

        let s = e.snap();
        let m = machine(&s, "PC-1");
        let d = detail(e.idx.conn(), &m.key, m.run_id).unwrap();

        assert_eq!(
            d.history.len(),
            2,
            "both runs of this machine, and only this machine"
        );
        assert_eq!(
            d.history[0].taken_at, "2026-07-01T10:00:00Z",
            "oldest first"
        );
        assert!(!d.subtests.is_empty());
        assert!(
            d.subtests
                .iter()
                .any(|t| t.representative.as_deref() == Some("peak")),
            "the drilldown has to say which figures are peaks"
        );
        assert!(
            d.subtests.iter().any(|t| !t.scored),
            "ungraded subtests are measurements too"
        );
    }

    /// The group labelled `value`, or a panic naming what there was instead.
    fn group<'a>(gs: &'a [Group], value: Option<&str>) -> &'a Group {
        gs.iter()
            .find(|g| g.value.as_deref() == value)
            .unwrap_or_else(|| {
                panic!(
                    "no group {value:?} in {:?}",
                    gs.iter().map(|g| &g.value).collect::<Vec<_>>()
                )
            })
    }

    fn site_tag() -> GroupBy {
        GroupBy::Tag("site".into())
    }

    /// A machine at a site, or untagged when `site` is `None`.
    fn at(site: Option<&str>) -> impl FnOnce(&mut Value) + use<'_> {
        move |d: &mut Value| {
            d["tags"] = match site {
                Some(s) => json!({ "site": s }),
                None => json!({}),
            };
        }
    }

    /// Clocks fell during the run: a warning, and so a flagged machine.
    fn hot(d: &mut Value) {
        d["telemetry"] = json!({"source": "test", "sample_count": 9, "thermal_limited": true});
    }

    /// A Dell of the given model, as loadbearer 1.6.0 records its firmware.
    fn dell(model: &str) -> impl FnOnce(&mut Value) + use<'_> {
        move |d: &mut Value| {
            d["machine"]["firmware"] = json!({
                "system_vendor": "Dell Inc.", "system_model": model,
                "bios_version": "1.2.0", "bios_date": "2025-01-01"
            });
        }
    }

    /// Every machine lands in exactly one group, the ones with no value
    /// included — so the groups add up to the fleet in view and nothing goes
    /// missing between the headline and the table.
    #[test]
    fn groups_add_up_to_the_machines_in_view_including_the_untagged() {
        let mut e = Estate::new();
        for i in 0..3 {
            e.add_with(
                &format!("GLA-{i}"),
                1000.0,
                "2026-09-01T10:00:00Z",
                at(Some("glasgow")),
            );
        }
        for i in 0..2 {
            e.add_with(
                &format!("EDI-{i}"),
                1000.0,
                "2026-09-01T10:00:00Z",
                at(Some("edinburgh")),
            );
        }
        e.add_with("LOOSE", 1000.0, "2026-09-01T10:00:00Z", at(None));

        let s = e.snap();
        let gs = groups(&s, &site_tag());
        assert_eq!(gs.len(), 3);
        assert_eq!(
            gs.iter().map(|g| g.machines).sum::<usize>(),
            s.summary.machines
        );
        assert_eq!(group(&gs, Some("glasgow")).machines, 3);
        assert_eq!(group(&gs, Some("edinburgh")).machines, 2);
        assert_eq!(
            group(&gs, None).machines,
            1,
            "an untagged machine is a group of its own, not dropped"
        );
        for g in &gs {
            assert_eq!(
                g.grades.iter().map(|(_, n)| n).sum::<usize>(),
                g.machines,
                "a group's grade histogram covers every member"
            );
        }

        // A key nobody carries is one honest group of everything, not an
        // empty answer.
        let nobody = groups(&s, &GroupBy::Tag("department".into()));
        assert_eq!(nobody.len(), 1);
        assert_eq!(nobody[0].value, None);
        assert_eq!(nobody[0].machines, s.summary.machines);
    }

    #[test]
    fn each_dimension_groups_by_its_own_field() {
        let mut e = Estate::new();
        e.add_with("A", 1000.0, "2026-09-01T10:00:00Z", |d| {
            dell("Latitude 7440")(d);
            d["machine"]["os"] = json!("Windows 11 Enterprise");
            d["tags"] = json!({ "site": "glasgow" });
        });
        e.add_with("B", 1000.0, "2026-09-01T10:00:00Z", |d| {
            dell("Latitude 7440")(d);
            d["machine"]["os"] = json!("Windows 10 Pro");
            d["machine"]["cpu_model"] = json!("Some Other CPU");
            d["tags"] = json!({ "site": "edinburgh" });
        });
        e.add_with("C", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["machine"]["os"] = json!("Windows 11 Enterprise");
            d["tags"] = json!({ "site": "glasgow" });
        });
        let s = e.snap();
        let base_cpu = machine(&s, "A").cpu_model.clone();

        let by_model = groups(&s, &GroupBy::Model);
        assert_eq!(
            group(&by_model, Some("Dell Inc. Latitude 7440")).machines,
            2
        );
        assert_eq!(
            group(&by_model, None).machines,
            1,
            "no firmware recorded is a group of its own"
        );

        let by_cpu = groups(&s, &GroupBy::Cpu);
        assert_eq!(group(&by_cpu, Some(&base_cpu)).machines, 2);
        assert_eq!(group(&by_cpu, Some("Some Other CPU")).machines, 1);

        let by_os = groups(&s, &GroupBy::Os);
        assert_eq!(group(&by_os, Some("Windows 11 Enterprise")).machines, 2);
        assert_eq!(group(&by_os, Some("Windows 10 Pro")).machines, 1);

        let by_site = groups(&s, &site_tag());
        assert_eq!(group(&by_site, Some("glasgow")).machines, 2);
        assert_eq!(group(&by_site, Some("edinburgh")).machines, 1);

        assert_eq!(
            dimensions(&s),
            ["model", "cpu", "os", "tag:site"],
            "the three every result can carry, then one per tag key in view"
        );
    }

    /// The headline compares each member with identical machines, so a member
    /// with no peer group has nothing to contribute to it — and the group says
    /// how many members the median actually came from.
    #[test]
    fn the_headline_is_the_median_against_identical_machines_and_says_how_many() {
        let mut e = Estate::new();
        for i in 0..3 {
            e.add_with(
                &format!("EDI-{i}"),
                1000.0,
                "2026-09-01T10:00:00Z",
                at(Some("edinburgh")),
            );
        }
        e.add_with("GLA-0", 900.0, "2026-09-01T10:00:00Z", at(Some("glasgow")));
        e.add_with("GLA-1", 950.0, "2026-09-01T10:00:00Z", at(Some("glasgow")));
        // Alone on its hardware, so it has no cohort delta. Slow on purpose: if
        // it leaked into the headline it would drag it down visibly.
        e.add_with("GLA-ODD", 500.0, "2026-09-01T10:00:00Z", |d| {
            d["machine"]["cpu_model"] = json!("Some Other CPU");
            d["tags"] = json!({ "site": "glasgow" });
        });

        let s = e.snap();
        assert!(machine(&s, "GLA-ODD").cohort_delta_pct.is_none());
        let gs = groups(&s, &site_tag());
        let gla = group(&gs, Some("glasgow"));
        assert_eq!(gla.machines, 3);
        assert_eq!(
            gla.with_cohort, 2,
            "GLA-ODD has no peers to be compared with"
        );
        let delta = gla.median_cohort_delta_pct.expect("two members have peers");
        assert!(
            (delta - -7.5).abs() < 1e-9,
            "median of -10% and -5%, got {delta}"
        );
        // The score, by contrast, is every member's: description, not the
        // comparison.
        assert_eq!(gla.median_score, Some(900.0));

        let edi = group(&gs, Some("edinburgh"));
        assert_eq!(edi.with_cohort, 3);
        assert_eq!(edi.median_cohort_delta_pct, Some(0.0));

        // Nobody with peers at all: no median, rather than a NaN.
        let mut e = Estate::new();
        e.add("LONE", 1000.0, "2026-09-01T10:00:00Z");
        let gs = groups(&e.snap(), &GroupBy::Cpu);
        assert_eq!(gs[0].with_cohort, 0);
        assert_eq!(gs[0].median_cohort_delta_pct, None);
        assert!(gs[0].thin);
    }

    /// Thin for the same reason a cohort that small is not compared against:
    /// with three machines, one bad one is a third of the median.
    #[test]
    fn a_group_is_thin_below_min_cohort_members_with_peers() {
        let n = Thresholds::default().min_cohort;
        let mut e = Estate::new();
        for i in 0..n {
            e.add_with(
                &format!("A-{i}"),
                1000.0,
                "2026-09-01T10:00:00Z",
                at(Some("alpha")),
            );
        }
        for i in 0..n - 1 {
            e.add_with(
                &format!("B-{i}"),
                1000.0,
                "2026-09-01T10:00:00Z",
                at(Some("bravo")),
            );
        }
        let gs = groups(&e.snap(), &site_tag());
        let alpha = group(&gs, Some("alpha"));
        assert_eq!(alpha.with_cohort, n);
        assert!(!alpha.thin, "exactly min_cohort is enough");
        let bravo = group(&gs, Some("bravo"));
        assert_eq!(bravo.with_cohort, n - 1);
        assert!(bravo.thin, "one fewer is not");
        assert!(
            bravo.median_cohort_delta_pct.is_some(),
            "thin is a caveat on the figure, not a reason to withhold it"
        );
    }

    /// The same rule as the overview's headline, so the groups' flagged counts
    /// add up to it exactly.
    #[test]
    fn flagged_groups_add_up_to_the_headline() {
        let mut e = Estate::new();
        e.add_with(
            "GLA-OK",
            1000.0,
            "2026-09-01T10:00:00Z",
            at(Some("glasgow")),
        );
        e.add_with("GLA-HOT", 1000.0, "2026-09-01T10:00:00Z", |d| {
            at(Some("glasgow"))(d);
            hot(d);
        });
        // Several findings on one machine still make one flagged machine.
        e.add_with("EDI-SICK", 1000.0, "2026-09-01T10:00:00Z", |d| {
            at(Some("edinburgh"))(d);
            hot(d);
            d["gates"] = json!({"on_ac": false});
        });
        e.add_with("LOOSE-HOT", 1000.0, "2026-09-01T10:00:00Z", |d| {
            at(None)(d);
            hot(d);
        });

        let s = e.snap();
        assert_eq!(s.summary.machines_flagged, 3);
        let gs = groups(&s, &site_tag());
        assert_eq!(
            gs.iter().map(|g| g.flagged).sum::<usize>(),
            s.summary.machines_flagged
        );
        let edi = group(&gs, Some("edinburgh"));
        assert_eq!(edi.flagged, 1, "one machine, two findings");
        assert!(edi.warnings >= 2, "findings are counted as findings");
        assert_eq!(
            edi.queues.measurement, edi.warnings,
            "both are about the measurement"
        );
        assert_eq!(
            gs.iter().map(|g| g.critical).sum::<usize>(),
            s.summary.critical
        );
        assert_eq!(
            gs.iter().map(|g| g.warnings).sum::<usize>(),
            s.summary.warnings
        );
    }

    #[test]
    fn top_findings_are_the_commonest_by_machines_then_by_code() {
        let mut e = Estate::new();
        // Three of the four are stale, an informational finding. If those
        // counted, `stale` would lead the list; they don't, so it isn't there.
        e.add_with("M1", 1000.0, "2026-01-01T10:00:00Z", |d| {
            hot(d);
            d["gates"] = json!({"on_ac": false});
        });
        e.add_with("M2", 1000.0, "2026-01-01T10:00:00Z", hot);
        e.add_with("M3", 1000.0, "2026-01-01T10:00:00Z", |d| {
            d["machine"]["battery"] = json!({
                "charge_pct": 90.0, "state": "charging", "health_pct": 61.0, "cycle_count": 812
            });
        });
        e.add_with("M4", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["machine"]["power"] = json!({
                "source": "mains", "plan": "Balanced",
                "plugged_in": {"power_mode": "best power efficiency"},
                "held_back": ["Best power efficiency mode"]
            });
        });

        let s = e.snap();
        assert_eq!(flags_for(&s, "stale").len(), 3, "{:?}", codes(&s));
        let gs = groups(&s, &GroupBy::Cpu);
        assert_eq!(gs.len(), 1);
        let top: Vec<(&str, usize)> = gs[0]
            .top_flags
            .iter()
            .map(|f| (f.code, f.machines))
            .collect();
        // Two machines first; then three tied at one, by code, and the third
        // of those (`power_limited`) past the cut.
        assert_eq!(
            top,
            [
                ("thermal_limited", 2),
                ("battery_worn", 1),
                ("on_battery", 1)
            ],
            "{:?}",
            codes(&s)
        );
    }

    #[test]
    fn groups_are_ordered_by_share_flagged_with_no_value_last() {
        let mut e = Estate::new();
        let mut add = |host: &str, site: Option<&'static str>, flagged: bool| {
            e.add_with(host, 1000.0, "2026-09-01T10:00:00Z", move |d| {
                at(site)(d);
                if flagged {
                    hot(d);
                }
            });
        };
        add("A-1", Some("alpha"), true);
        add("A-2", Some("alpha"), false);
        add("B-1", Some("bravo"), true);
        for i in 0..3 {
            add(&format!("C-{i}"), Some("charlie"), false);
        }
        add("E-1", Some("echo"), false);
        add("D-1", Some("delta"), false);
        // Every one of these flagged, and still last: "not recorded" is not
        // the headline.
        add("LOOSE", None, true);

        let gs = groups(&e.snap(), &site_tag());
        let order: Vec<Option<&str>> = gs.iter().map(|g| g.value.as_deref()).collect();
        assert_eq!(
            order,
            [
                Some("bravo"),   // 1 of 1
                Some("alpha"),   // 1 of 2
                Some("charlie"), // 0 of 3: more machines first at the same share
                Some("delta"),   // 0 of 1, and then by name
                Some("echo"),
                None,
            ]
        );
    }

    #[test]
    fn an_unknown_dimension_is_refused_and_a_known_one_round_trips() {
        for good in ["model", "cpu", "os", "tag:site", "tag:cost centre"] {
            let by: GroupBy = good.parse().expect(good);
            assert_eq!(by.to_string(), good);
        }
        assert_eq!(" os ".parse::<GroupBy>(), Ok(GroupBy::Os));
        for bad in ["", "site", "Model", "tag:", "tag:  ", "hostname"] {
            let err = bad.parse::<GroupBy>().expect_err(bad);
            assert!(
                err.contains("tag:"),
                "{bad:?} should say what is accepted: {err}"
            );
        }
    }

    /// Grouping is over the projection it is handed, so the viewer's filter
    /// and scope narrow the groups exactly as they narrow everything else —
    /// while each machine's standing against its peers is the fleet-wide one.
    #[test]
    fn grouping_a_filtered_view_sees_only_the_filtered_machines() {
        let mut e = Estate::new();
        for i in 0..3 {
            e.add_with(&format!("GLA-{i}"), 950.0, "2026-09-01T10:00:00Z", |d| {
                at(Some("glasgow"))(d);
                dell("Latitude 7440")(d);
            });
        }
        for i in 0..3 {
            e.add_with(&format!("EDI-{i}"), 1000.0, "2026-09-01T10:00:00Z", |d| {
                at(Some("edinburgh"))(d);
                dell("Latitude 5440")(d);
            });
        }
        let all = e.snap();
        let fleet = groups(&all, &GroupBy::Model);
        let glasgow = BTreeMap::from([("site".to_string(), "glasgow".to_string())]);

        for (why, filter) in [
            (
                "a filter",
                Filter {
                    tags: glasgow.clone(),
                    ..Default::default()
                },
            ),
            (
                "a scope",
                Filter {
                    scopes: vec![glasgow.clone()],
                    ..Default::default()
                },
            ),
        ] {
            let view = all.filtered(&filter);
            let gs = groups(&view, &GroupBy::Model);
            assert_eq!(gs.len(), 1, "{why}: only Glasgow's model is in view");
            assert_eq!(gs[0].value.as_deref(), Some("Dell Inc. Latitude 7440"));
            assert_eq!(gs[0].machines, view.summary.machines);
            assert_eq!(
                gs[0].median_cohort_delta_pct,
                group(&fleet, Some("Dell Inc. Latitude 7440")).median_cohort_delta_pct,
                "{why}: the same machines stand where they did against their peers"
            );
            assert_eq!(
                dimensions(&view),
                ["model", "cpu", "os", "tag:site"],
                "{why}"
            );
        }
    }

    /// What a group drills into: exact, so one model does not also find the
    /// model whose name starts the same way, and ANDed like everything else.
    #[test]
    fn a_filter_can_select_an_exact_model_cpu_and_os() {
        let mut e = Estate::new();
        e.add_with("PLAIN", 1000.0, "2026-09-01T10:00:00Z", |d| {
            dell("Latitude 7440")(d);
            d["machine"]["os"] = json!("Windows 11 Enterprise");
        });
        e.add_with("CONVERTIBLE", 1000.0, "2026-09-01T10:00:00Z", |d| {
            dell("Latitude 7440 2-in-1")(d);
            d["machine"]["os"] = json!("Windows 11 Pro");
            d["machine"]["cpu_model"] = json!("Some Other CPU");
        });
        let all = e.snap();
        let hosts = |f: Filter| -> Vec<String> {
            all.filtered(&f)
                .machines
                .iter()
                .filter_map(|m| m.hostname.clone())
                .collect()
        };

        assert_eq!(
            hosts(Filter {
                search: Some("Latitude 7440".into()),
                ..Default::default()
            })
            .len(),
            2,
            "search is a substring, which is why it is the wrong tool here"
        );
        assert_eq!(
            hosts(Filter {
                system_model: Some("Dell Inc. Latitude 7440".into()),
                ..Default::default()
            }),
            ["PLAIN"]
        );
        assert_eq!(
            hosts(Filter {
                cpu_model: Some("Some Other CPU".into()),
                ..Default::default()
            }),
            ["CONVERTIBLE"]
        );
        assert_eq!(
            hosts(Filter {
                os: Some("Windows 11 Enterprise".into()),
                ..Default::default()
            }),
            ["PLAIN"]
        );
        assert!(
            hosts(Filter {
                system_model: Some("Dell Inc. Latitude 7440".into()),
                os: Some("Windows 11 Pro".into()),
                ..Default::default()
            })
            .is_empty(),
            "two fields are both required"
        );
        assert!(
            hosts(Filter {
                os: Some("windows 11 enterprise".into()),
                ..Default::default()
            })
            .is_empty(),
            "exact means exact"
        );
    }

    #[test]
    fn the_default_dimension_is_the_model_then_the_first_tag_then_the_cpu() {
        let mut e = Estate::new();
        e.add_with("UNTAGGED", 1000.0, "2026-09-01T10:00:00Z", at(None));
        assert_eq!(GroupBy::default_for(&e.snap()), GroupBy::Cpu);

        e.add_with("TAGGED", 1000.0, "2026-09-01T10:00:00Z", |d| {
            d["tags"] = json!({ "site": "glasgow", "ring": "canary" });
        });
        assert_eq!(
            GroupBy::default_for(&e.snap()),
            GroupBy::Tag("ring".into()),
            "the first key, so the choice is stable"
        );

        e.add_with(
            "MODELLED",
            1000.0,
            "2026-09-01T10:00:00Z",
            dell("Latitude 7440"),
        );
        let s = e.snap();
        assert_eq!(GroupBy::default_for(&s), GroupBy::Model);
        assert_eq!(grouping(&s, None).by, "model");
        assert_eq!(grouping(&s, Some(GroupBy::Os)).by, "os");
    }

    // --- The fleet over time ---------------------------------------------

    /// The heart of the reconstruction: as of a date between two runs, the
    /// machine stands on the earlier one, and its trend is drawn only from the
    /// runs before *that* — the later run must not leak into the past.
    #[test]
    fn as_of_a_date_a_machine_stands_on_the_run_it_had_by_then() {
        let mut e = Estate::new();
        e.add("PC-1", 1000.0, "2026-06-01T10:00:00Z");
        e.add("PC-1", 900.0, "2026-07-01T10:00:00Z");
        e.add("PC-1", 600.0, "2026-08-01T10:00:00Z");

        let then = e.snap_as_of(datetime!(2026-07-15 00:00:00 UTC));
        let m = machine(&then, "PC-1");
        assert_eq!(m.taken_at, "2026-07-01T10:00:00Z");
        assert_eq!(m.runs, 2, "the third run had not been taken");
        assert_eq!(then.summary.runs, 2);
        assert_eq!(m.first_seen, "2026-06-01T10:00:00Z");
        assert!(
            (m.trend_pct.unwrap() + 10.0).abs() < 1e-9,
            "900 against the one earlier run, 1000: {:?}",
            m.trend_pct
        );
        assert_eq!(then.generated_at, "2026-07-15T00:00:00Z");
        assert!(
            flags_for(&then, "regressed").is_empty(),
            "-10% is within the regression threshold, and the -33% to come has not happened"
        );

        // Today, the same machine is the third run, trended against both.
        let now = e.snap();
        let m = machine(&now, "PC-1");
        assert_eq!(m.taken_at, "2026-08-01T10:00:00Z");
        assert!((m.trend_pct.unwrap() - (600.0 / 950.0 - 1.0) * 100.0).abs() < 1e-9);
        assert_eq!(flags_for(&now, "regressed").len(), 1);
    }

    #[test]
    fn a_machine_first_measured_after_the_date_is_absent_from_it() {
        let mut e = Estate::new();
        e.add("OLD", 1000.0, "2026-06-01T10:00:00Z");
        e.add("NEW", 1000.0, "2026-08-01T10:00:00Z");

        let then = e.snap_as_of(datetime!(2026-07-01 00:00:00 UTC));
        assert_eq!(then.summary.machines, 1);
        assert_eq!(then.machines[0].hostname.as_deref(), Some("OLD"));
        assert!(
            then.flags
                .iter()
                .all(|f| f.hostname.as_deref() == Some("OLD"))
        );

        let before_anything = e.snap_as_of(datetime!(2026-01-01 00:00:00 UTC));
        assert_eq!(before_anything.summary.machines, 0);
        assert!(before_anything.flags.is_empty());
    }

    /// Stale means "not measured for a while" *as of the date being looked
    /// at*. Judged against today instead, every past week of the timeline would
    /// be as stale as the machine is now.
    #[test]
    fn staleness_in_the_past_is_judged_against_that_date() {
        let mut e = Estate::new();
        e.add("PC-1", 1000.0, "2026-01-01T10:00:00Z");

        let fresh = e.snap_as_of(datetime!(2026-02-01 10:00:00 UTC));
        assert!(flags_for(&fresh, "stale").is_empty());
        assert!((machine(&fresh, "PC-1").age_days.unwrap() - 31.0).abs() < 1e-9);

        let later = e.snap_as_of(datetime!(2026-06-01 10:00:00 UTC));
        let stale = flags_for(&later, "stale");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].headline, "Last measured 151 days ago");
        assert_eq!(later.summary.stale, 1);
    }

    /// The live view admits every run however it is dated, as it always has —
    /// a machine with a clock set in the future stays on the dashboard. Only a
    /// reconstruction applies a cutoff, and at "now" that leaves the
    /// future-dated run out.
    #[test]
    fn the_live_snapshot_keeps_a_run_dated_in_the_future() {
        let mut e = Estate::new();
        e.add("PC-1", 1000.0, "2026-09-01T10:00:00Z");
        e.add("SKEWED", 1000.0, "2027-03-01T10:00:00Z");

        let live = e.snap();
        assert_eq!(live.summary.machines, 2);
        let reconstructed = e.snap_as_of(now());
        assert_eq!(reconstructed.summary.machines, 1);
    }

    /// What loadbearer writes, what the fixtures carry, and what RFC 3339
    /// otherwise permits, all through the one parser.
    #[test]
    fn run_timestamps_parse_in_every_form_the_index_holds() {
        let at = |s: &str| instant(s).map(|t| t.unix_timestamp_nanos());
        let whole = at("2026-09-09T13:50:25Z").unwrap();
        assert_eq!(
            at("2026-09-09T13:50:25.7775272Z"),
            Some(whole + 777_527_200)
        );
        assert_eq!(
            at("2026-09-09T16:59:48.365852386Z").map(|n| n % 1_000_000_000),
            Some(365_852_386)
        );
        assert_eq!(at("2026-09-09T13:50:25.0000000Z"), Some(whole));
        assert_eq!(at("2026-09-09T13:50:25+00:00"), Some(whole));
        assert_eq!(at("2026-09-09T14:50:25+01:00"), Some(whole));
        assert_eq!(
            at("2026-09-09t13:50:25z"),
            Some(whole),
            "RFC 3339 allows lower case"
        );
        assert_eq!(at("2026-09-09"), None);
        assert_eq!(at("yesterday"), None);
    }

    /// The reason the cutoff compares instants rather than text. As strings,
    /// `"…:00Z"` sorts after `"…:00.5Z"` — `.` is below `Z` — and an offset
    /// sorts by its local time, hours from where it belongs.
    #[test]
    fn the_cutoff_compares_instants_not_text() {
        let mut e = Estate::new();
        // Half a second apart; as text the later one sorts first.
        e.add("SUBSEC", 1000.0, "2026-09-01T10:00:00Z");
        e.add("SUBSEC", 900.0, "2026-09-01T10:00:00.5Z");
        let a = e.snap_as_of(datetime!(2026-09-01 10:00:00.25 UTC));
        assert_eq!(machine(&a, "SUBSEC").taken_at, "2026-09-01T10:00:00Z");
        assert_eq!(machine(&a, "SUBSEC").runs, 1);
        let b = e.snap_as_of(datetime!(2026-09-01 10:00:01 UTC));
        assert_eq!(
            machine(&b, "SUBSEC").taken_at,
            "2026-09-01T10:00:00.5Z",
            "the chronologically later run, though it sorts first as text"
        );

        // 12:00 at +05:00 is 07:00 UTC, so it was measured by 08:00 UTC
        // although "…T12" sorts after "…T08".
        e.add("OFFSET", 1000.0, "2026-09-01T12:00:00+05:00");
        let c = e.snap_as_of(datetime!(2026-09-01 08:00:00 UTC));
        assert!(
            c.machines
                .iter()
                .any(|m| m.hostname.as_deref() == Some("OFFSET"))
        );
        let d = e.snap_as_of(datetime!(2026-09-01 06:00:00 UTC));
        assert!(
            d.machines
                .iter()
                .all(|m| m.hostname.as_deref() != Some("OFFSET"))
        );
    }

    /// A timestamp nobody can read can't be shown to precede anything, so the
    /// past leaves it out — while the live view keeps the machine, as it
    /// always has, with its age unreadable.
    #[test]
    fn a_run_with_an_unreadable_timestamp_is_only_ever_current() {
        let mut e = Estate::new();
        e.add("READABLE", 1000.0, "2026-09-01T10:00:00Z");
        e.add("GARBLED", 1000.0, "last tuesday");

        let live = e.snap();
        assert_eq!(live.summary.machines, 2);
        assert!(machine(&live, "GARBLED").age_days.is_none());

        let past = e.snap_as_of(datetime!(2026-09-05 00:00:00 UTC));
        assert_eq!(past.summary.machines, 1);
        let (_, t) = e.timeline(4);
        let view = t.view(&live, 4);
        let last = view.points.last().unwrap();
        assert_eq!(last.machines, 2, "now is the live snapshot");
        assert_eq!(view.points[view.points.len() - 2].machines, 1);
    }

    #[test]
    fn timeline_points_are_a_week_apart_and_start_with_the_first_run() {
        let mut e = Estate::new();
        // First seen 20 days before now(): three weeks back holds nothing.
        e.add("PC-1", 1000.0, "2026-08-20T10:00:00Z");
        e.add("PC-2", 1000.0, "2026-09-05T10:00:00Z");

        let (snap, t) = e.timeline(TIMELINE_WEEKS);
        let dates = t.dates();
        assert_eq!(dates.len(), TIMELINE_WEEKS + 1, "every week is held");
        assert_eq!(*dates.last().unwrap(), now());
        assert!(
            dates
                .windows(2)
                .all(|w| w[1] - w[0] == time::Duration::weeks(1))
        );

        let view = t.view(&snap, TIMELINE_WEEKS);
        let at: Vec<&str> = view.points.iter().map(|p| p.at.as_str()).collect();
        assert_eq!(
            at,
            ["2026-08-26", "2026-09-02", "2026-09-09"],
            "the weeks before anything was measured are not sent as zeros"
        );
        let machines: Vec<usize> = view.points.iter().map(|p| p.machines).collect();
        assert_eq!(machines, [1, 1, 2]);

        // Fewer weeks asked for, fewer sent.
        assert_eq!(t.view(&snap, 1).points.len(), 2);
    }

    /// The newest point is the live snapshot, so it must say exactly what the
    /// overview's headline says — through the same definitions.
    #[test]
    fn the_newest_point_agrees_with_the_summary() {
        let mut e = Estate::new();
        for i in 0..5 {
            e.add(&format!("PC-{i}"), 1000.0, "2026-09-01T10:00:00Z");
        }
        e.add("SLOW", 650.0, "2026-09-01T10:00:00Z");
        e.add_with("HOT", 1000.0, "2026-09-01T10:00:00Z", hot);
        e.add("OLD", 450.0, "2026-03-01T10:00:00Z");
        e.add_with(
            "FULL",
            1000.0,
            "2026-09-01T10:00:00Z",
            with_disks(json!([volume("C:\\", 100, 2.0)])),
        );

        let (snap, t) = e.timeline(4);
        let p = t.view(&snap, 4).points.pop().unwrap();
        let s = &snap.summary;
        assert_eq!(p.at, "2026-09-09");
        assert_eq!(p.machines, s.machines);
        assert_eq!(p.grades, s.grades);
        assert_eq!(p.median_score, s.median_score);
        assert_eq!(p.machines_flagged, s.machines_flagged);
        assert_eq!(
            (p.critical, p.warnings, p.info),
            (s.critical, s.warnings, s.info)
        );
        assert_eq!(p.stale, s.stale);
        assert!(p.stale > 0 && p.critical > 0, "the estate exercises both");
        let critical: HashSet<&str> = snap
            .flags
            .iter()
            .filter(|f| f.severity == Severity::Critical)
            .map(|f| f.machine_key.as_str())
            .collect();
        assert_eq!(p.machines_critical, critical.len());
        let grouped = groups(&snap, &GroupBy::Cpu);
        assert_eq!(grouped.len(), 1, "one CPU, one group");
        assert_eq!(p.machines_critical, grouped[0].machines_critical);
        assert_eq!(p.queues.machine, grouped[0].queues.machine);
        assert_eq!(p.queues.coverage, grouped[0].queues.coverage);
    }

    /// Each past point is the reconstruction at its date, slimmed: the
    /// figures the timeline sends must be the ones a full snapshot of that
    /// date would have summarised to.
    #[test]
    fn a_past_point_agrees_with_a_reconstruction_of_its_date() {
        let mut e = Estate::new();
        for i in 0..5 {
            e.add(&format!("PC-{i}"), 1000.0, "2026-08-01T10:00:00Z");
            e.add(&format!("PC-{i}"), 1010.0, "2026-09-05T10:00:00Z");
        }
        e.add("SLOW", 690.0, "2026-08-25T10:00:00Z");
        e.add_with("HOT", 1000.0, "2026-08-28T10:00:00Z", hot);
        e.add("GONE-QUIET", 450.0, "2026-04-01T10:00:00Z");

        let (snap, t) = e.timeline(4);
        let points = t.view(&snap, 4).points;
        assert_eq!(points.len(), 5);
        for (k, p) in points.iter().rev().enumerate().skip(1) {
            let past = e.snap_as_of(now() - time::Duration::weeks(k as i64));
            let s = &past.summary;
            assert_eq!(p.machines, s.machines, "{k} week(s) back");
            assert_eq!(p.grades, s.grades, "{k}");
            assert_eq!(p.median_score, s.median_score, "{k}");
            assert_eq!(p.machines_flagged, s.machines_flagged, "{k}");
            assert_eq!(
                (p.critical, p.warnings, p.info),
                (s.critical, s.warnings, s.info),
                "{k}"
            );
            assert_eq!(p.stale, s.stale, "{k}");
        }
        // And they differ from week to week, or this proved nothing.
        assert_ne!(points[0].machines, points[4].machines);
        assert_ne!(points[0].machines_flagged, points[4].machines_flagged);
    }

    /// Projected onto the caller's view: a filtered or scoped viewer's past is
    /// made of their machines, and a machine judged by its current tags brings
    /// its history with it.
    #[test]
    fn the_timeline_is_projected_onto_the_view() {
        let mut e = Estate::new();
        e.add_with("GLA-1", 1000.0, "2026-08-01T10:00:00Z", at(Some("glasgow")));
        e.add_with(
            "EDI-1",
            1000.0,
            "2026-08-01T10:00:00Z",
            at(Some("edinburgh")),
        );
        // Moved from Edinburgh to Glasgow last week.
        e.add_with(
            "MOVER",
            1000.0,
            "2026-08-01T10:00:00Z",
            at(Some("edinburgh")),
        );
        e.add_with("MOVER", 1000.0, "2026-09-05T10:00:00Z", |d| {
            at(Some("glasgow"))(d);
            hot(d);
        });
        // In Glasgow, but only measured this week: before then the view had
        // one machine, then two, never a third that wasn't measured yet.
        e.add_with(
            "GLA-NEW",
            1000.0,
            "2026-09-08T10:00:00Z",
            at(Some("glasgow")),
        );

        let (snap, t) = e.timeline(4);
        let glasgow = snap.filtered(&Filter {
            scopes: vec![BTreeMap::from([("site".into(), "glasgow".into())])],
            ..Default::default()
        });
        assert_eq!(glasgow.summary.machines, 3);
        let points = t.view(&glasgow, 4).points;
        let machines: Vec<usize> = points.iter().map(|p| p.machines).collect();
        assert_eq!(
            machines,
            [2, 2, 2, 2, 3],
            "GLA-1 and MOVER throughout, GLA-NEW now"
        );
        assert_eq!(
            points.last().unwrap().machines_flagged,
            glasgow.summary.machines_flagged
        );

        let edinburgh = snap.filtered(&Filter {
            tags: BTreeMap::from([("site".into(), "edinburgh".into())]),
            ..Default::default()
        });
        let machines: Vec<usize> = t
            .view(&edinburgh, 4)
            .points
            .iter()
            .map(|p| p.machines)
            .collect();
        assert_eq!(machines, [1, 1, 1, 1, 1], "the mover left with its history");

        // Nobody in view: nothing sent, rather than a line of zeros.
        let nobody = snap.filtered(&Filter {
            tags: BTreeMap::from([("site".into(), "dundee".into())]),
            ..Default::default()
        });
        assert!(t.view(&nobody, 4).points.is_empty());
        assert!(t.changes(&nobody, 1).baseline_empty);
    }

    /// Every category, on one estate measured a week ago and again now.
    #[test]
    fn changes_name_what_appeared_went_away_and_moved() {
        let mut e = Estate::new();
        let then = "2026-08-30T10:00:00Z";
        let now_ = "2026-09-06T10:00:00Z";
        let disk = |pct: f64| with_disks(json!([volume("C:\\", 100, pct)]));
        // Cooled down: its thermal finding is resolved.
        e.add_with("COOLED", 1000.0, then, hot);
        e.add("COOLED", 1000.0, now_);
        // Started throttling: a new finding.
        e.add("WARMED", 1000.0, then);
        e.add_with("WARMED", 1000.0, now_, hot);
        // Filling up: warning, then critical.
        e.add_with("FILLING", 1000.0, then, disk(8.0));
        e.add_with("FILLING", 1000.0, now_, disk(3.0));
        // Cleared some space: critical, then warning.
        e.add_with("CLEARED", 1000.0, then, disk(3.0));
        e.add_with("CLEARED", 1000.0, now_, disk(8.0));
        // Dropped a grade, C to D, which is also a new grade_low.
        e.add("SLIPPED", 700.0, then);
        e.add("SLIPPED", 500.0, now_);
        // Up a grade, B to A.
        e.add("ROSE", 1100.0, then);
        e.add("ROSE", 1200.0, now_);
        // New to the fleet, and throttling.
        e.add_with("ARRIVED", 1000.0, now_, hot);

        let (snap, t) = e.timeline(4);
        let c = t.changes(&snap, 1);
        assert!(!c.baseline_empty);
        assert_eq!(
            (c.from.as_str(), c.to.as_str()),
            ("2026-09-02", "2026-09-09")
        );

        let new: Vec<(&str, &str)> = c
            .new_findings
            .iter()
            .filter(|f| f.severity != Severity::Info)
            .map(|f| (f.hostname.as_deref().unwrap(), f.code))
            .collect();
        assert_eq!(
            new,
            [
                // 700 to 500 is also a 29% regression, which is critical and
                // so comes first.
                ("SLIPPED", "regressed"),
                ("ARRIVED", "thermal_limited"),
                ("SLIPPED", "grade_low"),
                ("WARMED", "thermal_limited"),
            ]
        );
        assert!(
            c.new_findings
                .iter()
                .filter(|f| f.hostname.as_deref() == Some("ARRIVED"))
                .all(|f| f.new_machine)
        );
        assert!(
            c.new_findings
                .iter()
                .filter(|f| f.hostname.as_deref() != Some("ARRIVED"))
                .all(|f| !f.new_machine)
        );

        let resolved: Vec<(&str, &str)> = c
            .resolved
            .iter()
            .map(|f| (f.hostname.as_deref().unwrap(), f.code))
            .collect();
        assert_eq!(resolved, [("COOLED", "thermal_limited")]);

        assert_eq!(c.worsened.len(), 1);
        let w = &c.worsened[0];
        assert_eq!(
            (w.hostname.as_deref(), w.code),
            (Some("FILLING"), "disk_nearly_full")
        );
        assert_eq!((w.from, w.to), (Severity::Warning, Severity::Critical));
        assert!(
            w.headline.contains("3.0%"),
            "as it reads now: {}",
            w.headline
        );

        assert_eq!(c.eased.len(), 1);
        let ez = &c.eased[0];
        assert_eq!(
            (ez.hostname.as_deref(), ez.from, ez.to),
            (Some("CLEARED"), Severity::Critical, Severity::Warning)
        );

        let added: Vec<&str> = c
            .machines_added
            .iter()
            .map(|m| m.hostname.as_deref().unwrap())
            .collect();
        assert_eq!(added, ["ARRIVED"]);
        assert!(c.machines_added[0].flagged);

        let grades: Vec<(&str, &str, &str, timeline::GradeDirection)> = c
            .grade_changes
            .iter()
            .map(|g| {
                (
                    g.hostname.as_deref().unwrap(),
                    g.from.as_str(),
                    g.to.as_str(),
                    g.direction,
                )
            })
            .collect();
        assert_eq!(
            grades,
            [
                ("SLIPPED", "C", "D", timeline::GradeDirection::Down),
                ("ROSE", "B", "A", timeline::GradeDirection::Up),
            ],
            "down before up"
        );

        let n = &c.counts;
        assert_eq!((n.new_findings.critical, n.new_findings.warning), (1, 2));
        assert_eq!(n.new_machine_findings.warning, 1);
        assert_eq!((n.resolved.critical, n.resolved.warning), (0, 1));
        assert_eq!((n.worsened, n.eased), (1, 1));
        assert_eq!((n.machines_added, n.machines_added_flagged), (1, 1));
        assert_eq!((n.grades_down, n.grades_up), (1, 1));
    }

    /// Critical, then warning, then informational — so a `stale` appearing on
    /// a dozen machines lands under the one drive that failed, not over it —
    /// and by machine within each.
    #[test]
    fn changes_are_ordered_most_severe_first_then_by_machine() {
        let mut e = Estate::new();
        let then = "2026-08-30T10:00:00Z";
        let now_ = "2026-09-06T10:00:00Z";
        for host in ["ZED", "ALPHA", "MIKE"] {
            e.add(host, 1000.0, then);
        }
        e.add_with(
            "ZED",
            1000.0,
            now_,
            with_disks(json!([volume("C:\\", 100, 2.0)])),
        );
        e.add_with("ALPHA", 1000.0, now_, hot);
        e.add_with("MIKE", 1000.0, now_, |d| {
            hot(d);
            make_cpu_noisy(d);
        });

        let (snap, t) = e.timeline(4);
        let c = t.changes(&snap, 1);
        let order: Vec<(Severity, &str)> = c
            .new_findings
            .iter()
            .map(|f| (f.severity, f.hostname.as_deref().unwrap()))
            .collect();
        assert_eq!(
            order,
            [
                (Severity::Critical, "ZED"),
                (Severity::Warning, "ALPHA"),
                (Severity::Warning, "MIKE"),
                (Severity::Info, "MIKE"),
            ]
        );
        assert_eq!(c.counts.new_findings.info, 1);
    }

    /// With no data that far back, there is no baseline — which is said as
    /// that, rather than as every machine and every finding being new.
    #[test]
    fn a_window_reaching_before_the_data_has_no_baseline() {
        let mut e = Estate::new();
        e.add_with("PC-1", 1000.0, "2026-08-20T10:00:00Z", hot);
        let (snap, t) = e.timeline(TIMELINE_WEEKS);

        let quarter = t.changes(&snap, 13);
        assert!(quarter.baseline_empty);
        assert!(quarter.is_empty());
        assert_eq!(quarter.counts.machines_added, 0);
        assert_eq!(quarter.from, "2026-06-10");

        let week = t.changes(&snap, 1);
        assert!(!week.baseline_empty);
        assert!(week.is_empty(), "the same run a week ago and now: {week:?}");
    }

    #[test]
    fn only_the_three_windows_and_a_sane_number_of_weeks_are_accepted() {
        for ok in ["1", "4", "13", " 4 "] {
            assert!(parse_window(ok).is_ok(), "{ok}");
        }
        for bad in ["0", "2", "26", "-1", "week", ""] {
            let err = parse_window(bad).unwrap_err();
            assert!(err.contains("1, 4 or 13"), "{bad}: {err}");
        }
        assert_eq!(parse_weeks("4"), Ok(4));
        assert_eq!(parse_weeks(&TIMELINE_WEEKS.to_string()), Ok(TIMELINE_WEEKS));
        for bad in ["3", "0", "105", "many"] {
            assert!(parse_weeks(bad).is_err(), "{bad}");
        }
    }

    /// What a refresh costs at a realistic size, and what the result holds.
    /// Ignored by default — it ingests sixteen thousand documents. Run it in a
    /// release build:
    ///
    ///     cargo test --release timeline_build_cost -- --ignored --nocapture
    ///
    /// `LBF_BENCH_RUNS` and `LBF_BENCH_WEEKS` change the runs per machine (8)
    /// and the weeks built (`TIMELINE_WEEKS`), to try a longer history.
    #[test]
    #[ignore]
    fn timeline_build_cost() {
        let env = |name: &str, default: usize| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let mut e = Estate::new();
        let machines = 2000;
        let runs = env("LBF_BENCH_RUNS", 8);
        let weeks = env("LBF_BENCH_WEEKS", TIMELINE_WEEKS);
        let started = std::time::Instant::now();
        for i in 0..machines {
            for r in 0..runs {
                // One run every three weeks, staggered across machines, so the
                // half-year holds most of each machine's history.
                let taken = now() - time::Duration::days((r * 21 + i % 21) as i64);
                let score = 900.0 + ((i * 37 + r * 11) % 300) as f64;
                e.add_with(
                    &format!("PC-{i:05}"),
                    score,
                    &taken.format(&Rfc3339).unwrap(),
                    |d| {
                        d["machine"]["cpu_model"] = json!(format!("CPU model {}", i % 40));
                        if i % 7 == 0 {
                            hot(d);
                        }
                        d["tags"] = json!({ "site": format!("site-{}", i % 12) });
                    },
                );
            }
        }
        eprintln!("ingest: {:?}", started.elapsed());

        let th = Tuning::default();
        let started = std::time::Instant::now();
        let snap = snapshot(e.idx.conn(), &th, now()).unwrap();
        eprintln!("snapshot: {:?}", started.elapsed());
        let started = std::time::Instant::now();
        let t = Timeline::build(e.idx.conn(), &th, &snap, now(), weeks).unwrap();
        eprintln!(
            "timeline ({weeks} weeks, {machines} machines x {runs} runs): {:?}, about {} KiB held",
            started.elapsed(),
            t.heap_bytes() / 1024
        );
        let started = std::time::Instant::now();
        let view = t.view(&snap, weeks);
        let changes = t.changes(&snap, 13);
        eprintln!(
            "projection and changes: {:?} ({} points, {} new findings)",
            started.elapsed(),
            view.points.len(),
            changes.new_findings.len()
        );
        assert_eq!(view.points.last().unwrap().machines, machines);
    }

    /* --------------------------------------------------------- lifecycle */

    /// The CPU launched in this month, as loadbearer 1.6.0+ records it. The
    /// fixture's own CPU launched in 2023-01, which on `now()` is a machine
    /// well under the five-year line.
    fn launched(ym: &'static str) -> impl FnOnce(&mut Value) {
        move |d: &mut Value| {
            d["machine"]["age"] = json!({ "as_of": "2026-09", "cpu_launched": ym });
        }
    }

    /// A battery under `battery_health_pct`, on a run that was on mains — a
    /// part problem and no measurement caveat.
    fn worn_battery(d: &mut Value) {
        d["machine"]["battery"] = json!({
            "charge_pct": 80.0, "state": "charging", "health_pct": 61.0, "cycle_count": 812
        });
    }

    fn decided<'a>(s: &'a Snapshot, host: &str) -> &'a Lifecycle {
        &machine(s, host).lifecycle
    }

    /// A CPU of its own, launched in `ym`: no peers, so no cohort finding can
    /// join the evidence and a test of one rule tests only that rule.
    fn own_cpu<'a>(host: &'a str, ym: &'static str) -> impl FnOnce(&mut Value) + use<'a> {
        move |d: &mut Value| {
            d["machine"]["cpu_model"] = json!(format!("Test CPU for {host}"));
            launched(ym)(d);
        }
    }

    /// Each rule, and which wins when more than one could. One estate, so the
    /// precedence is read off side by side rather than spread over a dozen
    /// tests that could each pass for a different reason.
    #[test]
    fn each_lifecycle_rule_and_its_precedence() {
        let mut e = Estate::new();
        let at = "2026-09-08T10:00:00Z";
        let (young, old) = ("2023-01", "2019-01");
        e.add_with("F-YOUNG", 300.0, at, own_cpu("F-YOUNG", young));
        e.add_with("D-OLD", 500.0, at, own_cpu("D-OLD", old));
        e.add_with("BATTERY-OLD", 1000.0, at, |d| {
            own_cpu("BATTERY-OLD", old)(d);
            worn_battery(d);
        });
        e.add_with("BATTERY-YOUNG", 1000.0, at, |d| {
            own_cpu("BATTERY-YOUNG", young)(d);
            worn_battery(d);
        });
        e.add_with("SOUND-OLD", 1000.0, at, own_cpu("SOUND-OLD", old));
        e.add_with("D-YOUNG", 500.0, at, own_cpu("D-YOUNG", young));
        e.add_with("CLEAN", 1000.0, at, own_cpu("CLEAN", young));
        // Regressed by 20% against its own past, with a drive most of the way
        // through its rated endurance: a part *and* a slowdown.
        for (score, taken) in [
            (1000.0, "2026-07-01T10:00:00Z"),
            (1000.0, "2026-08-01T10:00:00Z"),
        ] {
            e.add_with(
                "DRIVE-REGRESSED",
                score,
                taken,
                own_cpu("DRIVE-REGRESSED", young),
            );
        }
        e.add_with("DRIVE-REGRESSED", 800.0, at, |d| {
            own_cpu("DRIVE-REGRESSED", young)(d);
            with_drives(vec![nvme_with("Boot SSD", "percentage_used", json!(93))])(d);
        });

        let s = e.snap();
        let want = [
            ("F-YOUNG", Verdict::Replace),
            ("D-OLD", Verdict::Replace),
            ("BATTERY-OLD", Verdict::Replace),
            ("BATTERY-YOUNG", Verdict::Repair),
            ("DRIVE-REGRESSED", Verdict::Repair),
            ("SOUND-OLD", Verdict::Watch),
            ("D-YOUNG", Verdict::Watch),
            ("CLEAN", Verdict::Keep),
        ];
        for (host, verdict) in want {
            assert_eq!(
                decided(&s, host).verdict,
                verdict,
                "{host}: {:?}",
                decided(&s, host)
            );
        }

        assert_eq!(
            decided(&s, "F-YOUNG").reasons,
            ["Graded F against the reference baseline"],
            "an F is the whole reason, and says what it is measured against"
        );
        assert_eq!(
            decided(&s, "DRIVE-REGRESSED").reasons,
            [
                "Boot SSD has used 93% of its rated endurance",
                "20% slower than its own earlier runs"
            ],
            "the part that decided it first, and the slowdown still said"
        );
        assert_eq!(
            decided(&s, "SOUND-OLD").reasons,
            ["At most 7.7 years old, by its CPU's launch date"]
        );
        assert!(
            decided(&s, "CLEAN").reasons.is_empty(),
            "keep has no reasons"
        );
        assert!(
            s.machines.iter().all(|m| !m.lifecycle.uncertain),
            "nothing here carries a measurement caveat"
        );
    }

    /// Slower than identical machines and nothing else wrong: find out why
    /// before spending anything — even on a machine old enough to watch.
    #[test]
    fn a_machine_slower_than_its_clones_is_investigated() {
        let mut e = Estate::new();
        for i in 0..5 {
            e.add(&format!("PC-{i}"), 1000.0, "2026-09-08T10:00:00Z");
        }
        e.add("PC-SLOW", 700.0, "2026-09-08T10:00:00Z");
        e.add_with("OLD-0", 1000.0, "2026-09-08T10:00:00Z", |d| {
            d["machine"]["cpu_model"] = json!("Some Other CPU");
            launched("2019-01")(d);
        });

        let s = e.snap();
        let slow = decided(&s, "PC-SLOW");
        assert_eq!(slow.verdict, Verdict::Investigate);
        assert_eq!(slow.reasons, ["30% slower than identical machines"]);
        assert_eq!(decided(&s, "PC-0").verdict, Verdict::Keep);

        // The same shortfall on old hardware: still investigate, since the
        // machine is slower than its own hardware should be, and the age
        // follows the reason that decided it.
        let mut e = Estate::new();
        for i in 0..5 {
            e.add_with(
                &format!("PC-{i}"),
                1000.0,
                "2026-09-08T10:00:00Z",
                launched("2019-01"),
            );
        }
        e.add_with(
            "PC-SLOW",
            700.0,
            "2026-09-08T10:00:00Z",
            launched("2019-01"),
        );
        let s = e.snap();
        assert_eq!(decided(&s, "PC-0").verdict, Verdict::Watch);
        let slow = decided(&s, "PC-SLOW");
        assert_eq!(slow.verdict, Verdict::Investigate);
        assert_eq!(
            slow.reasons,
            [
                "30% slower than identical machines",
                "At most 7.7 years old, by its CPU's launch date"
            ]
        );
    }

    /// Five years exactly is old; a month short of it is not. Months are what
    /// the age is counted in, so that is the resolution of the boundary.
    #[test]
    fn the_age_boundary_is_inclusive_at_the_threshold() {
        let mut e = Estate::new();
        // `now()` is 2026-09: sixty months after 2021-09, fifty-nine after 2021-10.
        e.add_with("SIXTY", 500.0, "2026-09-08T10:00:00Z", launched("2021-09"));
        e.add_with(
            "FIFTY-NINE",
            500.0,
            "2026-09-08T10:00:00Z",
            launched("2021-10"),
        );
        let s = e.snap();
        assert_eq!(machine(&s, "SIXTY").age_estimate.unwrap().months, 60);
        assert_eq!(decided(&s, "SIXTY").verdict, Verdict::Replace);
        assert_eq!(decided(&s, "FIFTY-NINE").verdict, Verdict::Watch);
        assert_eq!(
            decided(&s, "FIFTY-NINE").reasons,
            ["Graded D against the reference baseline"],
            "a young D is watched for its grade alone"
        );
    }

    #[test]
    fn the_best_age_is_a_firmware_dating_inside_the_cpu_bound() {
        let now = datetime!(2026-09-09 12:00:00 UTC);
        let upper = |months| {
            Some(AgeEstimate {
                months,
                basis: AgeBasis::UpperBound,
            })
        };
        let dated = |months| {
            Some(AgeEstimate {
                months,
                basis: AgeBasis::Firmware,
            })
        };

        // The CPU alone bounds it.
        assert_eq!(best_age(Some("2020-09"), None, None, now), upper(72));
        // Original firmware dates it, more closely.
        assert_eq!(
            best_age(Some("2020-09"), Some("2021-06"), Some(true), now),
            dated(63)
        );
        // Firmware updated since it shipped says nothing about the machine.
        assert_eq!(
            best_age(Some("2020-09"), Some("2025-06"), Some(false), now),
            upper(72)
        );
        assert_eq!(
            best_age(Some("2020-09"), Some("2021-06"), None, now),
            upper(72),
            "not judged original is not original"
        );
        // A machine cannot predate its CPU, so firmware claiming it does is
        // the firmware being wrong, and the bound stands.
        assert_eq!(
            best_age(Some("2020-09"), Some("2019-01"), Some(true), now),
            upper(72)
        );
        // A BIOS date in full reads as its month.
        assert_eq!(
            best_age(Some("2020-09"), Some("2021-06-02"), Some(true), now),
            dated(63)
        );
        // Original firmware on a CPU the launch table doesn't know still dates it.
        assert_eq!(best_age(None, Some("2022-09"), Some(true), now), dated(48));
        // A date in the future is no date.
        assert_eq!(
            best_age(Some("2020-09"), Some("2027-01"), Some(true), now),
            upper(72)
        );
        // Nothing to go on: no answer rather than a guess.
        assert_eq!(best_age(None, None, None, now), None);
        assert_eq!(best_age(None, Some("2021-06"), Some(false), now), None);

        assert_eq!(
            upper(74).unwrap().sentence(),
            "At most 6.2 years old, by its CPU's launch date"
        );
        assert_eq!(
            dated(49).unwrap().sentence(),
            "About 4.1 years old, by its original firmware's date"
        );
    }

    /// Through the real ingest path: the age block's original firmware dates
    /// the machine, and the reason says it is a dating rather than a bound.
    #[test]
    fn a_firmware_dating_reaches_the_verdict_and_its_wording() {
        let mut e = Estate::new();
        e.add_with("DATED", 500.0, "2026-09-08T10:00:00Z", |d| {
            d["machine"]["age"] = json!({
                "as_of": "2026-09", "cpu_launched": "2019-01",
                "firmware_dated": "2020-05", "firmware_likely_original": true
            });
        });
        let s = e.snap();
        let m = machine(&s, "DATED");
        assert_eq!(
            m.age_estimate,
            Some(AgeEstimate {
                months: 76,
                basis: AgeBasis::Firmware
            })
        );
        assert_eq!(m.lifecycle.verdict, Verdict::Replace);
        assert_eq!(
            m.lifecycle.reasons,
            [
                "About 6.3 years old, by its original firmware's date",
                "Graded D against the reference baseline"
            ]
        );
    }

    /// A CPU the launch table doesn't know and no firmware dating: the machine
    /// has no age, and that must never be read as old.
    #[test]
    fn no_age_evidence_never_makes_a_machine_old() {
        let mut e = Estate::new();
        let vm = |d: &mut Value| {
            d["machine"]["cpu_model"] = json!("QEMU Virtual CPU version 2.5+");
        };
        e.add_with("VM-D", 500.0, "2026-09-08T10:00:00Z", vm);
        e.add_with("VM-BATTERY", 1000.0, "2026-09-08T10:00:00Z", |d| {
            vm(d);
            worn_battery(d);
        });
        e.add_with("VM-OK", 1000.0, "2026-09-08T10:00:00Z", vm);
        let s = e.snap();
        assert!(s.machines.iter().all(|m| m.age_estimate.is_none()));
        assert_eq!(decided(&s, "VM-D").verdict, Verdict::Watch);
        assert_eq!(decided(&s, "VM-BATTERY").verdict, Verdict::Repair);
        assert_eq!(decided(&s, "VM-OK").verdict, Verdict::Keep);
    }

    /// A measurement caveat makes a verdict uncertain only where a performance
    /// figure decided it: a throttled run cannot cast doubt on a worn battery.
    #[test]
    fn a_verdict_resting_on_a_caveated_measurement_is_uncertain() {
        let mut e = Estate::new();
        let at = "2026-09-08T10:00:00Z";
        // Old and graded D: without the grade it would only be watched.
        e.add_with("OLD-D-HOT", 500.0, at, |d| {
            launched("2019-01")(d);
            hot(d);
        });
        // Old with a worn battery: replaced whatever the run measured.
        e.add_with("OLD-BATTERY-HOT", 500.0, at, |d| {
            launched("2019-01")(d);
            worn_battery(d);
            hot(d);
        });
        // Graded D, measured on battery.
        e.add_with("D-ON-BATTERY", 500.0, at, |d| {
            d["gates"] = json!({ "on_ac": false });
        });
        // Nothing performance-related decided it, so nothing to doubt.
        e.add_with("CLEAN-HOT", 1000.0, at, hot);
        e.add_with("OLD-D-COOL", 500.0, at, launched("2019-01"));

        let s = e.snap();
        let hot_d = decided(&s, "OLD-D-HOT");
        assert_eq!(hot_d.verdict, Verdict::Replace);
        assert!(hot_d.uncertain, "{hot_d:?}");
        assert_eq!(hot_d.caveats, ["Clocks fell during the run"]);

        let parts = decided(&s, "OLD-BATTERY-HOT");
        assert_eq!(parts.verdict, Verdict::Replace);
        assert!(!parts.uncertain, "age and a part decide it: {parts:?}");
        assert!(parts.caveats.is_empty());

        let on_battery = decided(&s, "D-ON-BATTERY");
        assert_eq!(on_battery.verdict, Verdict::Watch);
        assert!(on_battery.uncertain);
        assert_eq!(on_battery.caveats, ["Measured on battery"]);

        assert_eq!(decided(&s, "CLEAN-HOT").verdict, Verdict::Keep);
        assert!(!decided(&s, "CLEAN-HOT").uncertain);
        assert!(!decided(&s, "OLD-D-COOL").uncertain, "no caveat, no doubt");
    }

    /// A weak component is a part finding, but one read off the benchmark:
    /// a disk graded F on a throttled run is doubted like a low grade is,
    /// where a worn battery on the same run is not.
    #[test]
    fn a_weak_component_on_a_caveated_run_is_uncertain() {
        let weak_disk = |d: &mut Value| {
            for c in d["components"].as_array_mut().expect("array") {
                if c["id"] == json!("disk") {
                    c["grade"] = json!("F");
                    c["score"] = json!(180.0);
                }
            }
        };
        let mut e = Estate::new();
        let at = "2026-09-08T10:00:00Z";
        e.add_with("WEAK-HOT", 1000.0, at, |d| {
            weak_disk(d);
            hot(d);
        });
        e.add_with("WEAK-COOL", 1000.0, at, weak_disk);
        // The battery decides it with or without the disk figure.
        e.add_with("WEAK-BATTERY-HOT", 1000.0, at, |d| {
            weak_disk(d);
            worn_battery(d);
            hot(d);
        });

        let s = e.snap();
        let hot_weak = decided(&s, "WEAK-HOT");
        assert_eq!(hot_weak.verdict, Verdict::Repair);
        assert!(hot_weak.uncertain, "{hot_weak:?}");
        assert_eq!(hot_weak.caveats, ["Clocks fell during the run"]);

        let cool = decided(&s, "WEAK-COOL");
        assert_eq!(cool.verdict, Verdict::Repair);
        assert!(!cool.uncertain, "no caveat, no doubt");

        let both = decided(&s, "WEAK-BATTERY-HOT");
        assert_eq!(both.verdict, Verdict::Repair);
        assert!(!both.uncertain, "the battery stands: {both:?}");
    }

    /// Every piece of evidence, the decisive first: on a replace, the F, then
    /// the age, then the parts — those in the findings' own order, most
    /// severe first.
    #[test]
    fn reasons_lead_with_what_decided_the_verdict() {
        let mut e = Estate::new();
        e.add_with("EVERYTHING", 300.0, "2026-09-08T10:00:00Z", |d| {
            launched("2019-01")(d);
            worn_battery(d);
            with_drives(vec![
                nvme_with("Boot SSD", "media_errors", json!(2)),
                nvme_with("Data SSD", "critical_warning", json!(4)),
            ])(d);
        });
        let s = e.snap();
        let l = decided(&s, "EVERYTHING");
        assert_eq!(l.verdict, Verdict::Replace);
        assert_eq!(
            l.reasons,
            [
                "Graded F against the reference baseline",
                "At most 7.7 years old, by its CPU's launch date",
                "Data SSD is reporting a critical warning",
                "Battery at 61% of design capacity",
                "Boot SSD has recorded 2 media error(s)",
            ]
        );
    }

    /// The counts add up to the machines in view, in the summary and after a
    /// filter; a group counts its own; and the filter selects a verdict.
    #[test]
    fn lifecycle_counts_add_up_and_the_filter_selects_a_verdict() {
        let mut e = Estate::new();
        let at = "2026-09-08T10:00:00Z";
        e.add_with("GLA-F", 300.0, at, tagged("glasgow"));
        e.add_with("GLA-BATTERY", 1000.0, at, |d| {
            tagged("glasgow")(d);
            worn_battery(d);
        });
        e.add_with("GLA-OK", 1000.0, at, tagged("glasgow"));
        e.add_with("EDI-F", 300.0, at, tagged("edinburgh"));
        e.add_with("EDI-D", 500.0, at, tagged("edinburgh"));

        let s = e.snap();
        let c = s.summary.lifecycle;
        assert_eq!(
            (c.replace, c.repair, c.investigate, c.watch, c.keep),
            (2, 1, 0, 1, 1)
        );
        assert_eq!(
            c.replace + c.repair + c.investigate + c.watch + c.keep,
            s.summary.machines
        );

        let gs = groups(&s, &site_tag());
        let gla = group(&gs, Some("glasgow"));
        assert_eq!((gla.replace, gla.repair), (1, 1));
        let edi = group(&gs, Some("edinburgh"));
        assert_eq!((edi.replace, edi.repair), (1, 0));

        let replace = s.filtered(&Filter {
            lifecycle: Some(Verdict::Replace),
            ..Default::default()
        });
        let mut hosts: Vec<_> = replace
            .machines
            .iter()
            .filter_map(|m| m.hostname.as_deref())
            .collect();
        hosts.sort_unstable();
        assert_eq!(hosts, ["EDI-F", "GLA-F"]);
        assert_eq!(replace.summary.lifecycle.replace, 2);
        assert_eq!(replace.summary.machines, 2);
        assert_eq!(replace.summary.lifecycle.keep, 0);
        assert!(
            replace
                .flags
                .iter()
                .all(|f| f.hostname.as_deref() != Some("GLA-BATTERY")),
            "the findings narrow with the machines"
        );

        // With another filter, both apply.
        let mut tags = BTreeMap::new();
        tags.insert("site".to_string(), "glasgow".to_string());
        let glasgow_replace = s.filtered(&Filter {
            tags,
            lifecycle: Some(Verdict::Replace),
            ..Default::default()
        });
        assert_eq!(glasgow_replace.summary.machines, 1);

        let report = lifecycle_report(&s);
        assert_eq!(report.machines, 5);
        let order: Vec<_> = report
            .candidates
            .iter()
            .map(|c| (c.lifecycle.verdict, c.hostname.as_deref().unwrap()))
            .collect();
        assert_eq!(
            order,
            [
                (Verdict::Replace, "EDI-F"),
                (Verdict::Replace, "GLA-F"),
                (Verdict::Repair, "GLA-BATTERY"),
                (Verdict::Watch, "EDI-D"),
            ],
            "most urgent first, then by name, and keep only counted"
        );
    }

    #[test]
    fn a_verdict_parses_from_its_own_name_only() {
        for v in Verdict::ALL {
            assert_eq!(v.as_str().parse::<Verdict>(), Ok(v));
            assert_eq!(
                serde_json::to_value(v).unwrap(),
                json!(v.as_str()),
                "the query parameter and the JSON spell it the same"
            );
        }
        let err = "retire".parse::<Verdict>().unwrap_err();
        assert!(
            err.contains("replace, repair, investigate, watch or keep"),
            "{err}"
        );
        assert!(
            "Replace".parse::<Verdict>().is_err(),
            "exact, like the JSON"
        );
    }

    // --- Triage ------------------------------------------------------------

    /// `RULES` is the list a suppression is checked against, so it has to be
    /// exactly what `evaluate` can raise. Read from `evaluate`'s own source
    /// rather than from a fleet, because no fixture estate raises every rule
    /// — and a code missing here is a rule nobody can suppress, while a code
    /// listed and never raised is a suppression that silently does nothing.
    #[test]
    fn every_code_evaluate_can_raise_is_in_rules_and_nothing_else_is() {
        // A Windows checkout may have CRLF line endings, and the markers
        // below are written with LF.
        let src = include_str!("analytics.rs").replace("\r\n", "\n");
        let start = src
            .find("\nfn evaluate(")
            .expect("evaluate is in this file");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("evaluate ends")];

        // Every call of the local `push` closure — not `flags.push` or
        // `why.push` — names its rule as the first string literal it is
        // handed; the severity and the queue before it are enum values.
        let mut raised = std::collections::BTreeSet::new();
        let mut rest = body;
        while let Some(at) = rest.find("push(") {
            let before = rest[..at].chars().last();
            rest = &rest[at + "push(".len()..];
            if before.is_some_and(|c| c == '.' || c == '_' || c.is_alphanumeric()) {
                continue;
            }
            if rest.trim_start().starts_with("Flag {") {
                continue;
            }
            let open = rest.find('"').expect("a code literal after push(");
            let close = open + 1 + rest[open + 1..].find('"').expect("closed");
            raised.insert(&rest[open + 1..close]);
        }
        let listed: std::collections::BTreeSet<&str> = RULES.iter().copied().collect();
        assert_eq!(
            raised, listed,
            "evaluate raises exactly the codes in RULES — add a new rule there too"
        );
        assert_eq!(listed.len(), RULES.len(), "no code listed twice");
        assert_eq!(rule("stale"), Some("stale"));
        assert_eq!(rule("stail"), None);
    }

    /// A machine whose `D:` is `free_pct` free — a warning under 10%, critical
    /// under 5%, nothing at 10 or more — tagged with a site, or untagged.
    fn disk_at(free_pct: f64, site: Option<&str>) -> impl FnOnce(&mut Value) {
        let site = site.map(str::to_string);
        move |d: &mut Value| {
            d["machine"]["disks"] =
                json!([volume("C:\\", 100, 40.0), volume("D:\\", 100, free_pct)]);
            d["tags"] = match site {
                Some(s) => json!({ "site": s }),
                None => json!({}),
            };
        }
    }

    fn book_of(
        marks: &[crate::state::Mark],
        rules: &[crate::state::Suppression],
    ) -> crate::state::Book {
        let store = crate::state::Store::open_in_memory().expect("store");
        for m in marks {
            store.put_mark(m).expect("mark");
        }
        for r in rules {
            store.add_suppression(r).expect("rule");
        }
        store.load().expect("load")
    }

    fn mark_on(
        key: &str,
        code: &str,
        state: crate::state::TriageState,
        severity: Severity,
        until: Option<time::Date>,
    ) -> crate::state::Mark {
        crate::state::Mark {
            machine_key: key.to_string(),
            code: code.to_string(),
            state,
            severity,
            until,
            note: Some("in hand".to_string()),
            by: "Iain".to_string(),
            by_subject: "sub-1".to_string(),
            at: now(),
        }
    }

    fn suppression(
        code: &str,
        machine_key: Option<&str>,
        tags: Option<&[(&str, &str)]>,
    ) -> crate::state::Suppression {
        crate::state::Suppression {
            id: 0,
            code: code.to_string(),
            machine_key: machine_key.map(str::to_string),
            tags: tags.map(|t| {
                t.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            }),
            note: None,
            by: "Iain".to_string(),
            by_subject: "sub-1".to_string(),
            at: "2026-09-09T12:00:00Z".to_string(),
        }
    }

    fn triage_of(s: &Snapshot, host: &str) -> Option<crate::state::TriageState> {
        flag_on(s, "disk_nearly_full", host)
            .expect("the finding is still there, whatever its triage")
            .triage
            .as_ref()
            .map(|t| t.state)
    }

    /// An acknowledgement quiets a finding and changes nothing else: the
    /// finding is still in the snapshot, the lifecycle verdicts are the same,
    /// and only the attention counts — summary and groups — move, into
    /// `quieted`.
    #[test]
    fn a_quieted_finding_leaves_the_counts_that_need_attention_and_nothing_else() {
        use crate::state::TriageState::Acknowledged;
        let mut e = Estate::new();
        e.add_with(
            "FULL",
            1000.0,
            "2026-09-01T10:00:00Z",
            disk_at(8.0, Some("glasgow")),
        );
        e.add_with(
            "ALSO-FULL",
            1000.0,
            "2026-09-01T10:00:00Z",
            disk_at(8.0, Some("glasgow")),
        );
        let raw = e.snap();
        let key = machine(&raw, "FULL").key.clone();
        let book = book_of(
            &[mark_on(
                &key,
                "disk_nearly_full",
                Acknowledged,
                Severity::Warning,
                None,
            )],
            &[],
        );
        let plain = raw.filtered_with(&Filter::default(), &Default::default(), now());
        let view = raw.filtered_with(&Filter::default(), &book, now());

        assert_eq!(triage_of(&view, "FULL"), Some(Acknowledged));
        assert_eq!(
            triage_of(&view, "ALSO-FULL"),
            None,
            "another machine's is its own"
        );
        assert_eq!(
            view.flags.len(),
            plain.flags.len(),
            "quieted is not removed"
        );
        assert_eq!(view.summary.warnings, plain.summary.warnings - 1);
        assert_eq!(view.summary.quieted.acknowledged, 1);
        assert_eq!(view.summary.quieted.total(), 1);
        let others_need_attention = view
            .flags
            .iter()
            .any(|f| f.machine_key == key && f.is_live() && needs_attention(f.severity));
        assert_eq!(
            view.summary.machines_flagged,
            plain.summary.machines_flagged - usize::from(!others_need_attention)
        );
        for (a, b) in view.machines.iter().zip(&plain.machines) {
            assert_eq!(
                a.lifecycle.verdict, b.lifecycle.verdict,
                "triage is attention, not a verdict"
            );
        }
        assert_eq!(view.summary.lifecycle, plain.summary.lifecycle);

        let site = GroupBy::Tag("site".into());
        let (g, p) = (groups(&view, &site), groups(&plain, &site));
        let (g, p) = (group(&g, Some("glasgow")), group(&p, Some("glasgow")));
        assert_eq!(g.warnings, p.warnings - 1);
        assert_eq!(g.quieted.acknowledged, 1);
        assert_eq!(g.flagged, p.flagged - usize::from(!others_need_attention));

        // `flag=` is a question about the machines, and still finds it.
        let by_flag = raw.filtered_with(
            &Filter {
                flag: Some("disk_nearly_full".into()),
                ..Filter::default()
            },
            &book,
            now(),
        );
        assert!(by_flag.machines.iter().any(|m| m.key == key));
    }

    /// A snooze ends by the clock the view is produced with — no rescan, no
    /// restart — from the start of its `until` day, UTC.
    #[test]
    fn a_snooze_ends_by_the_clock_without_a_rescan() {
        use crate::state::TriageState::Snoozed;
        let mut e = Estate::new();
        e.add_with("FULL", 1000.0, "2026-09-01T10:00:00Z", disk_at(8.0, None));
        let raw = e.snap();
        let key = machine(&raw, "FULL").key.clone();
        let until = time::macros::date!(2026 - 09 - 12);
        let book = book_of(
            &[mark_on(
                &key,
                "disk_nearly_full",
                Snoozed,
                Severity::Warning,
                Some(until),
            )],
            &[],
        );
        let at = |t: OffsetDateTime| raw.filtered_with(&Filter::default(), &book, t);

        assert_eq!(triage_of(&at(now()), "FULL"), Some(Snoozed));
        assert_eq!(
            triage_of(&at(datetime!(2026-09-11 23:59:59 UTC)), "FULL"),
            Some(Snoozed)
        );
        let back = at(datetime!(2026-09-12 00:00:00 UTC));
        assert_eq!(triage_of(&back, "FULL"), None, "back on the day it ends");
        assert_eq!(back.summary.quieted.total(), 0);
        assert_eq!(
            book.spent(&raw, until),
            vec![(
                key,
                "disk_nearly_full".to_string(),
                crate::state::Spent::Expired
            )],
            "and dropped at the next rebuild"
        );
    }

    /// Worse than when somebody said they had seen it is news again; a
    /// suppression is a standing rule and does not care.
    #[test]
    fn escalation_lapses_an_acknowledgement_and_a_snooze_but_not_a_suppression() {
        use crate::state::TriageState::{Acknowledged, Snoozed, Suppressed};
        let mut e = Estate::new();
        for host in ["ACKED", "SNOOZED", "SUPPRESSED"] {
            e.add_with(host, 1000.0, "2026-09-01T10:00:00Z", disk_at(8.0, None));
        }
        let before = e.snap();
        let key = |h: &str| machine(&before, h).key.clone();
        let book = book_of(
            &[
                mark_on(
                    &key("ACKED"),
                    "disk_nearly_full",
                    Acknowledged,
                    Severity::Warning,
                    None,
                ),
                mark_on(
                    &key("SNOOZED"),
                    "disk_nearly_full",
                    Snoozed,
                    Severity::Warning,
                    Some(time::macros::date!(2026 - 12 - 01)),
                ),
            ],
            &[suppression(
                "disk_nearly_full",
                Some(&key("SUPPRESSED")),
                None,
            )],
        );
        let view = |s: &Snapshot| s.filtered_with(&Filter::default(), &book, now());
        let v = view(&before);
        assert_eq!(triage_of(&v, "ACKED"), Some(Acknowledged));
        assert_eq!(triage_of(&v, "SNOOZED"), Some(Snoozed));
        assert_eq!(triage_of(&v, "SUPPRESSED"), Some(Suppressed));

        // Each disk fills to critical.
        for host in ["ACKED", "SNOOZED", "SUPPRESSED"] {
            e.add_with(host, 1000.0, "2026-09-05T10:00:00Z", disk_at(3.0, None));
        }
        let after = e.snap();
        let v = view(&after);
        assert_eq!(
            flag_on(&v, "disk_nearly_full", "ACKED").unwrap().severity,
            Severity::Critical
        );
        assert_eq!(triage_of(&v, "ACKED"), None);
        assert_eq!(triage_of(&v, "SNOOZED"), None);
        assert_eq!(triage_of(&v, "SUPPRESSED"), Some(Suppressed));
        let spent = book.spent(&after, now().date());
        assert_eq!(spent.len(), 2);
        assert!(
            spent
                .iter()
                .all(|(_, _, why)| *why == crate::state::Spent::Escalated)
        );
    }

    /// An acknowledgement is of *this* occurrence. Resolved, it is spent; when
    /// the same rule fires again on the same machine months later, that is
    /// news, not something already in hand.
    #[test]
    fn an_acknowledgement_does_not_survive_resolve_then_recur() {
        use crate::state::TriageState::Acknowledged;
        let mut e = Estate::new();
        e.add_with("FULL", 1000.0, "2026-09-01T10:00:00Z", disk_at(8.0, None));
        e.add("ELSEWHERE", 1000.0, "2026-09-01T10:00:00Z");
        let first = e.snap();
        let key = machine(&first, "FULL").key.clone();
        let book = book_of(
            &[mark_on(
                &key,
                "disk_nearly_full",
                Acknowledged,
                Severity::Warning,
                None,
            )],
            &[],
        );

        // Somebody clears the disk.
        e.add_with("FULL", 1000.0, "2026-09-03T10:00:00Z", disk_at(40.0, None));
        let resolved = e.snap();
        let spent = book.spent(&resolved, now().date());
        assert_eq!(
            spent,
            vec![(
                key.clone(),
                "disk_nearly_full".to_string(),
                crate::state::Spent::Resolved
            )]
        );
        let book = book.without(&spent);

        // And it fills again.
        e.add_with("FULL", 1000.0, "2026-09-07T10:00:00Z", disk_at(8.0, None));
        let recurred = e.snap().filtered_with(&Filter::default(), &book, now());
        assert_eq!(triage_of(&recurred, "FULL"), None, "a recurrence is news");

        // A machine missing altogether is not a finding resolved: an index
        // superseded by an upgrade is empty until its first scan.
        let book = book_of(
            &[mark_on(
                &key,
                "disk_nearly_full",
                Acknowledged,
                Severity::Warning,
                None,
            )],
            &[],
        );
        let empty = Estate::new().snap();
        assert!(book.spent(&empty, now().date()).is_empty());
    }

    /// A suppression reaches one machine, the machines carrying a set of
    /// tags, or the whole fleet — and wins over a snooze or acknowledgement.
    #[test]
    fn a_suppression_reaches_one_machine_some_tags_or_the_fleet() {
        use crate::state::TriageState::{Acknowledged, Suppressed};
        let mut e = Estate::new();
        e.add_with(
            "GLA",
            1000.0,
            "2026-09-01T10:00:00Z",
            disk_at(8.0, Some("glasgow")),
        );
        e.add_with(
            "EDI",
            1000.0,
            "2026-09-01T10:00:00Z",
            disk_at(8.0, Some("edinburgh")),
        );
        e.add_with("NONE", 1000.0, "2026-09-01T10:00:00Z", disk_at(8.0, None));
        let raw = e.snap();
        let key = |h: &str| machine(&raw, h).key.clone();
        let view = |book: &crate::state::Book| raw.filtered_with(&Filter::default(), book, now());
        let states = |v: &Snapshot| ["GLA", "EDI", "NONE"].map(|h| triage_of(v, h));

        let by_tags = view(&book_of(
            &[],
            &[suppression(
                "disk_nearly_full",
                None,
                Some(&[("site", "glasgow")]),
            )],
        ));
        assert_eq!(states(&by_tags), [Some(Suppressed), None, None]);

        let by_machine = view(&book_of(
            &[],
            &[suppression("disk_nearly_full", Some(&key("EDI")), None)],
        ));
        assert_eq!(states(&by_machine), [None, Some(Suppressed), None]);

        let fleet = view(&book_of(
            &[],
            &[suppression("disk_nearly_full", None, None)],
        ));
        assert_eq!(states(&fleet), [Some(Suppressed); 3]);
        assert_eq!(fleet.summary.quieted.suppressed, 3);

        let other_rule = view(&book_of(&[], &[suppression("stale", None, None)]));
        assert_eq!(
            states(&other_rule),
            [None; 3],
            "a rule is about its own code"
        );

        let both = view(&book_of(
            &[mark_on(
                &key("GLA"),
                "disk_nearly_full",
                Acknowledged,
                Severity::Warning,
                None,
            )],
            &[suppression("disk_nearly_full", Some(&key("GLA")), None)],
        ));
        let t = flag_on(&both, "disk_nearly_full", "GLA")
            .unwrap()
            .triage
            .as_ref()
            .unwrap();
        assert_eq!(t.state, Suppressed, "suppress over acknowledge");
        assert_eq!(t.reach, Some(crate::state::Reach::Machine));
    }

    // --- Thresholds from the config file ------------------------------------

    /// `[thresholds]` as a config file holds it, through the same reader and
    /// the same checks a server applies at load.
    fn tuning(text: &str) -> Tuning {
        let c: crate::config::Config = toml::from_str(text).expect("parses");
        c.validate().expect("valid");
        c.thresholds.tuning().expect("valid thresholds")
    }

    impl Estate {
        fn snap_with(&self, t: &Tuning) -> Snapshot {
            snapshot(self.idx.conn(), t, now()).unwrap()
        }
    }

    /// Tagged with a site, and nothing else changed.
    fn at_site(site: &str) -> impl FnOnce(&mut Value) + use<'_> {
        move |d: &mut Value| {
            d["tags"] = json!({ "site": site });
        }
    }

    fn overridden(s: &Snapshot, host: &str) -> Vec<(String, Vec<(&'static str, f64)>)> {
        machine(s, host)
            .overridden_by
            .iter()
            .map(|o| {
                (
                    o.name.clone(),
                    o.set.iter().map(|(k, v)| (*k, *v)).collect(),
                )
            })
            .collect()
    }

    fn hosts_with(s: &Snapshot, code: &str) -> Vec<String> {
        flags_for(s, code)
            .iter()
            .filter_map(|f| f.hostname.clone())
            .collect()
    }

    #[test]
    fn a_fleet_wide_threshold_changes_the_finding() {
        let mut e = Estate::new();
        e.add("PC-1", 1000.0, "2026-05-01T10:00:00Z");
        let s = e.snap();
        assert_eq!(hosts_with(&s, "stale"), ["PC-1"], "131 days is past 90");
        assert_eq!(s.summary.stale, 1);

        let s = e.snap_with(&tuning("[thresholds]\nstale_days = 180\n"));
        assert!(flags_for(&s, "stale").is_empty(), "{:?}", codes(&s));
        assert_eq!(s.summary.stale, 0);
        assert_eq!(s.thresholds.stale_days, 180.0, "the snapshot carries it");
        assert!(
            machine(&s, "PC-1").overridden_by.is_empty(),
            "a fleet-wide value is not an override"
        );
    }

    #[test]
    fn an_override_by_tags_changes_only_the_machines_it_picks() {
        let mut e = Estate::new();
        e.add_with(
            "KIOSK-1",
            1000.0,
            "2026-09-08T10:00:00Z",
            disk_at(8.0, Some("kiosk")),
        );
        e.add_with(
            "DESK-1",
            1000.0,
            "2026-09-08T10:00:00Z",
            disk_at(8.0, Some("office")),
        );
        let t = tuning(
            "[[thresholds.overrides]]\nname = \"kiosks\"\ntags = { site = \"kiosk\" }\n\
             disk_free_pct = 5\ndisk_free_critical_pct = 2\n",
        );
        let s = e.snap_with(&t);
        assert_eq!(hosts_with(&s, "disk_nearly_full"), ["DESK-1"]);
        assert_eq!(
            overridden(&s, "KIOSK-1"),
            [(
                "kiosks".to_string(),
                vec![("disk_free_critical_pct", 2.0), ("disk_free_pct", 5.0)]
            )]
        );
        assert!(machine(&s, "DESK-1").overridden_by.is_empty());
        assert_eq!(s.threshold_overrides.len(), 1);
        assert_eq!(s.threshold_overrides[0].name, "kiosks");
        assert_eq!(s.thresholds.disk_free_pct, 10.0, "fleet-wide is untouched");

        // The same estate without the rule flags both.
        assert_eq!(
            hosts_with(&e.snap(), "disk_nearly_full"),
            ["DESK-1", "KIOSK-1"]
        );
    }

    /// The finding says which line it was drawn against, and for a machine an
    /// override picks that is the override's.
    #[test]
    fn a_finding_quotes_the_line_the_machine_was_judged_by() {
        let mut e = Estate::new();
        e.add_with(
            "KIOSK-1",
            1000.0,
            "2026-09-08T10:00:00Z",
            disk_at(11.0, Some("kiosk")),
        );
        e.add_with(
            "DESK-1",
            1000.0,
            "2026-09-08T10:00:00Z",
            disk_at(8.0, Some("office")),
        );
        let s = e.snap_with(&tuning(
            "[[thresholds.overrides]]\ntags = { site = \"kiosk\" }\ndisk_free_pct = 12.5\n",
        ));
        let detail = |host: &str| {
            flags_for(&s, "disk_nearly_full")
                .iter()
                .find(|f| f.hostname.as_deref() == Some(host))
                .map(|f| f.detail.clone())
                .unwrap_or_else(|| panic!("{host} should be nearly full"))
        };
        assert!(
            detail("KIOSK-1").contains("Under 12.5% free is nearly full, and under 5% critical"),
            "{}",
            detail("KIOSK-1")
        );
        assert!(
            detail("DESK-1").contains("Under 10% free is nearly full"),
            "{}",
            detail("DESK-1")
        );
        assert_eq!(
            overridden(&s, "KIOSK-1")[0].0,
            "site=kiosk",
            "named after its selector"
        );
    }

    /// "This model" means what the Groups view groups on: the system name as
    /// the firmware gives it, vendor and all.
    #[test]
    fn an_override_by_system_model_changes_only_that_model() {
        let mut e = Estate::new();
        e.add_with(
            "LAB-7440",
            1000.0,
            "2026-05-01T10:00:00Z",
            dell("Latitude 7440"),
        );
        e.add_with(
            "LAB-5440",
            1000.0,
            "2026-05-01T10:00:00Z",
            dell("Latitude 5440"),
        );
        let t = tuning(
            "[[thresholds.overrides]]\nsystem_model = \"Dell Inc. Latitude 7440\"\nstale_days = 365\n",
        );
        let s = e.snap_with(&t);
        assert_eq!(
            GroupBy::Model.value_of(machine(&s, "LAB-7440")),
            Some("Dell Inc. Latitude 7440"),
            "the rule and the group agree about what the model is called"
        );
        assert_eq!(hosts_with(&s, "stale"), ["LAB-5440"]);
        assert_eq!(s.summary.stale, 1);
        assert_eq!(
            overridden(&s, "LAB-7440"),
            [(
                "system_model=Dell Inc. Latitude 7440".to_string(),
                vec![("stale_days", 365.0)]
            )]
        );

        // Not a substring: the 7440's rule does not reach a "Latitude 7440
        // 2-in-1", any more than the model filter does.
        let mut e = Estate::new();
        e.add_with(
            "LAB-2IN1",
            1000.0,
            "2026-05-01T10:00:00Z",
            dell("Latitude 7440 2-in-1"),
        );
        assert_eq!(hosts_with(&e.snap_with(&t), "stale"), ["LAB-2IN1"]);
    }

    #[test]
    fn an_override_by_cpu_model_changes_only_that_cpu() {
        let mut e = Estate::new();
        e.add("NEW-CPU", 1000.0, "2026-05-01T10:00:00Z");
        e.add_with("OLD-CPU", 1000.0, "2026-05-01T10:00:00Z", |d| {
            d["machine"]["cpu_model"] = json!("Intel(R) Core(TM) i5-8350U CPU @ 1.70GHz");
        });
        let s = e.snap_with(&tuning(
            "[[thresholds.overrides]]\ncpu_model = \"Intel(R) Core(TM) i5-8350U CPU @ 1.70GHz\"\n\
             stale_days = 365\n",
        ));
        assert_eq!(hosts_with(&s, "stale"), ["NEW-CPU"]);
        assert!(machine(&s, "NEW-CPU").overridden_by.is_empty());
        assert_eq!(machine(&s, "OLD-CPU").overridden_by.len(), 1);
    }

    /// Per threshold, not per rule: the first rule that picks the machine and
    /// sets the threshold supplies it, and a later rule still supplies one the
    /// first left alone.
    #[test]
    fn the_first_matching_override_wins_threshold_by_threshold() {
        let mut e = Estate::new();
        e.add_with(
            "LAB-1",
            1000.0,
            "2026-05-01T10:00:00Z",
            disk_at(8.0, Some("lab")),
        );
        let t = tuning(
            "[thresholds]\nstale_days = 30\n\
             [[thresholds.overrides]]\nname = \"lab\"\ntags = { site = \"lab\" }\nstale_days = 365\n\
             [[thresholds.overrides]]\nname = \"lab disks\"\ntags = { site = \"lab\" }\n\
             stale_days = 7\ndisk_free_pct = 5\n",
        );
        let s = e.snap_with(&t);
        assert!(
            flags_for(&s, "stale").is_empty(),
            "365 from the first rule, not 7 from the second or 30 from [thresholds]"
        );
        assert!(
            flags_for(&s, "disk_nearly_full").is_empty(),
            "5 from the second rule, which the first did not set"
        );
        assert_eq!(
            overridden(&s, "LAB-1"),
            [
                ("lab".to_string(), vec![("stale_days", 365.0)]),
                ("lab disks".to_string(), vec![("disk_free_pct", 5.0)]),
            ],
            "each rule is listed with what it supplied, and only that"
        );
        assert_eq!(machine(&s, "LAB-1").thresholds.stale_days, 365.0);
        assert_eq!(machine(&s, "LAB-1").thresholds.disk_free_pct, 5.0);
        assert!(
            t.shadowed().is_empty(),
            "the second rule still does something"
        );

        // A rule whose every threshold an earlier, wider rule already sets
        // can never change anything, and is worth a warning.
        let t = tuning(
            "[[thresholds.overrides]]\nname = \"lab\"\ntags = { site = \"lab\" }\nstale_days = 365\n\
             [[thresholds.overrides]]\nname = \"lab pilot\"\ntags = { site = \"lab\", ring = \"pilot\" }\n\
             stale_days = 30\n",
        );
        let warned = t.shadowed();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].contains("\"lab pilot\"") && warned[0].contains("\"lab\""),
            "{warned:?}"
        );
    }

    /// Staleness is counted in the summary, in each group and in the
    /// timeline, and all of them read the machine's own line.
    #[test]
    fn stale_counts_follow_the_override_everywhere_they_are_made() {
        let mut e = Estate::new();
        e.add_with("LAB-1", 1000.0, "2026-05-01T10:00:00Z", at_site("lab"));
        e.add_with("DESK-1", 1000.0, "2026-05-01T10:00:00Z", at_site("office"));
        let t = tuning("[[thresholds.overrides]]\ntags = { site = \"lab\" }\nstale_days = 365\n");
        let s = e.snap_with(&t);
        assert_eq!(s.summary.stale, 1);

        let by_site = groups(&s, &GroupBy::Tag("site".into()));
        let stale_in = |site: &str| {
            by_site
                .iter()
                .find(|g| g.value.as_deref() == Some(site))
                .map(|g| g.stale)
                .expect("group")
        };
        assert_eq!((stale_in("lab"), stale_in("office")), (0, 1));

        let mut tags = BTreeMap::new();
        tags.insert("site".to_string(), "lab".to_string());
        let lab = s.filtered(&Filter {
            tags,
            ..Filter::default()
        });
        assert_eq!(
            lab.summary.stale, 0,
            "a filtered view counts by the same lines"
        );
        assert_eq!(
            lab.threshold_overrides.len(),
            1,
            "and still carries the rules"
        );
    }

    #[test]
    fn the_lifecycle_verdict_follows_an_overridden_replace_age() {
        let mut e = Estate::new();
        // The i7-1370P launched in January 2023: at most 3.7 years old today.
        e.add_with("KIOSK-1", 1000.0, "2026-09-08T10:00:00Z", at_site("kiosk"));
        e.add_with("DESK-1", 1000.0, "2026-09-08T10:00:00Z", at_site("office"));
        let s = e.snap();
        assert_eq!(machine(&s, "KIOSK-1").lifecycle.verdict, Verdict::Keep);

        let s = e.snap_with(&tuning(
            "[[thresholds.overrides]]\nname = \"kiosks\"\ntags = { site = \"kiosk\" }\n\
             replace_age_years = 3\n",
        ));
        let kiosk = &machine(&s, "KIOSK-1").lifecycle;
        assert_eq!(
            kiosk.verdict,
            Verdict::Watch,
            "old at three years: {kiosk:?}"
        );
        assert!(kiosk.reasons[0].contains("years old"), "{kiosk:?}");
        assert_eq!(machine(&s, "DESK-1").lifecycle.verdict, Verdict::Keep);
        assert_eq!(s.summary.lifecycle.watch, 1);
        let report = lifecycle_report(&s);
        assert_eq!(report.replace_age_years, 5.0, "the fleet-wide age");
        assert_eq!(report.candidates[0].overridden_by, ["kiosks"]);
    }

    /// The timeline re-judges the past with today's lines, through the same
    /// resolution as the snapshot, and an override picks a machine by the run
    /// standing for it at the time.
    #[test]
    fn the_timeline_judges_the_past_by_the_same_overrides() {
        let mut e = Estate::new();
        e.add_with("LAB-1", 1000.0, "2026-05-01T10:00:00Z", at_site("lab"));
        e.add_with("DESK-1", 1000.0, "2026-05-01T10:00:00Z", at_site("office"));
        // Was in the lab, and has since moved.
        e.add_with("MOVED-1", 1000.0, "2026-03-01T10:00:00Z", at_site("lab"));
        e.add_with("MOVED-1", 1000.0, "2026-09-01T10:00:00Z", at_site("office"));
        let t = tuning("[[thresholds.overrides]]\ntags = { site = \"lab\" }\nstale_days = 365\n");

        let s = e.snap_with(&t);
        let mut rebuilt = None;
        reconstruct(e.idx.conn(), &t, &[now()], |_, r| {
            rebuilt = Some(r);
            Ok(())
        })
        .unwrap();
        let rebuilt = rebuilt.expect("one cutoff");
        let findings = |s: &Snapshot| -> Vec<(String, &'static str)> {
            s.flags
                .iter()
                .map(|f| (f.machine_key.clone(), f.code))
                .collect()
        };
        assert_eq!(findings(&rebuilt), findings(&s), "now, rebuilt, is now");
        assert_eq!(overridden(&rebuilt, "LAB-1"), overridden(&s, "LAB-1"));
        assert_eq!(rebuilt.summary.stale, s.summary.stale);

        let timeline = Timeline::build(e.idx.conn(), &t, &s, now(), 4).unwrap();
        let points = timeline.view(&s, 4).points;
        assert_eq!(points.len(), 5);
        assert!(
            points.iter().all(|p| p.stale == 1),
            "only DESK-1, in every week: {:?}",
            points.iter().map(|p| p.stale).collect::<Vec<_>>()
        );
        let plain = Timeline::build(e.idx.conn(), &Tuning::default(), &e.snap(), now(), 4)
            .unwrap()
            .view(&s, 4)
            .points;
        assert_eq!(
            plain.iter().map(|p| p.stale).collect::<Vec<_>>(),
            [3, 3, 3, 2, 2],
            "without the rule both lab machines are stale too, MOVED-1 until its new run"
        );

        // A month ago MOVED-1 stood on its lab run, and was judged as a lab
        // machine; today it stands on its office run and is not.
        let then = e.snap_as_of(datetime!(2026-08-01 12:00:00 UTC));
        assert!(then.machines.iter().all(|m| m.overridden_by.is_empty()));
        let mut month_ago = None;
        reconstruct(
            e.idx.conn(),
            &t,
            &[datetime!(2026-08-01 12:00:00 UTC)],
            |_, r| {
                month_ago = Some(r);
                Ok(())
            },
        )
        .unwrap();
        let month_ago = month_ago.expect("one cutoff");
        assert_eq!(overridden(&month_ago, "MOVED-1").len(), 1);
        assert!(machine(&s, "MOVED-1").overridden_by.is_empty());
    }
}
