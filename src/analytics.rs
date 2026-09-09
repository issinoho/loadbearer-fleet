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

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Every number a rule turns on, in one place, so tuning the engine is a config
/// change rather than a code read.
#[derive(Debug, Clone, Serialize)]
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
    /// Stable identifier for the rule — for filtering, and for the suppression
    /// list an estate owner will eventually want.
    pub code: &'static str,
    pub machine_key: String,
    pub hostname: Option<String>,
    pub headline: String,
    pub detail: String,
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
    pub low_confidence_cpu_subtests: i64,
    pub tags: BTreeMap<String, String>,
    pub components: Vec<ComponentView>,
    pub cohort: String,
    /// Percentage difference from the cohort median. Negative is below.
    pub cohort_delta_pct: Option<f64>,
    /// Percentage difference from the median of this machine's own earlier
    /// comparable runs. Negative is a regression.
    pub trend_pct: Option<f64>,
    pub run_id: i64,
    pub source_path: String,
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
    pub critical: usize,
    pub warnings: usize,
    pub info: usize,
    /// Machines carrying at least one critical or warning flag. Always smaller
    /// than the flag count, and the more honest headline of the two.
    pub machines_flagged: usize,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub generated_at: String,
    pub thresholds: Thresholds,
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

fn age_days(taken_at: &str, now: OffsetDateTime) -> Option<f64> {
    let then = OffsetDateTime::parse(taken_at, &Rfc3339).ok()?;
    Some((now - then).as_seconds_f64() / 86_400.0)
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
/// are testable without waiting, and so a report can be regenerated as of a
/// past date.
pub fn snapshot(conn: &Connection, th: &Thresholds, now: OffsetDateTime) -> Result<Snapshot> {
    let history = load_history(conn)?;
    let mut machines = load_latest(conn, &history, now)?;
    let cohorts = build_cohorts(&machines, th);

    let by_id: HashMap<&str, &Cohort> = cohorts.iter().map(|c| (c.id.as_str(), c)).collect();
    for m in &mut machines {
        if let (Some(score), Some(c)) = (m.score, by_id.get(m.cohort.as_str()))
            && c.comparable
        {
            m.cohort_delta_pct = pct_delta(score, c.median);
        }
    }

    let flags = evaluate(&machines, &by_id, th);
    let summary = summarise(&machines, &cohorts, &flags, th);

    Ok(Snapshot {
        generated_at: now.format(&Rfc3339)?,
        thresholds: th.clone(),
        summary,
        machines,
        cohorts,
        flags,
    })
}

/// Every run, slim, for run counts and per-machine trend. The wide columns are
/// only fetched for the latest run of each machine.
fn load_history(conn: &Connection) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, machine_key, taken_at, overall_score, preset, profile, baseline, build_isa
           FROM run ORDER BY machine_key, taken_at, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(RunRow {
            id: r.get(0)?,
            machine_key: r.get(1)?,
            taken_at: r.get(2)?,
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

/// The latest run per machine, with everything the views need.
///
/// "Latest" breaks ties on row id as well as timestamp, so two files written in
/// the same instant resolve the same way every time rather than making the
/// dashboard flicker between them.
fn load_latest(
    conn: &Connection,
    history: &[RunRow],
    now: OffsetDateTime,
) -> Result<Vec<MachineView>> {
    const LATEST: &str = "SELECT id FROM (SELECT id, ROW_NUMBER() OVER \
         (PARTITION BY machine_key ORDER BY taken_at DESC, id DESC) rn FROM run) WHERE rn = 1";

    let mut tags: HashMap<i64, BTreeMap<String, String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT t.run_id, t.key, t.value FROM tag t JOIN ({LATEST}) l ON l.id = t.run_id"
        ))?;
        for row in stmt.query_map([], |r| {
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
             JOIN ({LATEST}) l ON l.id = c.run_id ORDER BY c.id"
        ))?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                ComponentView {
                    id: r.get(1)?,
                    score: r.get(2)?,
                    grade: r.get(3)?,
                    graded: r.get(4)?,
                },
            ))
        })? {
            let (id, c) = row?;
            components.entry(id).or_default().push(c);
        }
    }

    // loadbearer's own confidence verdict, not a cv threshold re-derived here:
    // it already folds in the thermal downgrade, so a run whose clocks fell
    // counts as shaky without this module having to decide that a second time.
    // Restricted to `cpu` — see `Thresholds::unstable_cpu_subtests`.
    let mut low_conf: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT s.run_id, COUNT(*) FROM subtest s JOIN ({LATEST}) l ON l.id = s.run_id \
             WHERE s.confidence = 'low' AND s.component = 'cpu' GROUP BY s.run_id"
        ))?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (id, n) = row?;
            low_conf.insert(id, n);
        }
    }

    // Run count and first sighting per machine, from the slim history.
    let mut counts: HashMap<&str, (usize, &str)> = HashMap::new();
    for r in history {
        counts
            .entry(r.machine_key.as_str())
            .and_modify(|e| e.0 += 1)
            .or_insert((1, r.taken_at.as_str()));
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT id, machine_key, key_kind, hostname, taken_at, tool_version, preset, profile,
                baseline, build_isa, overall_score, overall_grade, cpu_model, cpu_cores,
                ram_bytes, os, arch, serial, asset_tag, on_ac, thermal_limited,
                battery_health, battery_cycles, partial, caveats, source_path
           FROM run WHERE id IN ({LATEST}) ORDER BY machine_key"
    ))?;

    let mut out = Vec::new();
    for row in stmt.query_map([], |r| {
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
        let seen = counts.get(key.as_str()).copied();
        Ok(MachineView {
            cohort: cohort_id(&r.get::<_, String>(12)?, &cmp),
            age_days: age_days(&taken_at, now),
            runs: seen.map_or(1, |s| s.0),
            first_seen: seen.map_or_else(|| taken_at.clone(), |s| s.1.to_string()),
            key,
            key_kind: r.get(2)?,
            hostname: r.get(3)?,
            taken_at,
            tool_version: r.get(5)?,
            comparability: cmp,
            score: r.get(10)?,
            grade: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
            cpu_model: r.get(12)?,
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

    for m in &mut out {
        m.trend_pct = trend(history, m);
    }
    Ok(out)
}

/// A machine against its own past.
///
/// Only comparable earlier runs count — a machine re-measured under a different
/// preset has not regressed, it has been measured differently. The reference is
/// the median of the priors rather than the best of them, because one unusually
/// good run would otherwise make every later run look like a regression.
fn trend(history: &[RunRow], m: &MachineView) -> Option<f64> {
    let score = m.score?;
    let priors = sorted_of(
        history
            .iter()
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

fn evaluate(
    machines: &[MachineView],
    cohorts: &HashMap<&str, &Cohort>,
    th: &Thresholds,
) -> Vec<Flag> {
    let mut flags = Vec::new();
    for m in machines {
        let score = m.score.unwrap_or(0.0);
        let mut push = |severity: Severity,
                        kind: FlagKind,
                        code: &'static str,
                        headline: String,
                        detail: String| {
            flags.push(Flag {
                severity,
                kind,
                code,
                machine_key: m.key.clone(),
                hostname: m.hostname.clone(),
                headline,
                detail,
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
            if shortfall >= th.cohort_min_shortfall && z >= th.cohort_z {
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
            let explained = m.thermal_limited == Some(true) || m.on_ac == Some(false);
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
                     run(s). Comparing a machine with itself holds the hardware constant, which \
                     makes this the strongest signal in the fleet: something has changed.{}",
                    score / (1.0 + trend / 100.0),
                    m.runs.saturating_sub(1),
                    if explained {
                        " This run was thermally limited or taken on battery, which may account \
                         for some of it — re-measure before acting."
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
        for c in m.components.iter().filter(|c| c.graded) {
            let Some(rank) = grade_rank(&c.grade) else {
                continue;
            };
            let overall = grade_rank(&m.grade).unwrap_or(rank);
            if rank >= 4 && rank >= overall + 2 {
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
                match m.battery_cycles {
                    Some(c) => format!(
                        "{c} charge cycles. Nothing to do with performance, but it is the part \
                         of a laptop that fails first and the one its user notices."
                    ),
                    None => "Nothing to do with performance, but it is the part of a laptop that \
                             fails first and the one its user notices."
                        .to_string(),
                },
            );
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
                "The CPU kernels are deterministic, so iteration-to-iteration spread there means \
                 something else was running on the machine — unlike spread on disk writes or on \
                 the network, which is the hardware behaving normally. Re-measure it idle before \
                 drawing anything from this run."
                    .to_string(),
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
                    "Taken {} with loadbearer {}. Old enough that it describes the machine as it \
                     was rather than as it is.",
                    m.taken_at, m.tool_version
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
                    th.min_cohort
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

/// Everything in the summary is derived from the machines in view, never from a
/// separate query, so a filtered view's headline always agrees with the rows
/// underneath it.
fn summarise(
    machines: &[MachineView],
    cohorts: &[Cohort],
    flags: &[Flag],
    th: &Thresholds,
) -> Summary {
    let scores = sorted_of(machines.iter().filter_map(|m| m.score));

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut unrecognised = 0usize;
    for m in machines {
        match grade_rank(&m.grade) {
            Some(_) => *counts.entry(m.grade.as_str()).or_default() += 1,
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

    let mut configs: BTreeMap<String, usize> = BTreeMap::new();
    for m in machines {
        *configs.entry(m.comparability.label()).or_default() += 1;
    }
    let mut configurations: Vec<(String, usize)> = configs.into_iter().collect();
    configurations.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Info flags don't make a machine "flagged" — every machine in a small
    // estate carries one, and a headline that says all of them need attention
    // is a headline nobody reads twice.
    let flagged: HashSet<&str> = flags
        .iter()
        .filter(|f| f.severity != Severity::Info)
        .map(|f| f.machine_key.as_str())
        .collect();

    Summary {
        machines: machines.len(),
        runs: machines.iter().map(|m| m.runs).sum(),
        grades,
        median_score: (!scores.is_empty()).then(|| median(&scores)),
        p10_score: (!scores.is_empty()).then(|| quantile(&scores, 0.10)),
        p90_score: (!scores.is_empty()).then(|| quantile(&scores, 0.90)),
        critical: flags
            .iter()
            .filter(|f| f.severity == Severity::Critical)
            .count(),
        warnings: flags
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count(),
        info: flags
            .iter()
            .filter(|f| f.severity == Severity::Info)
            .count(),
        machines_flagged: flagged.len(),
        stale: machines
            .iter()
            .filter(|m| m.age_days.is_some_and(|a| a > th.stale_days))
            .count(),
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
    /// Case-insensitive substring of hostname, key, serial, asset tag or CPU.
    pub search: Option<String>,
    /// Only machines carrying a flag with this code.
    pub flag: Option<String>,
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
        for (k, v) in &self.tags {
            if m.tags.get(k).map(String::as_str) != Some(v.as_str()) {
                return false;
            }
        }
        if let Some(q) = &self.search {
            let q = q.to_lowercase();
            let hit = [
                Some(m.key.as_str()),
                m.hostname.as_deref(),
                m.serial.as_deref(),
                m.asset_tag.as_deref(),
                Some(m.cpu_model.as_str()),
            ]
            .into_iter()
            .flatten()
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
    /// summary so the headline always agrees with the rows beneath it.
    pub fn filtered(&self, f: &Filter, th: &Thresholds) -> Snapshot {
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
        let keys: HashSet<&str> = machines.iter().map(|m| m.key.as_str()).collect();
        let flags: Vec<Flag> = self
            .flags
            .iter()
            .filter(|fl| keys.contains(fl.machine_key.as_str()))
            .cloned()
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
            generated_at: self.generated_at.clone(),
            thresholds: self.thresholds.clone(),
            summary: summarise(&machines, &cohorts, &flags, th),
            machines,
            cohorts,
            flags,
        }
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
}

/// The per-machine detail the drilldown needs, which the fleet snapshot
/// deliberately doesn't carry: every run of one machine, and every subtest of
/// its latest run. Held back from the snapshot because an estate of ten
/// thousand machines would otherwise ship a hundred megabytes of measurements
/// to draw one summary.
pub fn detail(conn: &Connection, machine_key: &str, latest_run: i64) -> Result<Detail> {
    let mut stmt = conn.prepare(
        "SELECT id, taken_at, overall_score, overall_grade, tool_version, preset, profile,
                baseline, build_isa, thermal_limited, on_ac, partial, source_path
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

    Ok(Detail { history, subtests })
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
    fn now() -> OffsetDateTime {
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
    struct Estate {
        idx: Index,
        base: Value,
    }

    impl Estate {
        fn new() -> Self {
            let text = std::fs::read_to_string(fixture_path("win-modern.json")).expect("fixture");
            Estate {
                idx: Index::open_in_memory().unwrap(),
                base: serde_json::from_str(&text).expect("json"),
            }
        }

        fn add(&mut self, host: &str, score: f64, taken: &str) {
            self.add_with(host, score, taken, |_| {});
        }

        fn add_with(
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

        fn ingest(&mut self, name: &str) {
            assert!(self.idx.ingest_file(&fixture_path(name)).unwrap());
        }

        fn snap(&self) -> Snapshot {
            self.snap_at(now())
        }

        fn snap_at(&self, at: OffsetDateTime) -> Snapshot {
            snapshot(self.idx.conn(), &Thresholds::default(), at).unwrap()
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
        let view = all.filtered(
            &Filter {
                tags,
                ..Default::default()
            },
            &Thresholds::default(),
        );

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
        let view = all.filtered(
            &Filter {
                tags,
                ..Default::default()
            },
            &Thresholds::default(),
        );
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
        let th = Thresholds::default();

        let recent = all.filtered(
            &Filter {
                max_age_days: Some(30.0),
                ..Default::default()
            },
            &th,
        );
        assert_eq!(recent.summary.machines, 1);
        assert_eq!(recent.machines[0].hostname.as_deref(), Some("FRESH-1"));

        let searched = all.filtered(
            &Filter {
                search: Some("old".into()),
                ..Default::default()
            },
            &th,
        );
        assert_eq!(searched.summary.machines, 1, "search is case-insensitive");
        assert_eq!(searched.machines[0].hostname.as_deref(), Some("OLD-1"));

        let stale = all.filtered(
            &Filter {
                flag: Some("stale".into()),
                ..Default::default()
            },
            &th,
        );
        assert_eq!(stale.summary.machines, 1);
        assert_eq!(stale.machines[0].hostname.as_deref(), Some("OLD-1"));
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
}
