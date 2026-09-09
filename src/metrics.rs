//! Prometheus metrics.
//!
//! Chosen for what an operator would page on, not for what is easy to count.
//! Three of these matter more than the rest:
//!
//! * `scan_last_success_timestamp_seconds` — collection has stopped. This is
//!   the failure that hides every other one, because a dashboard full of
//!   yesterday's green is indistinguishable from a healthy estate.
//! * `newest_run_age_seconds` — the *machines* have stopped reporting, which is
//!   a different fault from this service having stopped reading.
//! * `scan_files_rejected` — something is writing files this cannot parse. One
//!   is a truncated upload; a hundred is a broken collector.
//!
//! Deliberately **not** exported: anything keyed by machine, hostname or
//! cohort. Per-machine series would put the whole estate's inventory into a
//! metrics store that is usually less protected than this service is, and would
//! multiply the fleet's size by every label. The fleet view is the place to ask
//! about a machine; this is the place to ask whether the fleet view can be
//! trusted.

use std::fmt::Write as _;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::analytics::Snapshot;

/// What one scan did, kept so a scrape can report on collection health.
#[derive(Debug, Clone, Copy)]
pub struct ScanStamp {
    pub at: OffsetDateTime,
    pub seen: usize,
    pub ingested: usize,
    pub rejected: usize,
}

/// Counters that outlive any single snapshot.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub started: OffsetDateTime,
    pub scans_total: u64,
    pub scan_failures_total: u64,
    pub last_scan: Option<ScanStamp>,
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            started: OffsetDateTime::now_utc(),
            scans_total: 0,
            scan_failures_total: 0,
            last_scan: None,
        }
    }
}

const PREFIX: &str = "loadbearer_fleet";

struct Text(String);

impl Text {
    fn gauge(&mut self, name: &str, help: &str, value: f64) {
        self.header(name, help, "gauge");
        let _ = writeln!(self.0, "{PREFIX}_{name} {}", number(value));
    }

    fn counter(&mut self, name: &str, help: &str, value: f64) {
        self.header(name, help, "counter");
        let _ = writeln!(self.0, "{PREFIX}_{name} {}", number(value));
    }

    fn header(&mut self, name: &str, help: &str, kind: &str) {
        let _ = writeln!(self.0, "# HELP {PREFIX}_{name} {help}");
        let _ = writeln!(self.0, "# TYPE {PREFIX}_{name} {kind}");
    }

    /// A labelled family: the HELP and TYPE lines are written once, then one
    /// line per label set, which is what the exposition format requires.
    fn family(&mut self, name: &str, help: &str, kind: &str, rows: &[(Vec<(&str, &str)>, f64)]) {
        self.header(name, help, kind);
        for (labels, value) in rows {
            let rendered: Vec<String> = labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
                .collect();
            let _ = writeln!(
                self.0,
                "{PREFIX}_{name}{{{}}} {}",
                rendered.join(","),
                number(*value)
            );
        }
    }
}

/// Integers should not arrive at a metrics store as `7.0000000001`.
fn number(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else if v.is_finite() {
        format!("{v:.4}")
    } else {
        // The exposition format spells these out; NaN is legal and means
        // "no value", which is honest for a median of nothing.
        if v.is_nan() {
            "NaN".to_string()
        } else if v > 0.0 {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    }
}

/// Label values carry text that came out of a result file, so the three
/// characters the format reserves have to be escaped.
fn escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            '\n' => "\\n".to_string(),
            other => other.to_string(),
        })
        .collect()
}

fn age_seconds(timestamp: Option<&String>, now: OffsetDateTime) -> Option<f64> {
    let then = OffsetDateTime::parse(timestamp?, &Rfc3339).ok()?;
    Some((now - then).as_seconds_f64())
}

