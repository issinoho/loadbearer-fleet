//! The lines each machine is judged against: the fleet-wide thresholds, and
//! the overrides that replace some of them for some machines.
//!
//! ## Why per machine at all
//!
//! One set of lines does not fit every estate. A lab machine that is measured
//! once a year is not stale at ninety days; a kiosk with a small fixed image
//! runs at 8% free by design. Without a way to say so, the choice is between
//! loosening the line for everybody and a queue that carries the same known
//! findings forever.
//!
//! ## Which thresholds can be overridden
//!
//! **Only the ones that judge one machine against a fixed line.** Whether a
//! battery is worn, a disk nearly full or a result stale depends on nothing
//! but the machine and the line, so a different line for some machines is
//! well defined: it changes what is said about those machines and nothing
//! else. The cohort statistics are different in kind. `min_cohort` decides
//! whether a cohort is comparable at all, and `mad_floor_frac` sets the scale
//! every member's z-score is divided by, so a value for one member would be a
//! different calculation over the same machines; `cohort_z` and
//! `cohort_min_shortfall` are the two halves of the test that calculation
//! feeds, and are kept with it. All four are fleet-wide only, and an override
//! that sets one is refused at load rather than half-applied.
//!
//! `firmware_behind_days` compares a machine with its peers, but the peers'
//! newest firmware is worked out across the estate without reference to any
//! threshold, so a machine's own line moves only what is said about that
//! machine — the same position as `stale_days`, and overridable for the same
//! reason.
//!
//! ## How a machine's lines are chosen
//!
//! For each threshold, the first override in file order that picks the machine
//! *and sets that threshold*; otherwise `[thresholds]`; otherwise the built-in
//! default. First match wins, as it does for the lifecycle verdict, so a later
//! rule still supplies a threshold an earlier one left alone. An override picks
//! a machine by the tags, system model and CPU model of the run being judged —
//! tags through `scopes_allow`, the one rule for "these tags pick this
//! machine", and the models by the values `GroupBy` groups on, so a rule and a
//! group agree about what "this model" means.
//!
//! The lines are resolved once per machine, in `assess`, and carried on the
//! machine (`MachineView::thresholds`), so every consumer — the findings, the
//! lifecycle verdict, staleness and the counts built on it, the timeline —
//! reads the same answer and none can fall back to the fleet-wide value by
//! accident. Machines with the same overrides share one copy.
//!
//! A threshold is configuration, not history: the timeline re-judges the past
//! by today's lines, as it already does by today's rules.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Serialize;

use super::{GroupBy, MachineView, Thresholds, scopes_allow};

/// What kind of number a threshold is, which is what a configured value is
/// checked against.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// How many machines a cohort needs. See `Field::check`.
    CohortSize,
    /// How many of something; at least one.
    Count,
    /// Anything above zero: a z-score, days, years.
    Positive,
    /// A share, written as a fraction: above 0 and below 1.
    Fraction,
    /// A percentage, 0 to 100.
    Percent,
}

/// One threshold, by the name the config file and the snapshot give it.
#[derive(Debug)]
pub struct Field {
    pub name: &'static str,
    /// Whether an override may set it for some machines. See the module notes
    /// for why four may not.
    pub per_machine: bool,
    kind: Kind,
    set: fn(&mut Thresholds, f64),
}

