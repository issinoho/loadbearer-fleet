//! The fleet over time: how it looked at each of the last few weeks, and what
//! changed between two of them.
//!
//! ## Reconstructed, not recorded
//!
//! Nothing here is written down as it happens. There is no table of weekly
//! summaries: the index already keeps one row per run with the time it was
//! taken, so the fleet *as it was known at time T* can be worked out again
//! whenever it is wanted — for each machine, its latest run taken at or before
//! T, judged with T as "now", with only the runs at or before T as its history
//! (`super::reconstruct`). That is the whole mechanism, and it was chosen over
//! recording for four reasons:
//!
//! * **History arrives retroactively.** An index that has been collecting for
//!   a year has a year of timeline the first time this build opens it, rather
//!   than starting from the day it was upgraded.
//! * **It survives a rebuild.** An index that is superseded and rebuilt from
//!   the folder — see `index`'s module notes — rebuilds its past with it. A
//!   recorded summary would be lost, or would have to be carried across.
//! * **No schema change.** Nothing new to migrate, back up or keep in step.
//! * **The past is judged by today's rules.** A threshold tuned or a rule added
//!   today applies to every week of the series at once, so a line that moves
//!   moved because the fleet did and not because the rules did. A recorded
//!   series would have a step in it at every upgrade.
//!
//! The honest caveats, each the same mechanism seen from another side:
//!
//! * **A forgotten machine leaves the past too.** `forget` removes its runs,
//!   so it is gone from every week, not just from now.
//! * **A run that arrives late changes the past.** A result taken a month ago
//!   and ingested today appears a month ago. That is correct — it is when the
//!   machine was measured, not when we heard — but it means a week's figures
//!   can move after the week has passed.
//! * **Old runs only know what was indexed from them.** Columns added by a
//!   later build are empty on runs indexed before it until `scan --reindex`,
//!   so a drive finding cannot appear in a week before the drive data does,
//!   and a jump in drive findings can be a reindex rather than the drives.
//! * **A run whose timestamp does not parse is never placed in the past.** It
//!   still counts *now*, as it always has; it just cannot be shown to precede
//!   any earlier date.
//!
//! ## What is kept
//!
//! A slim record per machine per week — key, score, grade, whether stale, and
//! each finding's code, severity, queue and headline — not the full machine
//! views, which carry components, drives, tags and every finding's full
//! explanation, and at a few thousand machines and twenty-seven weeks would
//! run to hundreds of megabytes. Strings that repeat from week to week (keys,
//! hostnames, grades, the headlines that don't carry a number) are shared
//! rather than copied. At 2,000 machines with eight runs each the whole
//! half-year holds about 5 MB and takes about 0.3 s to build in a release
//! build — see the `timeline_build_cost` test, which measures it.
//!
//! ## Who sees what
//!
//! Like everything else, the timeline is projected onto the caller's view
//! rather than filtered by its own rules: a machine is in a past week if it is
//! in the caller's *current* filtered, scope-limited snapshot. Every machine in
//! any past week is in the current snapshot — its latest run is still there —
//! so the projection is total, and authorization stays decided in the one
//! place it already is. The consequence is that a machine judged by its
//! *current* tags brings its whole history with it: one that moved from
//! Glasgow to Edinburgh shows in Edinburgh's past as well, exactly as its
//! drilldown already shows every run it has.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use super::{
    FlagKind, QueueCounts, Severity, Snapshot, Thresholds, grade_histogram, grade_rank, is_stale,
    median, needs_attention, reconstruct, sorted_of,
};

/// How far back the server keeps the timeline: half a year, weekly.
///
/// Not two years, though it would be one number to change: the timeline is
/// rebuilt with the snapshot, holding the index lock, and at 2,000 machines
/// each measured every three weeks, two years of weekly points took 1.8 s and
/// held 24 MB, against half a year's 0.3 s and 5 MB.
pub const TIMELINE_WEEKS: usize = 26;

/// The comparisons "what changed" offers: a week, a month, a quarter.
pub const CHANGE_WINDOWS: [usize; 3] = [1, 4, 13];

/// A change window as typed — on a command line or a query string — checked
/// against the three on offer. Anything else is refused rather than rounded,
/// because a comparison against a date nobody asked for reads as an answer.
pub fn parse_window(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(w) if CHANGE_WINDOWS.contains(&w) => Ok(w),
        _ => Err(format!(
            "{s:?} is not a change window. Use 1, 4 or 13 (weeks): a week, a month or a quarter"
        )),
    }
}

