//! The fleet by component: how one measure is spread across the machines in
//! view (`distribution`), and every machine's standing on every graded
//! component (`heatmap`).
//!
//! ## What can share an axis
//!
//! A distribution is a claim that its values are the same kind of number, so
//! the whole design is deciding what may be pooled. The rule is the one the
//! cohorts already follow: **a run's numbers are only comparable within its
//! configuration** — preset, profile, baseline and build ISA, `Comparability`
//! — so one distribution draws from one configuration, says which, offers the
//! others with how many machines each, and counts every machine in view it
//! left out with the reason.
//!
//! Within that, what is drawn depends on the measure:
//!
//! * **The overall score, or a component's,** is the score loadbearer gave it
//!   against the run's baseline. The baseline is part of the configuration,
//!   so two scores in one distribution were always expressed on one scale.
//! * **A subtest** is its raw value in its own unit, as `compare` reads it,
//!   rather than its score: not every subtest is scored (an informational one
//!   has no baseline at all), and the raw figure is what was measured. Strictly
//!   only the preset and the build ISA change a raw figure, but pooling it by
//!   the whole configuration keeps one rule for every measure, so the
//!   configuration picker means the same thing whichever is chosen. Two more
//!   things must match before raw values share an axis: the **unit**, and the
//!   **statistic** — a peak beside a median measures the statistic, not the
//!   machine (absent is a median, as `compare` reads it). Where runs in one
//!   configuration disagree, the commonest pairing is drawn and the rest are
//!   counted out.
//!
//! **Direction is said, not assumed.** A score is always higher-is-better. A
//! raw value only has a better end if the run recorded which way it runs;
//! runs indexed before 0.5.3 have none, and then the distribution says so
//! rather than implying one. Pooling does not need a direction — a value is a
//! value — so a run without one is drawn, not dropped.
//!
//! Partial, throttled and on-battery runs are drawn like any other, as the
//! score distribution on the overview draws them: this describes the estate
//! as measured, and the findings already say which measurements to doubt.
//!
//! ## The heatmap's cells
//!
//! Each cell is the machine's standing on that component **against its
//! cohort**: the difference from the median of the same component across the
//! machines with the same CPU measured the same way, worked out across the
//! whole index (`place_cohort_deltas`). It is the per-component form of the
//! comparison the Groups view headlines with, for the same reason — a raw
//! component score across different hardware mostly measures the hardware. A
//! machine whose cohort has fewer than `min_cohort` measurements of a
//! component has no value there, and the cell says so rather than reading as
//! average. The alternative encoding is the component's grade against the
//! baseline.
//!
//! The `component_weak` finding is a different comparison — a component
//! graded D or F and at least two grades below the machine's own — and a cell
//! must never look fine on a machine carrying it. So both read one rule,
//! `is_weak`, and every weak cell is marked as such whichever encoding is
//! drawn. That rule reads no threshold, so no override changes it.
//!
//! ## Who sees what
//!
//! Both take an already-projected snapshot, like `groups`: the caller's filter
//! and scope have been applied before anything here runs, so every count, bin
//! edge and "left out" number is made of the caller's machines only. The one
//! thing not narrowed is the cohort median behind a cell, which is worked out
//! across the whole index exactly as every other cohort figure is.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use super::{
    Comparability, ComponentView, GroupBy, MachineView, Snapshot, dimensions, grade_rank, median,
    pct_delta, quantile, sorted_of,
};

/// Whether one component is weak on its machine: graded D or F, and at least
/// two grades below the machine overall. Ungraded components (network, gpu)
/// never are — loadbearer keeps them out of the grade because they measure the
/// host and its drivers. The one definition, read by `evaluate` to raise
/// `component_weak` and by the heatmap to mark a cell.
pub(super) fn is_weak(machine_grade: &str, c: &ComponentView) -> bool {
    if !c.graded {
        return false;
    }
    let Some(rank) = grade_rank(&c.grade) else {
        return false;
    };
    let overall = grade_rank(machine_grade).unwrap_or(rank);
    rank >= 4 && rank >= overall + 2
}

/// Each component's difference from the median of the same component across
/// its machine's cohort. Called by `assess` over every machine, before any
/// filter, so like the cohort's own median it does not move with the view.
pub(super) fn place_cohort_deltas(machines: &mut [MachineView], min_cohort: usize) {
    let mut scores: HashMap<(&str, &str), Vec<f64>> = HashMap::new();
    for m in machines.iter() {
        for c in &m.components {
            scores
                .entry((m.cohort.as_str(), c.id.as_str()))
                .or_default()
                .push(c.score);
        }
    }
    let medians: HashMap<(&str, &str), f64> = scores
        .into_iter()
        .filter_map(|(k, v)| {
            let sorted = sorted_of(v);
            (sorted.len() >= min_cohort).then(|| (k, median(&sorted)))
        })
        .collect();
    let deltas: Vec<Vec<Option<f64>>> = machines
        .iter()
        .map(|m| {
            m.components
                .iter()
                .map(|c| {
                    medians
                        .get(&(m.cohort.as_str(), c.id.as_str()))
                        .and_then(|med| pct_delta(c.score, *med))
                })
                .collect()
        })
        .collect();
    for (m, ds) in machines.iter_mut().zip(deltas) {
        for (c, d) in m.components.iter_mut().zip(ds) {
            c.cohort_delta_pct = d;
        }
    }
}

/// The order components are listed in: the three every run grades, then
/// whatever else, alphabetically.
fn component_order(id: &str) -> (usize, &str) {
    let known = ["cpu", "memory", "disk", "network", "gpu"];
    (
        known.iter().position(|k| *k == id).unwrap_or(known.len()),
        id,
    )
}

/* ------------------------------------------------------------ measurements */

/// One subtest of a machine's latest run, as the distribution needs it.
#[derive(Debug, Clone)]
struct Reading {
    component: Arc<str>,
    id: Arc<str>,
    label: Option<Arc<str>>,
    unit: Arc<str>,
    direction: Option<Arc<str>>,
    /// `median` or `peak`; absent in the document reads as `median`.
    statistic: Arc<str>,
    value: f64,
    scored: bool,
}

/// Every subtest of every machine's latest run, held beside the snapshot.
///
/// The snapshot carries components but not subtests, deliberately — see
/// `detail`. A distribution of one subtest needs that subtest for every machine
/// in view, and the measure picker needs to know which machines have each, so
/// rather than query the index per request (taking its lock, and paying for a
/// join over the whole subtest table on every click) this is read once when
/// the snapshot is rebuilt and replaced with it. Strings that repeat across
/// machines are shared. At 2,000 machines with thirty subtests each it is
/// 60,000 small rows, a few megabytes.
#[derive(Debug, Default)]
pub struct Measures {
    by_run: HashMap<i64, Vec<Reading>>,
    component_labels: HashMap<Arc<str>, Arc<str>>,
}