/// Every threshold, in `Thresholds`' order — the one list the config reader,
/// the overrides and the checks below work from.
pub const FIELDS: [Field; 15] = [
    Field {
        name: "min_cohort",
        per_machine: false,
        kind: Kind::CohortSize,
        set: |t, v| t.min_cohort = v as usize,
    },
    Field {
        name: "cohort_z",
        per_machine: false,
        kind: Kind::Positive,
        set: |t, v| t.cohort_z = v,
    },
    Field {
        name: "cohort_min_shortfall",
        per_machine: false,
        kind: Kind::Fraction,
        set: |t, v| t.cohort_min_shortfall = v,
    },
    Field {
        name: "mad_floor_frac",
        per_machine: false,
        kind: Kind::Fraction,
        set: |t, v| t.mad_floor_frac = v,
    },
    Field {
        name: "regression_frac",
        per_machine: true,
        kind: Kind::Fraction,
        set: |t, v| t.regression_frac = v,
    },
    Field {
        name: "stale_days",
        per_machine: true,
        kind: Kind::Positive,
        set: |t, v| t.stale_days = v,
    },
    Field {
        name: "battery_health_pct",
        per_machine: true,
        kind: Kind::Percent,
        set: |t, v| t.battery_health_pct = v,
    },
    Field {
        name: "unstable_cpu_subtests",
        per_machine: true,
        kind: Kind::Count,
        set: |t, v| t.unstable_cpu_subtests = v as i64,
    },
    Field {
        name: "drive_wear_pct",
        per_machine: true,
        kind: Kind::Percent,
        set: |t, v| t.drive_wear_pct = v,
    },
    Field {
        name: "drive_spare_pct",
        per_machine: true,
        kind: Kind::Percent,
        set: |t, v| t.drive_spare_pct = v,
    },
    Field {
        name: "disk_free_pct",
        per_machine: true,
        kind: Kind::Percent,
        set: |t, v| t.disk_free_pct = v,
    },
    Field {
        name: "disk_free_critical_pct",
        per_machine: true,
        kind: Kind::Percent,
        set: |t, v| t.disk_free_critical_pct = v,
    },
    Field {
        name: "firmware_behind_days",
        per_machine: true,
        kind: Kind::Positive,
        set: |t, v| t.firmware_behind_days = v,
    },
    Field {
        name: "firmware_old_years",
        per_machine: true,
        kind: Kind::Positive,
        set: |t, v| t.firmware_old_years = v,
    },
    Field {
        name: "replace_age_years",
        per_machine: true,
        kind: Kind::Positive,
        set: |t, v| t.replace_age_years = v,
    },
];

/// The threshold of this name, or `None` for a name that is not one.
pub fn field(name: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.name == name)
}

impl Field {
    /// Set this threshold to `value`. Only meaningful for a value `check`
    /// accepted.
    pub fn apply(&self, th: &mut Thresholds, value: f64) {
        (self.set)(th, value);
    }

    /// Whether it counts something, and so takes a whole number.
    pub fn whole(&self) -> bool {
        matches!(self.kind, Kind::CohortSize | Kind::Count)
    }

    /// Whether `value` is one this threshold can take, or why not — the end of
    /// a sentence that starts with the setting's name.
    pub fn check(&self, value: f64) -> Result<(), String> {
        if !value.is_finite() {
            return Err("is not a finite number".to_string());
        }
        match self.kind {
            // Two is not enough. With two members each is exactly as far from
            // their median as the other, so the modified z-score of either is
            // at most 0.67 and nothing can ever stand out; three is the
            // smallest cohort in which one machine can.
            Kind::CohortSize if value < 3.0 => Err(format!(
                "is {value}; a cohort needs at least 3 machines. With two, each is exactly as far \
                 from their median as the other, so neither can ever be named as an outlier"
            )),
            Kind::Count if value < 1.0 => Err(format!(
                "is {value}; it is a count and has to be at least 1, or every run would be flagged"
            )),
            Kind::Positive if value <= 0.0 => {
                Err(format!("is {value}; it has to be more than zero"))
            }
            Kind::Fraction if value <= 0.0 || value >= 1.0 => Err(format!(
                "is {value}; it is a fraction, above 0 and below 1 — 0.15 means 15%"
            )),
            Kind::Percent if !(0.0..=100.0).contains(&value) => {
                Err(format!("is {value}; it is a percentage, from 0 to 100"))
            }
            _ => Ok(()),
        }
    }
}

/// The thresholds the engine runs with: the fleet-wide values, and the
/// overrides that replace some of them for some machines. What `[thresholds]`
/// in the config file becomes; the default is the built-in values and no
/// overrides, which is a config without that section.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tuning {
    pub fleet: Thresholds,
    /// In file order, which is the order they are tried in.
    pub overrides: Vec<Override>,
}

/// One `[[thresholds.overrides]]` rule, as validated: which machines it picks
/// and the thresholds it sets for them. Carried in every snapshot, so what
/// changed the lines is visible beside the lines.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Override {
    /// As configured, or made from the selector when it was not.
    pub name: String,
    /// Every one of these must be among the machine's tags. Empty is no
    /// condition on tags.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// Threshold name to value. Only names from `FIELDS` with `per_machine`.
    pub set: BTreeMap<&'static str, f64>,
}

impl Override {
    /// Whether this rule picks the machine: everything it gives must match.
    pub fn picks(&self, m: &MachineView) -> bool {
        (self.tags.is_empty() || scopes_allow(std::slice::from_ref(&self.tags), &m.tags))
            && self
                .system_model
                .as_deref()
                .is_none_or(|v| GroupBy::Model.value_of(m) == Some(v))
            && self
                .cpu_model
                .as_deref()
                .is_none_or(|v| GroupBy::Cpu.value_of(m) == Some(v))
    }