/// Render the whole exposition. `now` is a parameter so the age gauges are
/// testable, for the same reason the analytics take a clock.
pub fn render(snapshot: &Snapshot, runtime: &Runtime, now: OffsetDateTime) -> String {
    let s = &snapshot.summary;
    let mut out = Text(String::with_capacity(4096));

    out.family(
        "build_info",
        "Always 1; the version is in the label.",
        "gauge",
        &[(vec![("version", env!("CARGO_PKG_VERSION"))], 1.0)],
    );
    out.gauge(
        "uptime_seconds",
        "How long this process has been running.",
        (now - runtime.started).as_seconds_f64(),
    );

    out.gauge("machines", "Machines in the index.", s.machines as f64);
    out.gauge("runs", "Result files indexed.", s.runs as f64);
    out.gauge(
        "machines_flagged",
        "Machines carrying at least one warning or critical finding. Counts machines rather than \
         findings, because one machine can raise several.",
        s.machines_flagged as f64,
    );

    let grades: Vec<(Vec<(&str, &str)>, f64)> = s
        .grades
        .iter()
        .map(|(grade, n)| (vec![("grade", grade.as_str())], *n as f64))
        .collect();
    out.family(
        "machines_by_grade",
        "Machines at each grade against the reference baseline.",
        "gauge",
        &grades,
    );

    let findings = [
        (s.critical, "critical"),
        (s.warnings, "warning"),
        (s.info, "info"),
    ]
    .iter()
    .map(|(n, severity)| (vec![("severity", *severity)], *n as f64))
    .collect::<Vec<_>>();
    out.family("findings", "Open findings by severity.", "gauge", &findings);

    let quantiles: Vec<(Vec<(&str, &str)>, f64)> = [
        ("0.1", s.p10_score),
        ("0.5", s.median_score),
        ("0.9", s.p90_score),
    ]
    .iter()
    .map(|(q, v)| (vec![("quantile", *q)], v.unwrap_or(f64::NAN)))
    .collect();
    out.family(
        "score",
        "Distribution of machine scores across the fleet.",
        "gauge",
        &quantiles,
    );

    out.gauge(
        "machines_stale",
        "Machines whose last result is older than the staleness threshold.",
        s.stale as f64,
    );
    out.gauge(
        "machines_thermally_limited",
        "Machines whose last run was thermally limited, so its figures are what they sustain \
         rather than what they can do.",
        s.thermally_limited as f64,
    );
    out.gauge(
        "machines_partial_run",
        "Machines whose last run did not complete everything it was asked to.",
        s.partial as f64,
    );
    out.gauge(
        "machines_weak_identity",
        "Machines attributed by an identifier that does not survive a reimage.",
        s.weak_identity as f64,
    );
    out.gauge(
        "machines_without_peer_group",
        "Machines with no cohort large enough to compare them against.",
        s.uncohorted as f64,
    );
    out.gauge(
        "cohorts",
        "Distinct hardware-and-configuration groups.",
        s.cohorts as f64,
    );
    out.gauge(
        "cohorts_comparable",
        "Cohorts with enough members for their median to be worth comparing against.",
        s.comparable_cohorts as f64,
    );
    out.gauge(
        "configurations",
        "Distinct measurement configurations in the fleet. More than one means part of the estate \
         cannot be compared with the rest.",
        s.configurations.len() as f64,
    );

    if let Some(age) = age_seconds(s.newest_run.as_ref(), now) {
        out.gauge(
            "newest_run_age_seconds",
            "Age of the most recent result in the index. Alert on this: it means the machines \
             have stopped reporting, which is a different fault from this service having stopped \
             reading.",
            age,
        );
    }

    out.counter(
        "scans_total",
        "Folder scans attempted since startup.",
        runtime.scans_total as f64,
    );
    out.counter(
        "scan_failures_total",
        "Folder scans that could not be completed at all — an unreachable share, usually, rather \
         than an unreadable file.",
        runtime.scan_failures_total as f64,
    );
    if let Some(scan) = &runtime.last_scan {
        out.gauge(
            "scan_last_success_timestamp_seconds",
            "When the folder was last read successfully. Alert on this above everything else: \
             collection stopping is the failure that hides all the others, because a dashboard \
             full of yesterday's green looks exactly like a healthy estate.",
            scan.at.unix_timestamp() as f64,
        );
        out.gauge(
            "scan_files_seen",
            "Result files present in the collection folder at the last scan.",
            scan.seen as f64,
        );
        out.gauge(
            "scan_files_ingested",
            "Files that were new at the last scan.",
            scan.ingested as f64,
        );
        out.gauge(
            "scan_files_rejected",
            "Files at the last scan that could not be parsed. One is a truncated upload; a \
             hundred is a broken collector.",
            scan.rejected as f64,
        );
    }

    out.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::{Filter, Snapshot, Thresholds};
    use crate::index::Index;
    use std::path::Path;
    use time::Duration;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-09-09 12:00:00 UTC)
    }

    fn snapshot() -> Snapshot {
        let mut index = Index::open_in_memory().unwrap();
        for f in ["win-modern.json", "linux-throttled.json", "low-end.json"] {
            index
                .ingest_file(
                    &Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures")
                        .join(f),
                )
                .unwrap();
        }
        crate::analytics::snapshot(index.conn(), &Thresholds::default(), now()).unwrap()
    }

    fn runtime() -> Runtime {
        Runtime {
            started: now() - Duration::hours(3),
            scans_total: 12,
            scan_failures_total: 1,
            last_scan: Some(ScanStamp {
                at: now() - Duration::minutes(5),
                seen: 3,
                ingested: 0,
                rejected: 0,
            }),
        }
    }

    fn value_of(text: &str, line_prefix: &str) -> String {
        text.lines()
            .find(|l| l.starts_with(line_prefix) && !l.starts_with('#'))
            .unwrap_or_else(|| panic!("no metric line starting {line_prefix:?} in:\n{text}"))
            .rsplit(' ')
            .next()
            .expect("a value")
            .to_string()
    }

    #[test]
    fn the_exposition_is_well_formed() {
        let text = render(&snapshot(), &runtime(), now());
        // Every metric line must have a HELP and a TYPE ahead of it, which is
        // what a scraper needs to accept the family.
        let mut declared = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                declared.insert(rest.split(' ').next().unwrap().to_string());
                continue;
            }
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let name = line
                .split(['{', ' '])
                .next()
                .expect("a metric name")
                .to_string();
            assert!(declared.contains(&name), "{name} has no TYPE line");
            let value = line.rsplit(' ').next().unwrap();
            assert!(
                value.parse::<f64>().is_ok() || ["NaN", "+Inf", "-Inf"].contains(&value),
                "{line:?} does not end in a value"
            );
        }
        assert!(declared.len() > 10, "only {} families", declared.len());
    }

    #[test]
    fn the_fleet_counts_come_from_the_snapshot() {
        let snap = snapshot();
        let text = render(&snap, &runtime(), now());
        assert_eq!(
            value_of(&text, "loadbearer_fleet_machines "),
            snap.summary.machines.to_string()
        );
        assert_eq!(
            value_of(&text, "loadbearer_fleet_machines_thermally_limited "),
            snap.summary.thermally_limited.to_string()
        );
        assert!(text.contains("loadbearer_fleet_machines_by_grade{grade=\"S\"}"));
        assert!(text.contains("loadbearer_fleet_findings{severity=\"critical\"}"));
        assert!(text.contains("loadbearer_fleet_score{quantile=\"0.5\"}"));
        assert!(text.contains(concat!(
            "loadbearer_fleet_build_info{version=\"",
            env!("CARGO_PKG_VERSION"),
            "\"}"
        )));
    }

    /// The metric an operator should page on.
    #[test]
    fn collection_health_is_reported_in_seconds_not_prose() {
        let text = render(&snapshot(), &runtime(), now());
        let stamp = value_of(
            &text,
            "loadbearer_fleet_scan_last_success_timestamp_seconds ",
        );
        let expected = (now() - Duration::minutes(5)).unix_timestamp();
        assert_eq!(stamp, expected.to_string());
        assert_eq!(value_of(&text, "loadbearer_fleet_uptime_seconds "), "10800");
        assert_eq!(value_of(&text, "loadbearer_fleet_scans_total "), "12");
        assert_eq!(
            value_of(&text, "loadbearer_fleet_scan_failures_total "),
            "1"
        );
    }

    /// Before the first scan finishes there is nothing to say about scans, and
    /// a zero timestamp would read as 1970 — an alert firing for the wrong
    /// reason.
    #[test]
    fn a_scan_that_has_not_happened_yet_reports_nothing_rather_than_zero() {
        let mut rt = runtime();
        rt.last_scan = None;
        let text = render(&snapshot(), &rt, now());
        assert!(!text.contains("scan_last_success_timestamp_seconds"));
        assert!(!text.contains("scan_files_seen"));
        // The counters still exist, so "no scan has ever succeeded" is visible.
        assert!(text.contains("loadbearer_fleet_scans_total"));
    }

    #[test]
    fn an_empty_fleet_renders_without_pretending_to_have_a_median() {
        let index = Index::open_in_memory().unwrap();
        let snap = crate::analytics::snapshot(index.conn(), &Thresholds::default(), now()).unwrap();
        let text = render(&snap, &runtime(), now());
        assert_eq!(value_of(&text, "loadbearer_fleet_machines "), "0");
        assert!(
            text.contains("loadbearer_fleet_score{quantile=\"0.5\"} NaN"),
            "the median of nothing is not zero:\n{text}"
        );
        assert!(!text.contains("newest_run_age_seconds"));
    }

    /// Metrics are an operator's view of the whole service, so they are taken
    /// from the fleet-wide snapshot. A filtered projection would make a scrape
    /// depend on whose session happened to trigger it.
    #[test]
    fn the_numbers_are_fleet_wide() {
        let snap = snapshot();
        let narrowed = snap.filtered(
            &Filter {
                search: Some("nothing-matches-this".into()),
                ..Default::default()
            },
            &Thresholds::default(),
        );
        assert_eq!(narrowed.summary.machines, 0);
        let text = render(&snap, &runtime(), now());
        assert_eq!(
            value_of(&text, "loadbearer_fleet_machines "),
            snap.summary.machines.to_string()
        );
    }

    #[test]
    fn a_label_value_cannot_break_out_of_its_quotes() {
        assert_eq!(escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape("two\nlines"), "two\\nlines");
    }

    #[test]
    fn integers_are_not_rendered_as_floats() {
        assert_eq!(number(7.0), "7");
        assert_eq!(number(-3.0), "-3");
        assert_eq!(number(993.5), "993.5000");
        assert_eq!(number(f64::NAN), "NaN");
        assert_eq!(number(f64::INFINITY), "+Inf");
        assert_eq!(number(f64::NEG_INFINITY), "-Inf");
    }
}