/// How many weeks of timeline to send, as typed. At least four, because a line
/// through fewer points claims a trend that isn't there; at most what the
/// server keeps.
pub fn parse_weeks(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(w) if (4..=TIMELINE_WEEKS).contains(&w) => Ok(w),
        _ => Err(format!(
            "{s:?} is not a number of weeks this server keeps. Use 4 to {TIMELINE_WEEKS}"
        )),
    }
}

/// One finding, as the timeline keeps it.
#[derive(Debug, Clone)]
struct Held {
    code: &'static str,
    severity: Severity,
    kind: FlagKind,
    headline: Arc<str>,
}

/// One machine at one point in time.
#[derive(Debug, Clone)]
struct Seen {
    key: Arc<str>,
    hostname: Option<Arc<str>>,
    score: Option<f64>,
    grade: Arc<str>,
    stale: bool,
    flags: Vec<Held>,
}

impl Seen {
    fn name(&self) -> &str {
        self.hostname.as_deref().unwrap_or(&self.key)
    }

    /// By the same rule as `Summary::machines_flagged`.
    fn flagged(&self) -> bool {
        self.flags.iter().any(|f| needs_attention(f.severity))
    }

    fn critical(&self) -> bool {
        self.flags.iter().any(|f| f.severity == Severity::Critical)
    }
}

/// The fleet as it was known at one instant.
#[derive(Debug)]
struct Moment {
    at: OffsetDateTime,
    /// In machine-key order.
    machines: Vec<Seen>,
}

/// Shares the strings that repeat from one week to the next.
#[derive(Default)]
struct Interner(HashSet<Arc<str>>);

impl Interner {
    fn get(&mut self, s: &str) -> Arc<str> {
        if let Some(shared) = self.0.get(s) {
            return Arc::clone(shared);
        }
        let shared: Arc<str> = Arc::from(s);
        self.0.insert(Arc::clone(&shared));
        shared
    }
}

impl Moment {
    fn of(at: OffsetDateTime, snap: &Snapshot, th: &Thresholds, strings: &mut Interner) -> Self {
        let mut flags: HashMap<&str, Vec<Held>> = HashMap::new();
        for f in &snap.flags {
            flags.entry(f.machine_key.as_str()).or_default().push(Held {
                code: f.code,
                severity: f.severity,
                kind: f.kind,
                headline: strings.get(&f.headline),
            });
        }
        let machines = snap
            .machines
            .iter()
            .map(|m| Seen {
                key: strings.get(&m.key),
                hostname: m.hostname.as_deref().map(|h| strings.get(h)),
                score: m.score,
                grade: strings.get(&m.grade),
                stale: is_stale(m, th),
                flags: flags.remove(m.key.as_str()).unwrap_or_default(),
            })
            .collect();
        Moment { at, machines }
    }

    /// The machines of this moment that are in the caller's view.
    fn in_view<'a>(&'a self, keys: &'a HashSet<&str>) -> impl Iterator<Item = &'a Seen> + 'a {
        self.machines.iter().filter(|m| keys.contains(&*m.key))
    }

    fn point(&self, keys: &HashSet<&str>) -> TimelinePoint {
        let ms: Vec<&Seen> = self.in_view(keys).collect();
        let scores = sorted_of(ms.iter().filter_map(|m| m.score));
        let mut queues = QueueCounts::default();
        let (mut critical, mut warnings, mut info) = (0, 0, 0);
        for f in ms.iter().flat_map(|m| &m.flags) {
            queues.add(f.kind);
            match f.severity {
                Severity::Critical => critical += 1,
                Severity::Warning => warnings += 1,
                Severity::Info => info += 1,
            }
        }
        TimelinePoint {
            at: day(self.at),
            machines: ms.len(),
            grades: grade_histogram(ms.iter().map(|m| &*m.grade)),
            median_score: (!scores.is_empty()).then(|| median(&scores)),
            machines_flagged: ms.iter().filter(|m| m.flagged()).count(),
            machines_critical: ms.iter().filter(|m| m.critical()).count(),
            critical,
            warnings,
            info,
            queues,
            stale: ms.iter().filter(|m| m.stale).count(),
        }
    }
}

fn day(at: OffsetDateTime) -> String {
    at.date().to_string()
}

/// The fleet at each of the last few weeks, held beside the snapshot and
/// projected per request. See the module notes for how it is made and what
/// that means.
#[derive(Debug, Default)]
pub struct Timeline {
    /// Oldest first, one week apart, the last being the live snapshot itself.
    /// Weeks from before the first run are kept, empty, so that "k weeks ago"
    /// is always `len - 1 - k`; they are dropped when the timeline is sent.
    points: Vec<Moment>,
}