impl Measures {
    /// The subtests of the runs `snap` stands on — its machines' latest.
    pub fn load(conn: &Connection, snap: &Snapshot) -> Result<Self> {
        let ids = format!(
            "[{}]",
            snap.machines
                .iter()
                .map(|m| m.run_id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut strings: HashSet<Arc<str>> = HashSet::new();
        let mut share = |s: &str| -> Arc<str> {
            if let Some(shared) = strings.get(s) {
                return Arc::clone(shared);
            }
            let shared: Arc<str> = Arc::from(s);
            strings.insert(Arc::clone(&shared));
            shared
        };
        let mut out = Measures::default();
        // One JSON array rather than a placeholder per run, for the reason
        // `Pick::sql` gives: a fleet of thousands passes SQLite's limit.
        let mut stmt = conn.prepare(
            "SELECT s.run_id, s.component, s.id, s.label, s.unit, s.direction, s.representative,
                    s.value, s.scored, c.label
               FROM subtest s
               JOIN json_each(?1) j ON j.value = s.run_id
               LEFT JOIN component c ON c.run_id = s.run_id AND c.id = s.component
              ORDER BY s.run_id, s.component, s.id",
        )?;
        let mut rows = stmt.query([ids])?;
        while let Some(r) = rows.next()? {
            let run: i64 = r.get(0)?;
            let component = share(&r.get::<_, String>(1)?);
            if let Some(label) = r.get::<_, Option<String>>(9)?.filter(|l| !l.is_empty()) {
                out.component_labels
                    .entry(Arc::clone(&component))
                    .or_insert_with(|| share(&label));
            }
            let reading = Reading {
                id: share(&r.get::<_, String>(2)?),
                label: r
                    .get::<_, Option<String>>(3)?
                    .filter(|l| !l.is_empty())
                    .map(|l| share(&l)),
                unit: share(&r.get::<_, String>(4)?),
                direction: r.get::<_, Option<String>>(5)?.map(|d| share(&d)),
                statistic: share(
                    r.get::<_, Option<String>>(6)?
                        .as_deref()
                        .unwrap_or("median"),
                ),
                value: r.get(7)?,
                scored: r.get(8)?,
                component,
            };
            out.by_run.entry(run).or_default().push(reading);
        }
        Ok(out)
    }

    fn reading(&self, run: i64, component: &str, id: &str) -> Option<&Reading> {
        self.by_run
            .get(&run)?
            .iter()
            .find(|r| &*r.component == component && &*r.id == id)
    }

    fn component_label(&self, id: &str) -> String {
        self.component_labels
            .get(id)
            .map(|l| l.to_string())
            .unwrap_or_else(|| id.to_string())
    }
}

/* ------------------------------------------------------------ distribution */

/// What a distribution is of. Spelled `overall`, a component id (`disk`), or
/// a component and subtest (`disk/seq_read`) — the same in a URL, on the
/// command line and in the JSON that echoes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Measure {
    Overall,
    Component(String),
    Subtest(String, String),
}

impl std::str::FromStr for Measure {
    type Err = String;

    /// Only the shape is judged here; whether any machine in view has the
    /// measure is `distribution`'s question, since only it knows the view.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let bad = || {
            format!(
                "{s:?} is not a measure. Use overall, a component (disk), or a component and \
                 subtest (disk/seq_read)"
            )
        };
        if s.is_empty() {
            return Err(bad());
        }
        if s == "overall" {
            return Ok(Self::Overall);
        }
        match s.split_once('/') {
            None => Ok(Self::Component(s.to_string())),
            Some((c, t)) if !c.trim().is_empty() && !t.trim().is_empty() && !t.contains('/') => {
                Ok(Self::Subtest(c.trim().to_string(), t.trim().to_string()))
            }
            Some(_) => Err(bad()),
        }
    }
}

impl std::fmt::Display for Measure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overall => f.write_str("overall"),
            Self::Component(c) => f.write_str(c),
            Self::Subtest(c, t) => write!(f, "{c}/{t}"),
        }
    }
}

/// A configuration's key in a URL: `preset|profile|baseline|build_isa`, the
/// tail of a cohort id.
fn config_key(c: &Comparability) -> String {
    format!("{}|{}|{}|{}", c.preset, c.profile, c.baseline, c.build_isa)
}

/// One configuration the measure could be drawn from.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigOption {
    pub key: String,
    pub label: String,
    /// Machines in view measured under it that have the measure.
    pub machines: usize,
}