    /// The name a rule gets when it was not given one: its selector, as the
    /// config file spells it — `site=lab, system_model=Dell Inc. OptiPlex 3000`.
    pub fn describe(
        tags: &BTreeMap<String, String>,
        system_model: Option<&str>,
        cpu_model: Option<&str>,
    ) -> String {
        tags.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .chain(system_model.map(|v| format!("system_model={v}")))
            .chain(cpu_model.map(|v| format!("cpu_model={v}")))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Whether no machine can match both rules: they ask for different values
    /// of the same thing.
    pub fn disjoint(&self, other: &Override) -> bool {
        let differ =
            |a: Option<&str>, b: Option<&str>| matches!((a, b), (Some(a), Some(b)) if a != b);
        self.tags
            .iter()
            .any(|(k, v)| other.tags.get(k).is_some_and(|w| w != v))
            || differ(self.system_model.as_deref(), other.system_model.as_deref())
            || differ(self.cpu_model.as_deref(), other.cpu_model.as_deref())
    }

    /// Whether every machine `other` picks is picked by this rule too: each
    /// of this rule's conditions is one of `other`'s.
    fn covers(&self, other: &Override) -> bool {
        let within = |a: Option<&str>, b: Option<&str>| a.is_none() || a == b;
        self.tags.iter().all(|(k, v)| other.tags.get(k) == Some(v))
            && within(self.system_model.as_deref(), other.system_model.as_deref())
            && within(self.cpu_model.as_deref(), other.cpu_model.as_deref())
    }
}

/// An override that changed how one machine was judged, and the thresholds it
/// supplied for it — only those: a threshold it also sets that an earlier
/// rule supplied first is not listed, because it changed nothing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppliedOverride {
    pub name: String,
    pub set: BTreeMap<&'static str, f64>,
}

impl Tuning {
    /// Resolve every machine's own thresholds, and record which overrides
    /// supplied them. The one place a machine's lines are chosen.
    pub(super) fn apply(&self, machines: &mut [MachineView]) {
        let fleet = Arc::new(self.fleet.clone());
        // Keyed by which rule supplied each threshold, so a thousand kiosks
        // share one set of lines rather than holding a thousand copies.
        let mut made: HashMap<Vec<(&'static str, usize)>, Arc<Thresholds>> = HashMap::new();
        for m in machines {
            let mut from: BTreeMap<&'static str, usize> = BTreeMap::new();
            for (i, o) in self.overrides.iter().enumerate() {
                if o.picks(m) {
                    for name in o.set.keys() {
                        from.entry(name).or_insert(i);
                    }
                }
            }
            if from.is_empty() {
                m.thresholds = Arc::clone(&fleet);
                m.overridden_by = Vec::new();
                continue;
            }
            let key: Vec<(&'static str, usize)> = from.iter().map(|(f, i)| (*f, *i)).collect();
            let lines = made.entry(key).or_insert_with(|| {
                let mut th = self.fleet.clone();
                for (name, i) in &from {
                    if let Some(f) = field(name) {
                        f.apply(&mut th, self.overrides[*i].set[name]);
                    }
                }
                Arc::new(th)
            });
            m.thresholds = Arc::clone(lines);

            let mut by_rule: BTreeMap<usize, BTreeMap<&'static str, f64>> = BTreeMap::new();
            for (name, i) in from {
                by_rule
                    .entry(i)
                    .or_default()
                    .insert(name, self.overrides[i].set[name]);
            }
            m.overridden_by = by_rule
                .into_iter()
                .map(|(i, set)| AppliedOverride {
                    name: self.overrides[i].name.clone(),
                    set,
                })
                .collect();
        }
    }

    /// Overrides that can never change anything: every machine one picks is
    /// also picked by earlier rules that between them set every threshold it
    /// sets. Worth a warning rather than a refusal — the file means something,
    /// just not what its author thinks — and only the cheap, certain case is
    /// detected: earlier rules whose conditions are a subset of this one's.
    pub fn shadowed(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (j, later) in self.overrides.iter().enumerate() {
            let covering: Vec<&Override> = self.overrides[..j]
                .iter()
                .filter(|o| o.covers(later))
                .collect();
            let unset = later
                .set
                .keys()
                .any(|name| !covering.iter().any(|o| o.set.contains_key(name)));
            if !covering.is_empty() && !unset {
                out.push(format!(
                    "the threshold override {:?} never changes anything: every machine it picks \
                     is also picked by {}, which come(s) first and set(s) every threshold it sets. \
                     First match wins — move it above, or remove it",
                    later.name,
                    covering
                        .iter()
                        .map(|o| format!("{:?}", o.name))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ));
            }
        }
        out
    }
}