impl Timeline {
    /// `weeks` points back from `now`, plus `now` itself — which is `current`,
    /// the live snapshot, rather than a reconstruction of it, so the newest
    /// point always agrees with the overview's headline.
    pub fn build(
        conn: &Connection,
        th: &Thresholds,
        current: &Snapshot,
        now: OffsetDateTime,
        weeks: usize,
    ) -> Result<Self> {
        let cutoffs: Vec<OffsetDateTime> = (1..=weeks)
            .rev()
            .map(|k| now - Duration::weeks(k as i64))
            .collect();
        let mut strings = Interner::default();
        let mut points = Vec::with_capacity(weeks + 1);
        reconstruct(conn, th, &cutoffs, |at, snap| {
            points.push(Moment::of(at, &snap, th, &mut strings));
            Ok(())
        })?;
        points.push(Moment::of(now, current, th, &mut strings));
        Ok(Timeline { points })
    }

    /// The last `weeks` weeks as `view` sees them: only the machines in it, at
    /// every point, and the figures worked out after that projection. Points
    /// from before the first of those machines was measured are left out —
    /// before then there is no data, and a run of zeros would say the fleet
    /// was empty rather than unmeasured.
    pub fn view(&self, view: &Snapshot, weeks: usize) -> TimelineView {
        let keys = keys_of(view);
        let from = self.points.len().saturating_sub(weeks + 1);
        let points: Vec<TimelinePoint> = self.points[from..]
            .iter()
            .map(|p| p.point(&keys))
            .skip_while(|p| p.machines == 0)
            .collect();
        TimelineView {
            generated_at: self
                .points
                .last()
                .and_then(|p| p.at.format(&Rfc3339).ok())
                .unwrap_or_default(),
            weeks,
            points,
        }
    }

    /// What changed between now and `window` weeks ago, among the machines in
    /// `view`. A finding is the same finding if it is the same rule on the
    /// same machine.
    pub fn changes(&self, view: &Snapshot, window: usize) -> Changes {
        let keys = keys_of(view);
        let now = self.points.last();
        let then = self
            .points
            .len()
            .checked_sub(window + 1)
            .and_then(|i| self.points.get(i));
        let to_at = now.map_or_else(OffsetDateTime::now_utc, |p| p.at);
        let mut out = Changes {
            window_weeks: window,
            from: day(then.map_or(to_at - Duration::weeks(window as i64), |p| p.at)),
            to: day(to_at),
            baseline_empty: false,
            counts: ChangeCounts::default(),
            new_findings: Vec::new(),
            worsened: Vec::new(),
            machines_added: Vec::new(),
            grade_changes: Vec::new(),
            resolved: Vec::new(),
            eased: Vec::new(),
        };

        let before: HashMap<&str, &Seen> = then
            .map(|p| p.in_view(&keys).map(|m| (&*m.key, m)).collect())
            .unwrap_or_default();
        // Nothing in view had been measured by then. Reporting every machine
        // and every finding as new would be true to the letter and useless:
        // it is the absence of a baseline, and is said as that.
        if before.is_empty() {
            out.baseline_empty = true;
            return out;
        }
        let Some(now) = now else {
            return out;
        };

        for m in now.in_view(&keys) {
            let Some(was) = before.get(&*m.key) else {
                out.machines_added.push(MachineAdded {
                    machine_key: m.key.to_string(),
                    hostname: m.hostname.as_deref().map(str::to_string),
                    grade: m.grade.to_string(),
                    flagged: m.flagged(),
                });
                for f in &m.flags {
                    out.new_findings.push(FindingChange::of(m, f, true));
                }
                continue;
            };

            for f in &m.flags {
                match was.flags.iter().find(|p| p.code == f.code) {
                    None => out.new_findings.push(FindingChange::of(m, f, false)),
                    // Ord on Severity runs Critical first, so "less" is worse.
                    Some(p) if f.severity < p.severity => {
                        out.worsened.push(SeverityChange::of(m, f, p.severity))
                    }
                    Some(p) if f.severity > p.severity => {
                        out.eased.push(SeverityChange::of(m, f, p.severity))
                    }
                    Some(_) => {}
                }
            }
            for p in &was.flags {
                if !m.flags.iter().any(|f| f.code == p.code) {
                    out.resolved.push(FindingChange::of(was, p, false));
                }
            }

            if m.grade != was.grade {
                let direction = match (grade_rank(&was.grade), grade_rank(&m.grade)) {
                    (Some(a), Some(b)) if b > a => GradeDirection::Down,
                    (Some(a), Some(b)) if b < a => GradeDirection::Up,
                    _ => GradeDirection::Changed,
                };
                out.grade_changes.push(GradeChange {
                    machine_key: m.key.to_string(),
                    hostname: m.hostname.as_deref().map(str::to_string),
                    from: was.grade.to_string(),
                    to: m.grade.to_string(),
                    direction,
                });
            }
        }

        // Most severe first, then by name, so the list reads top-down the way
        // the overview's findings do — and an informational change, which is
        // real but rarely the news, sinks below everything that is.
        let by_severity = |a: &FindingChange, b: &FindingChange| {
            (a.severity, &a.name, a.code).cmp(&(b.severity, &b.name, b.code))
        };
        out.new_findings.sort_by(by_severity);
        out.resolved.sort_by(by_severity);
        out.worsened
            .sort_by(|a, b| (a.to, &a.name, a.code).cmp(&(b.to, &b.name, b.code)));
        out.eased
            .sort_by(|a, b| (a.from, &a.name, a.code).cmp(&(b.from, &b.name, b.code)));
        out.grade_changes.sort_by(|a, b| {
            (a.direction, a.hostname.as_deref().unwrap_or(&a.machine_key))
                .cmp(&(b.direction, b.hostname.as_deref().unwrap_or(&b.machine_key)))
        });
        out.machines_added.sort_by(|a, b| {
            a.hostname
                .as_deref()
                .unwrap_or(&a.machine_key)
                .cmp(b.hostname.as_deref().unwrap_or(&b.machine_key))
        });

        let c = &mut out.counts;
        for f in &out.new_findings {
            if f.new_machine {
                c.new_machine_findings.add(f.severity);
            } else {
                c.new_findings.add(f.severity);
            }
        }
        for f in &out.resolved {
            c.resolved.add(f.severity);
        }
        c.worsened = out.worsened.len();
        c.eased = out.eased.len();
        c.machines_added = out.machines_added.len();
        c.machines_added_flagged = out.machines_added.iter().filter(|m| m.flagged).count();
        c.grades_down = out
            .grade_changes
            .iter()
            .filter(|g| g.direction == GradeDirection::Down)
            .count();
        c.grades_up = out
            .grade_changes
            .iter()
            .filter(|g| g.direction == GradeDirection::Up)
            .count();
        out
    }