/// Machines in view left out of the distribution, for one reason.
#[derive(Debug, Clone, Serialize)]
pub struct LeftOut {
    /// `not_measured`, `other_configuration`, `other_unit` or
    /// `other_statistic`.
    pub reason: &'static str,
    pub machines: usize,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bin {
    /// From `lo`, up to but not including `hi` — except the last bin, which
    /// includes it.
    pub lo: f64,
    pub hi: f64,
    pub machines: usize,
}

/// One machine drawn, so a bin can list its machines.
#[derive(Debug, Clone, Serialize)]
pub struct Point {
    pub key: String,
    pub hostname: Option<String>,
    pub value: f64,
    /// Which of `bins` it falls in.
    pub bin: usize,
}

/// A subtest in the measure picker.
#[derive(Debug, Clone, Serialize)]
pub struct MeasureOption {
    pub measure: String,
    pub label: String,
    pub unit: String,
    /// Scored against the baseline. An informational subtest is measured and
    /// shown and never folded into a score.
    pub scored: bool,
    /// Machines in view whose latest run has it, in any configuration.
    pub machines: usize,
}

/// A component in the measure picker, with its subtests.
#[derive(Debug, Clone, Serialize)]
pub struct MeasureGroup {
    pub measure: String,
    pub label: String,
    pub graded: bool,
    pub machines: usize,
    pub subtests: Vec<MeasureOption>,
}

/// The distribution of one group of the machines drawn.
#[derive(Debug, Clone, Serialize)]
pub struct SplitGroup {
    /// `None` for the machines with no value for the dimension.
    pub value: Option<String>,
    pub machines: usize,
    pub median: f64,
    pub p10: f64,
    pub p90: f64,
    /// Fewer than `min_cohort` machines: quantiles of so few describe those
    /// machines rather than the group, as a thin group does on Groups.
    pub thin: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Split {
    pub by: String,
    pub groups: Vec<SplitGroup>,
}

/// The whole answer to "how is this spread across the machines in view", as
/// `/api/distribution` and `report --distribution --json` send it.
#[derive(Debug, Clone, Serialize)]
pub struct Distribution {
    pub measure: String,
    /// `overall`, `component` or `subtest`.
    pub kind: &'static str,
    pub label: String,
    /// `score` for the overall and a component — a score against the
    /// baseline — or the subtest's own unit.
    pub unit: String,
    /// `higher_is_better` or `lower_is_better` where every machine drawn
    /// agrees, else `None`, and `direction_note` says why.
    pub direction: Option<String>,
    pub direction_note: Option<String>,
    /// A subtest's statistic, `median` or `peak`.
    pub statistic: Option<String>,
    /// False for an ungraded component or an informational subtest: measured,
    /// shown, and never part of a grade.
    pub counted: bool,
    /// The configuration drawn, by key and in words. `None` only when nothing
    /// in view has the measure.
    pub config: Option<String>,
    pub config_label: Option<String>,
    /// Every configuration in view with the measure, most machines first.
    pub configurations: Vec<ConfigOption>,
    /// Machines in view. `included` and every `left_out` add up to it.
    pub machines: usize,
    pub included: usize,
    pub left_out: Vec<LeftOut>,
    pub median: Option<f64>,
    pub p10: Option<f64>,
    pub p90: Option<f64>,
    /// Their `machines` add up to `included`.
    pub bins: Vec<Bin>,
    /// Every machine drawn, lowest value first.
    pub values: Vec<Point>,
    /// Machines in view with an overall score, for the picker.
    pub overall_machines: usize,
    pub measures: Vec<MeasureGroup>,
    /// Everything the machines drawn could be split by.
    pub dimensions: Vec<String>,
    pub split: Option<Split>,
    pub min_cohort: usize,
}

/// One machine's value for a measure, with what decides whether it may share
/// an axis with another's.
struct Value<'a> {
    value: f64,
    unit: &'a str,
    statistic: Option<&'a str>,
    direction: Option<&'a str>,
}

fn value_of<'a>(
    m: &'a MachineView,
    measure: &Measure,
    measures: &'a Measures,
) -> Option<Value<'a>> {
    let v = match measure {
        Measure::Overall => Value {
            value: m.score?,
            unit: "score",
            statistic: None,
            direction: Some("higher_is_better"),
        },
        Measure::Component(id) => Value {
            value: m.components.iter().find(|c| &c.id == id)?.score,
            unit: "score",
            statistic: None,
            direction: Some("higher_is_better"),
        },
        Measure::Subtest(c, t) => {
            let r = measures.reading(m.run_id, c, t)?;
            Value {
                value: r.value,
                unit: &r.unit,
                statistic: Some(&r.statistic),
                direction: r.direction.as_deref(),
            }
        }
    };
    v.value.is_finite().then_some(v)
}

/// The components and subtests the machines in view have, each with how many.
fn picker(snap: &Snapshot, measures: &Measures) -> Vec<MeasureGroup> {
    struct Seen {
        graded: bool,
        machines: usize,
        subtests: BTreeMap<String, MeasureOption>,
    }
    let mut seen: BTreeMap<(usize, String), Seen> = BTreeMap::new();
    for m in &snap.machines {
        for c in &m.components {
            let (rank, id) = component_order(&c.id);
            let e = seen.entry((rank, id.to_string())).or_insert(Seen {
                graded: c.graded,
                machines: 0,
                subtests: BTreeMap::new(),
            });
            e.machines += 1;
            e.graded |= c.graded;
        }
        let mut counted: HashSet<(&str, &str)> = HashSet::new();
        for r in measures.by_run.get(&m.run_id).into_iter().flatten() {
            if !counted.insert((&*r.component, &*r.id)) {
                continue;
            }
            let (rank, id) = component_order(&r.component);
            let e = seen.entry((rank, id.to_string())).or_insert(Seen {
                graded: false,
                machines: 0,
                subtests: BTreeMap::new(),
            });
            let o = e
                .subtests
                .entry(r.id.to_string())
                .or_insert_with(|| MeasureOption {
                    measure: format!("{}/{}", r.component, r.id),
                    label: r.label.as_deref().unwrap_or(&r.id).to_string(),
                    unit: r.unit.to_string(),
                    scored: r.scored,
                    machines: 0,
                });
            o.machines += 1;
        }
    }
    seen.into_iter()
        .map(|((_, id), s)| MeasureGroup {
            label: measures.component_label(&id),
            measure: id,
            graded: s.graded,
            machines: s.machines,
            subtests: s.subtests.into_values().collect(),
        })
        .collect()
}

/// Round bin edges: a step of 1, 2, 2.5 or 5 times a power of ten, the one
/// whose bins over `lo..hi` come nearest `target` without passing twelve.
/// Rounding the step up instead made too few: nine machines spread over
/// 1,600 points became two bins a thousand wide, which is no shape at all.
fn nice_step(lo: f64, hi: f64, target: f64) -> f64 {
    let mag = 10f64.powf(((hi - lo) / target).log10().floor());
    let count = |step: f64| ((hi - (lo / step).floor() * step) / step).floor() + 1.0;
    [0.1, 1.0, 10.0]
        .iter()
        .flat_map(|m| [1.0, 2.0, 2.5, 5.0].map(|n| n * m * mag))
        .filter(|step| count(*step) <= 12.0)
        .min_by(|a, b| {
            (count(*a) - target)
                .abs()
                .total_cmp(&(count(*b) - target).abs())
        })
        .unwrap_or(10.0 * mag)
}

/// Histogram bins over ascending values, and which bin each value is in.
/// About the square root of the count, between four and twelve: enough to
/// show a shape, few enough to read on a phone.
fn binned(sorted: &[f64]) -> (Vec<Bin>, Vec<usize>) {
    let (Some(&lo), Some(&hi)) = (sorted.first(), sorted.last()) else {
        return (Vec::new(), Vec::new());
    };
    if hi - lo <= f64::EPSILON * lo.abs().max(1.0) {
        // Everyone the same: one bin, rather than a range of zero divided up.
        return (
            vec![Bin {
                lo,
                hi,
                machines: sorted.len(),
            }],
            vec![0; sorted.len()],
        );
    }
    let target = (sorted.len() as f64).sqrt().ceil().clamp(4.0, 12.0);
    let step = nice_step(lo, hi, target);
    let start = (lo / step).floor() * step;
    let count = (((hi - start) / step).floor() as usize + 1).max(1);
    let mut bins: Vec<Bin> = (0..count)
        .map(|i| Bin {
            lo: start + step * i as f64,
            hi: start + step * (i + 1) as f64,
            machines: 0,
        })
        .collect();
    let at: Vec<usize> = sorted
        .iter()
        .map(|v| (((v - start) / step).floor().max(0.0) as usize).min(count - 1))
        .collect();
    for i in &at {
        bins[*i].machines += 1;
    }
    (bins, at)
}