    /// Roughly what the timeline holds on the heap, for the cost test. Counts
    /// each shared string once, as it is stored.
    #[cfg(test)]
    pub(super) fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let mut shared: HashSet<*const u8> = HashSet::new();
        let mut bytes = self.points.capacity() * size_of::<Moment>();
        let mut string = |s: &Arc<str>| {
            if shared.insert(s.as_ptr()) {
                // The text and Arc's two counts.
                s.len() + 16
            } else {
                0
            }
        };
        for p in &self.points {
            bytes += p.machines.capacity() * size_of::<Seen>();
            for m in &p.machines {
                bytes += string(&m.key) + string(&m.grade);
                bytes += m.hostname.as_ref().map_or(0, &mut string);
                bytes += m.flags.capacity() * size_of::<Held>();
                for f in &m.flags {
                    bytes += string(&f.headline);
                }
            }
        }
        bytes
    }

    #[cfg(test)]
    pub(super) fn dates(&self) -> Vec<OffsetDateTime> {
        self.points.iter().map(|p| p.at).collect()
    }
}

fn keys_of(view: &Snapshot) -> HashSet<&str> {
    view.machines.iter().map(|m| m.key.as_str()).collect()
}

/// The fleet at one point, as the caller sees it.
#[derive(Debug, Clone, Serialize)]
pub struct TimelinePoint {
    /// `YYYY-MM-DD`.
    pub at: String,
    pub machines: usize,
    /// Same shape and order as `Summary::grades`.
    pub grades: Vec<(String, usize)>,
    pub median_score: Option<f64>,
    /// By the same rule as `Summary::machines_flagged`.
    pub machines_flagged: usize,
    pub machines_critical: usize,
    /// Findings, not machines.
    pub critical: usize,
    pub warnings: usize,
    pub info: usize,
    pub queues: QueueCounts,
    pub stale: usize,
}

/// `/api/timeline`: the points, oldest first.
#[derive(Debug, Clone, Serialize)]
pub struct TimelineView {
    pub generated_at: String,
    /// How many weeks were asked for. There are fewer points than this plus
    /// one when the data doesn't go back that far.
    pub weeks: usize,
    pub points: Vec<TimelinePoint>,
}