fn name(m: &MachineView) -> &str {
    m.hostname.as_deref().unwrap_or(&m.key)
}

/// How one measure is spread across the machines in a projected snapshot.
///
/// `config` is a key from `configurations`, or `None` for the one with the
/// most machines. An unknown measure or configuration is an `Err` naming what
/// there is, judged against the view alone, so the refusal says nothing about
/// machines the caller cannot see.
pub fn distribution(
    snap: &Snapshot,
    measures: &Measures,
    measure: &Measure,
    config: Option<&str>,
    split: Option<&GroupBy>,
) -> Result<Distribution, String> {
    let picker = picker(snap, measures);
    let overall_machines = snap.machines.iter().filter(|m| m.score.is_some()).count();

    let (kind, label, counted) = match measure {
        Measure::Overall => ("overall", "Overall score".to_string(), true),
        Measure::Component(id) => {
            let Some(g) = picker.iter().find(|g| &g.measure == id) else {
                return Err(format!(
                    "no machine in view has a component called {id:?}. Those in view: {}",
                    names(picker.iter().map(|g| g.measure.as_str()))
                ));
            };
            ("component", g.label.clone(), g.graded)
        }
        Measure::Subtest(c, t) => {
            let found = picker
                .iter()
                .find(|g| &g.measure == c)
                .and_then(|g| g.subtests.iter().find(|s| &s.id_of() == t).map(|s| (g, s)));
            let Some((g, s)) = found else {
                return Err(format!(
                    "no machine in view has a subtest {c}/{t}. Pick one of the measures the \
                     machines in view have{}",
                    match picker.iter().find(|g| &g.measure == c) {
                        Some(g) => format!(
                            " — {c} has {}",
                            names(g.subtests.iter().map(|s| s.measure.as_str()))
                        ),
                        None => String::new(),
                    }
                ));
            };
            (
                "subtest",
                format!("{} · {}", g.label, s.label),
                g.graded && s.scored,
            )
        }
    };

    let with_value: Vec<(&MachineView, Value<'_>)> = snap
        .machines
        .iter()
        .filter_map(|m| value_of(m, measure, measures).map(|v| (m, v)))
        .collect();

    let mut configs: BTreeMap<String, ConfigOption> = BTreeMap::new();
    for (m, _) in &with_value {
        configs
            .entry(config_key(&m.comparability))
            .or_insert_with(|| ConfigOption {
                key: config_key(&m.comparability),
                label: m.comparability.label(),
                machines: 0,
            })
            .machines += 1;
    }
    let mut configurations: Vec<ConfigOption> = configs.into_values().collect();
    configurations.sort_by(|a, b| {
        b.machines
            .cmp(&a.machines)
            .then_with(|| a.label.cmp(&b.label))
    });

    let chosen = match config.map(str::trim).filter(|c| !c.is_empty()) {
        Some(key) => match configurations.iter().find(|c| c.key == key) {
            Some(c) => Some(c.clone()),
            None => {
                return Err(format!(
                    "{key:?} is not a configuration any machine in view measured {} under. \
                     Those in view: {}",
                    measure,
                    names(configurations.iter().map(|c| c.key.as_str()))
                ));
            }
        },
        None => configurations.first().cloned(),
    };

    let not_measured = snap.machines.len() - with_value.len();
    let mut left_out = Vec::new();
    if not_measured > 0 {
        left_out.push(LeftOut {
            reason: "not_measured",
            machines: not_measured,
            detail: format!(
                "{not_measured} machine(s) in view have no {} in their latest run.",
                match measure {
                    Measure::Overall => "overall score".to_string(),
                    _ => label.clone(),
                }
            ),
        });
    }
    let in_config: Vec<&(&MachineView, Value<'_>)> = with_value
        .iter()
        .filter(|(m, _)| {
            chosen
                .as_ref()
                .is_some_and(|c| c.key == config_key(&m.comparability))
        })
        .collect();
    let other_config = with_value.len() - in_config.len();
    if other_config > 0 {
        left_out.push(LeftOut {
            reason: "other_configuration",
            machines: other_config,
            detail: format!(
                "{other_config} machine(s) were measured under another configuration. A \
                 different preset, profile, baseline or instruction set changes the number \
                 without anything changing about the machine, so they are drawn on their own — \
                 choose their configuration to see them."
            ),
        });
    }

    // Within the configuration, the commonest unit and statistic: the rest
    // would be a different number on the same axis.
    let mut pairings: BTreeMap<(&str, Option<&str>), usize> = BTreeMap::new();
    for (_, v) in &in_config {
        *pairings.entry((v.unit, v.statistic)).or_default() += 1;
    }
    let pairing = pairings
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, _)| *k);
    let mut drawn: Vec<(&MachineView, &Value<'_>)> = Vec::new();
    let (mut other_unit, mut other_statistic) = (0usize, 0usize);
    for (m, v) in &in_config {
        match pairing {
            Some((unit, _)) if v.unit != unit => other_unit += 1,
            Some((_, statistic)) if v.statistic != statistic => other_statistic += 1,
            _ => drawn.push((*m, v)),
        }
    }
    if other_unit > 0 {
        left_out.push(LeftOut {
            reason: "other_unit",
            machines: other_unit,
            detail: format!(
                "{other_unit} machine(s) recorded this in another unit, so their figures are not \
                 the same number."
            ),
        });
    }
    if other_statistic > 0 {
        left_out.push(LeftOut {
            reason: "other_statistic",
            machines: other_statistic,
            detail: format!(
                "{other_statistic} machine(s) recorded a different statistic — a peak where these \
                 are medians, or the reverse — which would measure the statistic rather than the \
                 machine."
            ),
        });
    }

    let (unit, statistic) = match pairing {
        Some((u, s)) => (u.to_string(), s.map(str::to_string)),
        None => (
            match measure {
                Measure::Subtest(c, t) => picker
                    .iter()
                    .find(|g| &g.measure == c)
                    .and_then(|g| g.subtests.iter().find(|s| &s.id_of() == t))
                    .map(|s| s.unit.clone())
                    .unwrap_or_default(),
                _ => "score".to_string(),
            },
            None,
        ),
    };

    let directions: HashSet<Option<&str>> = drawn.iter().map(|(_, v)| v.direction).collect();
    let (direction, direction_note) = match (directions.len(), directions.iter().next()) {
        (0, _) => (None, None),
        (1, Some(Some(d))) => (Some((*d).to_string()), None),
        _ if directions.contains(&None) => (
            None,
            Some(
                "No direction is recorded for this measure on some or all of these runs, so this \
                 view does not say which end is better. Runs indexed before 0.5.3 have none; \
                 `scan --reindex` fills it in from the documents."
                    .to_string(),
            ),
        ),
        _ => (
            None,
            Some(
                "These runs disagree about which way this measure runs, so this view does not say \
                 which end is better."
                    .to_string(),
            ),
        ),
    };

    let mut order: Vec<(&MachineView, f64)> = drawn.iter().map(|(m, v)| (*m, v.value)).collect();
    order.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| name(a.0).cmp(name(b.0))));
    let sorted: Vec<f64> = order.iter().map(|(_, v)| *v).collect();
    let (bins, at) = binned(&sorted);
    let values: Vec<Point> = order
        .iter()
        .zip(at)
        .map(|((m, v), bin)| Point {
            key: m.key.clone(),
            hostname: m.hostname.clone(),
            value: *v,
            bin,
        })
        .collect();
    let some = !sorted.is_empty();

    let split = split.map(|by| {
        let mut groups: BTreeMap<Option<&str>, Vec<f64>> = BTreeMap::new();
        for (m, v) in &order {
            groups.entry(by.value_of(m)).or_default().push(*v);
        }
        let mut groups: Vec<SplitGroup> = groups
            .into_iter()
            .map(|(value, vs)| SplitGroup {
                value: value.map(str::to_string),
                machines: vs.len(),
                median: median(&vs),
                p10: quantile(&vs, 0.10),
                p90: quantile(&vs, 0.90),
                thin: vs.len() < snap.thresholds.min_cohort,
            })
            .collect();
        // Machines with no value last, as on Groups: "not recorded" is not a
        // place and should not head the list.
        groups.sort_by(|a, b| {
            a.value
                .is_none()
                .cmp(&b.value.is_none())
                .then_with(|| a.value.cmp(&b.value))
        });
        Split {
            by: by.to_string(),
            groups,
        }
    });

    Ok(Distribution {
        measure: measure.to_string(),
        kind,
        label,
        unit,
        direction,
        direction_note,
        statistic,
        counted,
        config: chosen.as_ref().map(|c| c.key.clone()),
        config_label: chosen.as_ref().map(|c| c.label.clone()),
        configurations,
        machines: snap.machines.len(),
        included: sorted.len(),
        left_out,
        median: some.then(|| median(&sorted)),
        p10: some.then(|| quantile(&sorted, 0.10)),
        p90: some.then(|| quantile(&sorted, 0.90)),
        bins,
        values,
        overall_machines,
        measures: picker,
        dimensions: dimensions(snap),
        split,
        min_cohort: snap.thresholds.min_cohort,
    })
}