/// Findings by severity, so a headline can say how many of a change were
/// critical or warning and how many were informational — a `stale` appearing
/// on a dozen machines is real, and is not the same news as one drive failing.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct BySeverity {
    pub critical: usize,
    pub warning: usize,
    pub info: usize,
}

impl BySeverity {
    fn add(&mut self, s: Severity) {
        match s {
            Severity::Critical => self.critical += 1,
            Severity::Warning => self.warning += 1,
            Severity::Info => self.info += 1,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChangeCounts {
    /// New on machines that were already in the fleet.
    pub new_findings: BySeverity,
    /// Carried by machines measured for the first time since — news of a
    /// different kind, counted apart so it doesn't read as the fleet worsening.
    pub new_machine_findings: BySeverity,
    pub resolved: BySeverity,
    pub worsened: usize,
    pub eased: usize,
    pub machines_added: usize,
    /// Of those, how many carry a critical or warning finding.
    pub machines_added_flagged: usize,
    pub grades_down: usize,
    pub grades_up: usize,
}

/// A finding that appeared, or went away.
#[derive(Debug, Clone, Serialize)]
pub struct FindingChange {
    pub machine_key: String,
    pub hostname: Option<String>,
    pub code: &'static str,
    pub severity: Severity,
    pub kind: FlagKind,
    pub headline: String,
    /// On a machine that had not been measured by the earlier date.
    pub new_machine: bool,
    #[serde(skip)]
    name: String,
}

impl FindingChange {
    fn of(m: &Seen, f: &Held, new_machine: bool) -> Self {
        FindingChange {
            machine_key: m.key.to_string(),
            hostname: m.hostname.as_deref().map(str::to_string),
            code: f.code,
            severity: f.severity,
            kind: f.kind,
            headline: f.headline.to_string(),
            new_machine,
            name: m.name().to_string(),
        }
    }
}

/// The same finding on the same machine at another severity.
#[derive(Debug, Clone, Serialize)]
pub struct SeverityChange {
    pub machine_key: String,
    pub hostname: Option<String>,
    pub code: &'static str,
    pub kind: FlagKind,
    pub from: Severity,
    pub to: Severity,
    /// As it reads now.
    pub headline: String,
    #[serde(skip)]
    name: String,
}

impl SeverityChange {
    fn of(m: &Seen, f: &Held, from: Severity) -> Self {
        SeverityChange {
            machine_key: m.key.to_string(),
            hostname: m.hostname.as_deref().map(str::to_string),
            code: f.code,
            kind: f.kind,
            from,
            to: f.severity,
            headline: f.headline.to_string(),
            name: m.name().to_string(),
        }
    }
}

/// Worse first, so a list sorted by it leads with the news.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GradeDirection {
    Down,
    /// To or from a grade this build doesn't recognise, which has no order.
    Changed,
    Up,
}

#[derive(Debug, Clone, Serialize)]
pub struct GradeChange {
    pub machine_key: String,
    pub hostname: Option<String>,
    pub from: String,
    pub to: String,
    pub direction: GradeDirection,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineAdded {
    pub machine_key: String,
    pub hostname: Option<String>,
    pub grade: String,
    /// Carries a critical or warning finding.
    pub flagged: bool,
}

/// `/api/changes` and `report --changes --json`.
#[derive(Debug, Clone, Serialize)]
pub struct Changes {
    pub window_weeks: usize,
    /// The two dates compared, `YYYY-MM-DD`.
    pub from: String,
    pub to: String,
    /// Nothing in view had been measured by `from`, so there is nothing to
    /// compare against and every list is empty — rather than the whole fleet
    /// being reported as new.
    pub baseline_empty: bool,
    pub counts: ChangeCounts,
    /// Most severe first, then by machine. Includes the findings of machines
    /// added since, marked `new_machine`.
    pub new_findings: Vec<FindingChange>,
    pub worsened: Vec<SeverityChange>,
    pub machines_added: Vec<MachineAdded>,
    /// Down first, then up.
    pub grade_changes: Vec<GradeChange>,
    pub resolved: Vec<FindingChange>,
    pub eased: Vec<SeverityChange>,
}

impl Changes {
    /// True when the two dates compared hold the same picture.
    pub fn is_empty(&self) -> bool {
        self.new_findings.is_empty()
            && self.worsened.is_empty()
            && self.machines_added.is_empty()
            && self.grade_changes.is_empty()
            && self.resolved.is_empty()
            && self.eased.is_empty()
    }
}