impl MeasureOption {
    /// The subtest's own id, the part after the component.
    fn id_of(&self) -> String {
        self.measure
            .split_once('/')
            .map(|(_, t)| t.to_string())
            .unwrap_or_default()
    }
}

/// "a, b and c", capped, for a refusal that lists what there is.
fn names<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let all: Vec<&str> = items.collect();
    if all.is_empty() {
        return "none".to_string();
    }
    let shown = all.iter().take(12).copied().collect::<Vec<_>>().join(", ");
    if all.len() > 12 {
        format!("{shown} and {} more", all.len() - 12)
    } else {
        shown
    }
}

/* ----------------------------------------------------------------- heatmap */

/// How many rows the heatmap sends.
///
/// A reading limit, like `compare::MAX_RUNS`, rather than a technical one: a
/// grid of a thousand rows is not read, it is scrolled past, and the table code
/// draws every row it is given. Sorted first and capped after, so "the worst
/// two hundred on disk" is exactly that, and the response says how many there
/// were so the page can say to filter.
pub const HEATMAP_ROWS: usize = 200;

/// What a heatmap cell shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// Against the machine's cohort — the default; see the module notes.
    #[default]
    Cohort,
    /// The component's grade against the reference baseline.
    Grade,
}

impl std::str::FromStr for Encoding {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "cohort" => Ok(Self::Cohort),
            "grade" => Ok(Self::Grade),
            other => Err(format!(
                "{other:?} is not a heatmap encoding. Use cohort (against identical machines) or \
                 grade (against the reference baseline)"
            )),
        }
    }
}

impl Encoding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cohort => "cohort",
            Self::Grade => "grade",
        }
    }
}

/// What the heatmap's rows are ordered by.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HeatSort {
    Host,
    /// The machine as a whole, in the encoding drawn. The default, weakest
    /// first, so a capped grid shows the machines most worth looking at.
    #[default]
    Overall,
    Component(String),
}

impl std::str::FromStr for HeatSort {
    type Err = String;

    /// Whether a component sort names a column is `heatmap`'s question.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "" => Err("sort needs a value: host, overall or a component".to_string()),
            "host" => Ok(Self::Host),
            "overall" => Ok(Self::Overall),
            other => Ok(Self::Component(other.to_string())),
        }
    }
}

impl std::fmt::Display for HeatSort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host => f.write_str("host"),
            Self::Overall => f.write_str("overall"),
            Self::Component(c) => f.write_str(c),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HeatCell {
    pub component: String,
    /// False when the latest run has no such component — a run from before
    /// it existed, or one that skipped it.
    pub measured: bool,
    pub score: Option<f64>,
    pub grade: Option<String>,
    /// `None` when the cohort has too few measurements of this component to
    /// compare against: no peer group, not average.
    pub cohort_delta_pct: Option<f64>,
    /// Carries the `component_weak` finding, by the same rule.
    pub weak: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeatRow {
    pub key: String,
    pub hostname: Option<String>,
    pub grade: String,
    pub score: Option<f64>,
    pub cohort_delta_pct: Option<f64>,
    pub cells: Vec<HeatCell>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Heatmap {
    pub encoding: &'static str,
    pub sort: String,
    /// `asc` or `desc`. Machines with no value for the sort go last either way.
    pub dir: &'static str,
    /// The graded components any machine in view has, one column each.
    pub components: Vec<String>,
    /// Machines in view; `rows` holds at most `cap` of them.
    pub machines: usize,
    pub shown: usize,
    pub cap: usize,
    /// Weak cells across every machine in view, not just those shown.
    pub weak: usize,
    /// Machines in view with no component compared against a cohort.
    pub without_peers: usize,
    pub min_cohort: usize,
    pub rows: Vec<HeatRow>,
}

/// Every machine in a projected snapshot against every graded component,
/// sorted, then capped at `cap`.
pub fn heatmap(
    snap: &Snapshot,
    encoding: Encoding,
    sort: &HeatSort,
    descending: bool,
    cap: usize,
) -> Result<Heatmap, String> {
    let mut components: Vec<&str> = snap
        .machines
        .iter()
        .flat_map(|m| {
            m.components
                .iter()
                .filter(|c| c.graded)
                .map(|c| c.id.as_str())
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    components.sort_by(|a, b| component_order(a).cmp(&component_order(b)));
    if let HeatSort::Component(c) = sort
        && !components.contains(&c.as_str())
    {
        return Err(format!(
            "{c:?} is not a column here. Sort by host, overall, or one of the graded components \
             in view: {}",
            names(components.iter().copied())
        ));
    }

    let mut rows: Vec<HeatRow> = snap
        .machines
        .iter()
        .map(|m| HeatRow {
            key: m.key.clone(),
            hostname: m.hostname.clone(),
            grade: m.grade.clone(),
            score: m.score,
            cohort_delta_pct: m.cohort_delta_pct,
            cells: components
                .iter()
                .map(|id| match m.components.iter().find(|c| c.id == *id) {
                    Some(c) => HeatCell {
                        component: c.id.clone(),
                        measured: true,
                        score: Some(c.score),
                        grade: Some(c.grade.clone()),
                        cohort_delta_pct: c.cohort_delta_pct,
                        weak: is_weak(&m.grade, c),
                    },
                    None => HeatCell {
                        component: (*id).to_string(),
                        measured: false,
                        score: None,
                        grade: None,
                        cohort_delta_pct: None,
                        weak: false,
                    },
                })
                .collect(),
        })
        .collect();
    let weak = rows
        .iter()
        .flat_map(|r| &r.cells)
        .filter(|c| c.weak)
        .count();
    let without_peers = rows
        .iter()
        .filter(|r| r.cells.iter().all(|c| c.cohort_delta_pct.is_none()))
        .count();

    let key_of = |r: &HeatRow| -> Option<f64> {
        let cell = |id: &str| r.cells.iter().find(|c| c.component == id);
        match (sort, encoding) {
            (HeatSort::Host, _) => None,
            (HeatSort::Overall, Encoding::Cohort) => r.cohort_delta_pct,
            (HeatSort::Overall, Encoding::Grade) => r.score,
            (HeatSort::Component(c), Encoding::Cohort) => cell(c.as_str())?.cohort_delta_pct,
            (HeatSort::Component(c), Encoding::Grade) => cell(c.as_str())?.score,
        }
    };
    let host = |r: &HeatRow| r.hostname.as_deref().unwrap_or(&r.key).to_lowercase();
    rows.sort_by(|a, b| {
        let by_name = host(a).cmp(&host(b)).then_with(|| a.key.cmp(&b.key));
        if *sort == HeatSort::Host {
            return if descending {
                by_name.reverse()
            } else {
                by_name
            };
        }
        match (key_of(a), key_of(b)) {
            (Some(x), Some(y)) => {
                let o = x.total_cmp(&y);
                (if descending { o.reverse() } else { o }).then(by_name)
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => by_name,
        }
    });
    let machines = rows.len();
    rows.truncate(cap);

    Ok(Heatmap {
        encoding: encoding.as_str(),
        sort: sort.to_string(),
        dir: if descending { "desc" } else { "asc" },
        components: components.into_iter().map(str::to_string).collect(),
        machines,
        shown: rows.len(),
        cap,
        weak,
        without_peers,
        min_cohort: snap.thresholds.min_cohort,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::Filter;
    use super::super::tests::Estate;
    use super::*;

    /// Set one component's score and grade in a document.
    fn component(d: &mut serde_json::Value, id: &str, score: f64, grade: &str) {
        for c in d["components"].as_array_mut().expect("array") {
            if c["id"] == json!(id) {
                c["score"] = json!(score);
                c["grade"] = json!(grade);
            }
        }
    }

    /// Change one raw subtest, in both places a result records it: the scored
    /// copy is what the index keeps for value and unit, `raw` for the rest.
    fn subtest(
        d: &mut serde_json::Value,
        comp: &str,
        id: &str,
        f: impl Fn(&mut serde_json::Value),
    ) {
        for section in ["components", "raw"] {
            for c in d[section].as_array_mut().expect("array") {
                if c["id"] == json!(comp) {
                    for s in c["subtests"].as_array_mut().expect("array") {
                        if s["id"] == json!(id) {
                            f(s);
                        }
                    }
                }
            }
        }
    }

    fn measures(e: &Estate, s: &Snapshot) -> Measures {
        Measures::load(e.idx.conn(), s).expect("measures")
    }

    fn left(d: &Distribution, reason: &str) -> usize {
        d.left_out
            .iter()
            .filter(|l| l.reason == reason)
            .map(|l| l.machines)
            .sum()
    }

    /// Everything adds up: the bins to the machines drawn, those and the ones
    /// left out to the machines in view, and the quantiles sit in order inside
    /// the range drawn.
    fn assert_adds_up(d: &Distribution) {
        let binned: usize = d.bins.iter().map(|b| b.machines).sum();
        assert_eq!(binned, d.included, "bins {:?}", d.bins);
        assert_eq!(d.values.len(), d.included);
        let out: usize = d.left_out.iter().map(|l| l.machines).sum();
        assert_eq!(d.included + out, d.machines, "{:?}", d.left_out);
        for (i, p) in d.values.iter().enumerate() {
            let b = &d.bins[p.bin];
            let last = p.bin == d.bins.len() - 1;
            assert!(
                p.value >= b.lo && (p.value < b.hi || (last && p.value <= b.hi)),
                "{} in bin {i} {b:?}",
                p.value
            );
        }
        if d.included > 0 {
            let (p10, med, p90) = (d.p10.unwrap(), d.median.unwrap(), d.p90.unwrap());
            assert!(p10 <= med && med <= p90, "{p10} {med} {p90}");
            assert!(d.values[0].value <= p10 && p90 <= d.values[d.included - 1].value);
        }
    }

    /// Only runs measured the same way share an axis, and every machine left
    /// out is counted with its reason — here a different preset, a run with
    /// no disk at all, a different unit and a peak among medians.
    #[test]
    fn a_distribution_pools_only_comparable_runs_and_counts_the_rest() {
        let mut e = Estate::new();
        for (i, score) in [900.0, 1000.0, 1100.0, 1200.0, 1300.0, 1400.0]
            .into_iter()
            .enumerate()
        {
            e.add_with(&format!("PC-{i}"), score, "2026-09-08T10:00:00Z", |d| {
                subtest(d, "disk", "seq_read", |s| {
                    s["value"] = json!(1000.0 + 300.0 * i as f64)
                });
            });
        }
        e.add_with("PC-QUICK", 1000.0, "2026-09-08T10:00:00Z", |d| {
            d["config"]["duration_preset"] = json!("quick");
        });
        e.add_with("PC-UNIT", 1000.0, "2026-09-08T10:00:00Z", |d| {
            subtest(d, "disk", "seq_read", |s| s["unit"] = json!("GiB/s"));
        });
        e.add_with("PC-PEAK", 1000.0, "2026-09-08T10:00:00Z", |d| {
            subtest(d, "disk", "seq_read", |s| {
                s["representative"] = json!("peak");
            });
        });
        e.ingest("legacy-1.2.4.json");

        let s = e.snap();
        let m = measures(&e, &s);
        let d = distribution(&s, &m, &"disk/seq_read".parse().unwrap(), None, None).unwrap();
        assert_adds_up(&d);
        assert_eq!(d.machines, 10);
        assert_eq!(d.included, 6, "{:?}", d.left_out);
        assert_eq!(left(&d, "not_measured"), 1, "the legacy run has no disk");
        assert_eq!(left(&d, "other_configuration"), 1);
        assert_eq!(left(&d, "other_unit"), 1);
        assert_eq!(left(&d, "other_statistic"), 1);
        assert_eq!(d.unit, "MiB/s");
        assert_eq!(d.statistic.as_deref(), Some("median"));
        assert_eq!(d.configurations.len(), 2, "{:?}", d.configurations);
        assert!(d.config_label.as_deref().unwrap().starts_with("thorough"));

        // The other configuration is offered, and choosing it draws it alone.
        let quick = d
            .configurations
            .iter()
            .find(|c| c.key.starts_with("quick"))
            .expect("offered");
        assert_eq!(quick.machines, 1);
        let q = distribution(
            &s,
            &m,
            &"disk/seq_read".parse().unwrap(),
            Some(&quick.key),
            None,
        )
        .unwrap();
        assert_adds_up(&q);
        assert_eq!(q.included, 1);
        assert_eq!(left(&q, "other_configuration"), 8);

        // Quantiles of 1000, 1300 … 2500.
        assert!((d.median.unwrap() - 1750.0).abs() < 1e-9);
        assert!((d.p10.unwrap() - 1150.0).abs() < 1e-9);
        assert!((d.p90.unwrap() - 2350.0).abs() < 1e-9);
    }

    /// The overall score and a component's are drawn too, as scores; every
    /// fixture is one configuration, so all four real runs share the axis.
    #[test]
    fn scores_are_drawn_and_everything_adds_up() {
        let mut e = Estate::new();
        for f in [
            "win-modern.json",
            "linux-throttled.json",
            "low-end.json",
            "legacy-1.2.4.json",
        ] {
            e.ingest(f);
        }
        let s = e.snap();
        let m = measures(&e, &s);
        let d = distribution(&s, &m, &Measure::Overall, None, None).unwrap();
        assert_adds_up(&d);
        assert_eq!(d.included, 4);
        assert_eq!(d.direction.as_deref(), Some("higher_is_better"));
        let disk = distribution(&s, &m, &Measure::Component("disk".into()), None, None).unwrap();
        assert_adds_up(&disk);
        assert_eq!(disk.included, 3);
        assert_eq!(left(&disk, "not_measured"), 1);
        assert!(disk.counted);
        let net = distribution(&s, &m, &Measure::Component("network".into()), None, None).unwrap();
        assert!(!net.counted, "network is never graded");
        // The picker counts the machines that have each measure.
        let cpu = d.measures.iter().find(|g| g.measure == "cpu").unwrap();
        assert_eq!(cpu.machines, 4);
        assert_eq!(d.measures[0].measure, "cpu", "cpu first");
        assert!(
            cpu.subtests
                .iter()
                .any(|t| t.measure == "cpu/int_single" && t.machines == 4)
        );

        // One machine, and nobody at all, still add up.
        let one = s.filtered(&Filter {
            search: Some("FLEET-WIN-01".into()),
            ..Filter::default()
        });
        let d = distribution(&one, &m, &Measure::Overall, None, None).unwrap();
        assert_adds_up(&d);
        assert_eq!((d.included, d.bins.len()), (1, 1));
        let none = s.filtered(&Filter {
            search: Some("nothing-matches-this".into()),
            ..Filter::default()
        });
        let d = distribution(&none, &m, &Measure::Overall, None, None).unwrap();
        assert_adds_up(&d);
        assert_eq!(d.included, 0);
        assert!(d.median.is_none() && d.bins.is_empty() && d.config.is_none());
    }

    /// A subtest whose runs recorded no direction is drawn, and the view says
    /// it cannot tell which end is better rather than implying one.
    #[test]
    fn a_measure_without_direction_says_so() {
        let mut e = Estate::new();
        e.ingest("win-modern.json");
        e.ingest("low-end.json");
        e.idx
            .conn()
            .execute("UPDATE subtest SET direction = NULL", [])
            .unwrap();
        let s = e.snap();
        let m = measures(&e, &s);
        let d = distribution(&s, &m, &"memory/latency".parse().unwrap(), None, None).unwrap();
        assert_eq!(d.included, 2);
        assert!(d.direction.is_none());
        assert!(
            d.direction_note
                .as_deref()
                .unwrap()
                .contains("which end is better")
        );

        let mut e = Estate::new();
        e.ingest("win-modern.json");
        let s = e.snap();
        let m = measures(&e, &s);
        let d = distribution(&s, &m, &"memory/latency".parse().unwrap(), None, None).unwrap();
        assert_eq!(d.direction.as_deref(), Some("lower_is_better"));
        assert!(d.direction_note.is_none());
    }

    #[test]
    fn an_unknown_measure_or_configuration_is_refused_with_what_there_is() {
        let mut e = Estate::new();
        e.ingest("win-modern.json");
        let s = e.snap();
        let m = measures(&e, &s);
        for bad in ["", "disk/", "/x", "a/b/c"] {
            assert!(bad.parse::<Measure>().is_err(), "{bad:?}");
        }
        let err =
            distribution(&s, &m, &Measure::Component("floppy".into()), None, None).unwrap_err();
        assert!(err.contains("cpu"), "{err}");
        let err = distribution(&s, &m, &"disk/nope".parse().unwrap(), None, None).unwrap_err();
        assert!(err.contains("disk/seq_read"), "{err}");
        let err = distribution(&s, &m, &Measure::Overall, Some("quick|x|y|z"), None).unwrap_err();
        assert!(err.contains("thorough|general"), "{err}");
    }

    /// The split falls out of the same grouping the Groups view uses, and its
    /// groups add up to the machines drawn.
    #[test]
    fn a_split_groups_the_machines_drawn() {
        let mut e = Estate::new();
        for i in 0..5 {
            e.add_with(
                &format!("GLA-{i}"),
                1000.0 + 10.0 * i as f64,
                "2026-09-08T10:00:00Z",
                |d| {
                    d["tags"] = json!({ "site": "glasgow" });
                },
            );
        }
        e.add_with("EDI-0", 900.0, "2026-09-08T10:00:00Z", |d| {
            d["tags"] = json!({ "site": "edinburgh" });
        });
        // The fixture is tagged; this one is not.
        e.add_with("NONE-0", 950.0, "2026-09-08T10:00:00Z", |d| {
            d["tags"] = json!({})
        });
        let s = e.snap();
        let m = measures(&e, &s);
        let by = GroupBy::Tag("site".into());
        let d = distribution(&s, &m, &Measure::Overall, None, Some(&by)).unwrap();
        let split = d.split.expect("split");
        assert_eq!(split.by, "tag:site");
        assert_eq!(
            split.groups.iter().map(|g| g.machines).sum::<usize>(),
            d.included
        );
        assert_eq!(
            split.groups.last().unwrap().value,
            None,
            "no value goes last"
        );
        let gla = split
            .groups
            .iter()
            .find(|g| g.value.as_deref() == Some("glasgow"))
            .unwrap();
        assert!(!gla.thin && (gla.median - 1020.0).abs() < 1e-9);
        assert!(
            split
                .groups
                .iter()
                .find(|g| g.value.as_deref() == Some("edinburgh"))
                .unwrap()
                .thin
        );
    }

    /// Ten identical laptops, one with a disk graded F on an otherwise B
    /// machine: the cell is marked weak exactly where `component_weak` fires,
    /// and nowhere else.
    #[test]
    fn weak_cells_are_exactly_the_component_weak_findings() {
        let mut e = Estate::new();
        for i in 0..6 {
            e.add(&format!("PC-{i}"), 1000.0, "2026-09-08T10:00:00Z");
        }
        e.add_with("PC-DISK", 1000.0, "2026-09-08T10:00:00Z", |d| {
            component(d, "disk", 180.0, "F");
        });
        // D on a C machine is one grade down: no finding, so no mark.
        e.add_with("PC-MEH", 700.0, "2026-09-08T10:00:00Z", |d| {
            component(d, "memory", 450.0, "D");
        });
        // F on a D machine is only one grade down; D on an A machine is two.
        e.add_with("PC-LOW", 450.0, "2026-09-08T10:00:00Z", |d| {
            component(d, "disk", 150.0, "F");
        });
        e.add_with("PC-GOOD", 1200.0, "2026-09-08T10:00:00Z", |d| {
            component(d, "memory", 450.0, "D");
        });
        // Network graded F is never weak: it is not graded.
        e.add_with("PC-NET", 1000.0, "2026-09-08T10:00:00Z", |d| {
            component(d, "network", 90.0, "F");
        });
        e.ingest("legacy-1.2.4.json");
        let s = e.snap();
        let h = heatmap(
            &s,
            Encoding::Cohort,
            &HeatSort::Overall,
            false,
            HEATMAP_ROWS,
        )
        .unwrap();
        assert!(
            !h.components.contains(&"network".to_string()),
            "{:?}",
            h.components
        );

        let mut found: Vec<(String, String)> = s
            .flags
            .iter()
            .filter(|f| f.code == "component_weak")
            .map(|f| {
                let comp = f.headline.split_whitespace().next().unwrap().to_string();
                (f.machine_key.clone(), comp)
            })
            .collect();
        let mut marked: Vec<(String, String)> = h
            .rows
            .iter()
            .flat_map(|r| {
                r.cells
                    .iter()
                    .filter(|c| c.weak)
                    .map(|c| (r.key.clone(), c.component.clone()))
            })
            .collect();
        found.sort();
        marked.sort();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found, marked);
        assert_eq!(h.weak, 2);
        // The same in the other encoding: weak is the rule, not the colour.
        let g = heatmap(&s, Encoding::Grade, &HeatSort::Overall, false, HEATMAP_ROWS).unwrap();
        assert_eq!(g.weak, 2);
    }

    /// A cell is against the same component on identical machines. Below
    /// `min_cohort` of them there is nothing to compare with, and the cell is
    /// empty — not zero, which would read as exactly average.
    #[test]
    fn a_cell_is_against_its_cohort_and_empty_without_one() {
        let mut e = Estate::new();
        for i in 0..4 {
            e.add(&format!("PC-{i}"), 1000.0, "2026-09-08T10:00:00Z");
        }
        e.add_with("PC-SLOWDISK", 1000.0, "2026-09-08T10:00:00Z", |d| {
            let disk = d["components"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == json!("disk"))
                .unwrap()["score"]
                .as_f64()
                .unwrap();
            component(d, "disk", disk * 0.7, "A");
        });
        e.ingest("legacy-1.2.4.json");
        let s = e.snap();
        let h = heatmap(
            &s,
            Encoding::Cohort,
            &HeatSort::Component("disk".into()),
            false,
            200,
        )
        .unwrap();
        let disk = |r: &HeatRow| {
            r.cells
                .iter()
                .find(|c| c.component == "disk")
                .unwrap()
                .clone()
        };
        let top = &h.rows[0];
        assert_eq!(
            top.hostname.as_deref(),
            Some("PC-SLOWDISK"),
            "worst disk first"
        );
        assert!((disk(top).cohort_delta_pct.unwrap() + 30.0).abs() < 1e-6);
        let legacy = h
            .rows
            .iter()
            .find(|r| r.hostname.as_deref() == Some("FLEET-OLD-01"))
            .unwrap();
        let cpu = legacy.cells.iter().find(|c| c.component == "cpu").unwrap();
        assert!(cpu.measured && cpu.cohort_delta_pct.is_none(), "no peers");
        assert!(!disk(legacy).measured, "the legacy run has no disk");
        assert_eq!(
            h.rows.last().unwrap().key,
            legacy.key,
            "no value sorts last"
        );
        assert_eq!(h.without_peers, 1);
    }

    /// The cap is applied after sorting, and the response says how many
    /// there were.
    #[test]
    fn the_row_cap_reports_the_total() {
        let mut e = Estate::new();
        for i in 0..7 {
            e.add(
                &format!("PC-{i}"),
                900.0 + 10.0 * i as f64,
                "2026-09-08T10:00:00Z",
            );
        }
        let s = e.snap();
        let h = heatmap(&s, Encoding::Grade, &HeatSort::Overall, false, 3).unwrap();
        assert_eq!((h.machines, h.shown, h.cap, h.rows.len()), (7, 3, 3, 3));
        let scores: Vec<f64> = h.rows.iter().map(|r| r.score.unwrap()).collect();
        assert_eq!(
            scores,
            vec![900.0, 910.0, 920.0],
            "lowest first, then capped"
        );
        let h = heatmap(&s, Encoding::Grade, &HeatSort::Host, true, 2).unwrap();
        assert_eq!(h.rows[0].hostname.as_deref(), Some("PC-6"));
        assert!(
            heatmap(
                &s,
                Encoding::Cohort,
                &HeatSort::Component("network".into()),
                false,
                3
            )
            .is_err()
        );
        assert!("sideways".parse::<Encoding>().is_err());
    }
}
