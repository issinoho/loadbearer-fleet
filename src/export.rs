//! The machines and the findings as CSV, for the people who will never open
//! the dashboard: a spreadsheet for procurement, a list for a review meeting.
//!
//! Built from a snapshot that has already been projected for the caller —
//! `Snapshot::filtered_with`, through `visible` on the server and the same
//! call in `report` — so a file holds exactly the machines and findings the
//! view it was asked from holds, and nothing a scope excludes. Nothing here
//! filters, and nothing here re-derives a figure: every cell is a field the
//! snapshot already carries, so the file cannot disagree with the dashboard.
//!
//! Three rules shape the encoding, each because a spreadsheet is the reader:
//!
//! - **Stable and machine-readable.** A header row, a fixed column order with
//!   the tag columns after the fixed ones, numbers as plain decimals (no
//!   thousands separator, a `.` point), empty for a missing value rather than
//!   a dash, times as RFC 3339 in UTC, and RFC 4180 quoting with CRLF line
//!   ends — whatever platform wrote it.
//! - **No cell is a formula.** Hostnames, tag values, notes and findings come
//!   from machines in the field and from people. A text cell that begins with
//!   `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed with `'`, the
//!   OWASP recommendation, so Excel and Sheets read it as text rather than
//!   running it. Numeric columns are written by another path and never
//!   prefixed: a negative cohort delta is a number, not an attack.
//! - **No byte-order mark here.** `report --csv` is piped into other tools,
//!   which do not want one; the HTTP download adds it (`BOM`), because Excel on
//!   Windows reads BOM-less UTF-8 as the ANSI code page and turns every
//!   accented hostname into mojibake.

use std::collections::BTreeSet;

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::analytics::{FlagKind, MachineView, Severity, Snapshot};

/// What a browser download starts with: see the module notes.
pub const BOM: &str = "\u{feff}";

/// Which table to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Machines,
    Findings,
}

impl Table {
    pub fn as_str(self) -> &'static str {
        match self {
            Table::Machines => "machines",
            Table::Findings => "findings",
        }
    }

    /// The table, as CSV, from a snapshot already projected for its reader.
    pub fn render(self, snap: &Snapshot) -> String {
        match self {
            Table::Machines => machines(snap),
            Table::Findings => findings(snap),
        }
    }

    /// `loadbearer-fleet-machines-2026-11-13.csv`: what it is and when, so a
    /// folder of downloads sorts and explains itself.
    pub fn filename(self, now: OffsetDateTime) -> String {
        format!(
            "loadbearer-fleet-{}-{}.csv",
            self.as_str(),
            now.to_offset(UtcOffset::UTC).date()
        )
    }
}

impl std::str::FromStr for Table {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "machines" => Ok(Table::Machines),
            "findings" => Ok(Table::Findings),
            other => Err(format!(
                "{other:?} is not a table. Use machines or findings"
            )),
        }
    }
}

/// One cell, typed by what it holds rather than by what it looks like — the
/// distinction the formula rule turns on.
enum Cell {
    /// Anything a person or a machine wrote, or a word this tool chose.
    Text(String),
    /// A figure. `None`, or a value that is not finite, is an empty cell.
    Num(Option<f64>),
    Int(Option<i64>),
}

use Cell::{Int, Num, Text};

fn text(s: impl Into<String>) -> Cell {
    Text(s.into())
}

/// An optional text field: empty when absent.
fn opt(s: Option<&str>) -> Cell {
    Text(s.unwrap_or_default().to_string())
}

/// The first characters a spreadsheet takes as the start of a formula, or
/// (tab, carriage return) as a way of smuggling one past a check for them.
const FORMULA_LEADS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// A text cell made safe to open: `'` in front of anything a spreadsheet
/// would evaluate.
fn defused(s: &str) -> std::borrow::Cow<'_, str> {
    if s.starts_with(FORMULA_LEADS) {
        format!("'{s}").into()
    } else {
        s.into()
    }
}

/// A field as RFC 4180 wants it: quoted, with quotes doubled, when it holds a
/// comma, a quote, a CR or an LF; as it is otherwise.
fn quoted(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\"")).into()
    } else {
        s.into()
    }
}

/// Shortest round-trip decimal — what the JSON carries, so a cell parses back
/// to the same number the snapshot holds. Rust never writes an exponent or a
/// separator here.
fn number(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        String::new()
    }
}

/// One row, CRLF-terminated.
fn row(out: &mut String, cells: impl IntoIterator<Item = Cell>) {
    for (i, cell) in cells.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match cell {
            Text(s) => out.push_str(&quoted(&defused(&s))),
            Num(v) => out.push_str(&v.map(number).unwrap_or_default()),
            Int(v) => out.push_str(&v.map(|n| n.to_string()).unwrap_or_default()),
        }
    }
    out.push_str("\r\n");
}

/// A header row: the names are this file's own, but go through the same
/// encoding as any other text, since a tag key is part of one.
fn header(out: &mut String, names: impl IntoIterator<Item = String>) {
    row(out, names.into_iter().map(Text));
}

/// A timestamp as RFC 3339 in UTC, to the second: loadbearer writes as many
/// fractional digits as the clock gave, which no spreadsheet reads as a time.
/// Empty for one that does not parse, rather than passed on as it came.
fn utc(at: &str) -> Cell {
    let Ok(t) = OffsetDateTime::parse(at, &Rfc3339) else {
        return text("");
    };
    let t = t.to_offset(UtcOffset::UTC);
    let t = t.replace_nanosecond(0).unwrap_or(t);
    text(t.format(&Rfc3339).unwrap_or_default())
}

fn severity(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn queue(k: FlagKind) -> &'static str {
    match k {
        FlagKind::Machine => "machine",
        FlagKind::Measurement => "measurement",
        FlagKind::Coverage => "coverage",
    }
}

/// Every tag key the machines in view carry, sorted: one column each, so a
/// spreadsheet can sort and filter by site as the Machines tab does.
fn tag_keys(snap: &Snapshot) -> Vec<&str> {
    snap.machines
        .iter()
        .flat_map(|m| m.tags.keys().map(String::as_str))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `tag:site`: a fixed column's name never holds a colon, so a tag key —
/// whatever the deployment tool chose — cannot collide with one.
fn tag_header(key: &str) -> String {
    format!("tag:{key}")
}

fn tag_cells<'a>(m: Option<&'a MachineView>, keys: &'a [&str]) -> impl Iterator<Item = Cell> + 'a {
    keys.iter()
        .map(move |k| opt(m.and_then(|m| m.tags.get(*k)).map(String::as_str)))
}

/// The fixed columns of the machines table, in order. Named as the snapshot
/// names the field wherever there is one, so the file and `/api/snapshot`
/// can be read against each other.
pub const MACHINE_COLUMNS: [&str; 34] = [
    "hostname",
    "machine_key",
    "key_kind",
    "serial",
    "asset_tag",
    "system_model",
    "cpu_model",
    "cpu_cores",
    "ram_bytes",
    "os",
    "arch",
    "grade",
    "score",
    "cohort",
    "cohort_delta_pct",
    "trend_pct",
    "lifecycle",
    "lifecycle_reason",
    "lifecycle_remeasure_first",
    "age_estimate_months",
    "age_estimate_basis",
    "live_critical",
    "live_warning",
    "taken_at",
    "age_days",
    "runs",
    "bios_version",
    "bios_date",
    "worst_drive_wear_pct",
    "drive_power_on_hours",
    "disk_low_mount",
    "disk_low_free_pct",
    "disk_low_free_bytes",
    "overridden_by",
];

/// The machines in view, one row each, by hostname.
pub fn machines(snap: &Snapshot) -> String {
    let keys = tag_keys(snap);
    let mut out = String::new();
    header(
        &mut out,
        MACHINE_COLUMNS
            .iter()
            .map(|c| (*c).to_string())
            .chain(keys.iter().map(|k| tag_header(k))),
    );
    let mut live: std::collections::HashMap<&str, (i64, i64)> = Default::default();
    for f in snap.flags.iter().filter(|f| f.is_live()) {
        let c = live.entry(f.machine_key.as_str()).or_default();
        match f.severity {
            Severity::Critical => c.0 += 1,
            Severity::Warning => c.1 += 1,
            Severity::Info => {}
        }
    }
    let mut ordered: Vec<&MachineView> = snap.machines.iter().collect();
    // By name, as a reader looks a machine up; the key breaks a tie, so two
    // exports of the same view are the same file.
    ordered.sort_by(|a, b| {
        let name = |m: &MachineView| m.hostname.as_deref().unwrap_or(&m.key).to_lowercase();
        name(a).cmp(&name(b)).then_with(|| a.key.cmp(&b.key))
    });
    for m in ordered {
        let (critical, warning) = live.get(m.key.as_str()).copied().unwrap_or_default();
        let l = &m.lifecycle;
        let cells = [
            opt(m.hostname.as_deref()),
            text(&m.key),
            text(&m.key_kind),
            opt(m.serial.as_deref()),
            opt(m.asset_tag.as_deref()),
            opt(m.system_model.as_deref()),
            text(&m.cpu_model),
            Int(Some(m.cpu_cores)),
            Int(Some(m.ram_bytes)),
            opt(m.os.as_deref()),
            text(&m.arch),
            text(&m.grade),
            Num(m.score),
            text(&m.cohort),
            Num(m.cohort_delta_pct),
            Num(m.trend_pct),
            text(l.verdict.as_str()),
            opt(l.reasons.first().map(String::as_str)),
            text(if l.uncertain { "true" } else { "false" }),
            Int(m.age_estimate.map(|a| a.months)),
            opt(m.age_estimate.map(|a| match a.basis {
                crate::analytics::AgeBasis::UpperBound => "upper_bound",
                crate::analytics::AgeBasis::Firmware => "firmware",
            })),
            Int(Some(critical)),
            Int(Some(warning)),
            utc(&m.taken_at),
            Num(m.age_days),
            Int(Some(m.runs as i64)),
            opt(m.bios_version.as_deref()),
            opt(m.bios_date.as_deref()),
            Int(m.worst_drive_wear_pct),
            Int(m.drive_power_on_hours),
            opt(m.disk_low_mount.as_deref()),
            Num(m.disk_low_free_pct),
            Int(m.disk_low_free_bytes),
            text(
                m.overridden_by
                    .iter()
                    .map(|o| o.name.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        ];
        row(&mut out, cells.into_iter().chain(tag_cells(Some(m), &keys)));
    }
    out
}

/// The fixed columns of the findings table, in order.
pub const FINDING_COLUMNS: [&str; 15] = [
    "severity",
    "queue",
    "code",
    "hostname",
    "machine_key",
    "headline",
    "detail",
    "state",
    "triage_by",
    "triage_at",
    "triage_until",
    "triage_note",
    "suppression_rule",
    "suppression_reach",
    "severity_when_quieted",
];

/// Every finding on the machines in view — informational and quieted ones
/// included, each with its state — in the snapshot's order, most severe
/// first. A spreadsheet can filter a row out; a file that left one out
/// cannot be filtered back in.
pub fn findings(snap: &Snapshot) -> String {
    let keys = tag_keys(snap);
    let by_key: std::collections::HashMap<&str, &MachineView> =
        snap.machines.iter().map(|m| (m.key.as_str(), m)).collect();
    let mut out = String::new();
    header(
        &mut out,
        FINDING_COLUMNS
            .iter()
            .map(|c| (*c).to_string())
            .chain(keys.iter().map(|k| tag_header(k))),
    );
    for f in &snap.flags {
        let t = f.triage.as_ref();
        let cells = [
            text(severity(f.severity)),
            text(queue(f.kind)),
            text(f.code),
            opt(f.hostname.as_deref()),
            text(&f.machine_key),
            text(&f.headline),
            text(&f.detail),
            text(t.map_or("live", |t| t.state.as_str())),
            opt(t.map(|t| t.by.as_str())),
            t.map_or_else(|| text(""), |t| utc(&t.at)),
            opt(t.and_then(|t| t.until.as_deref())),
            opt(t.and_then(|t| t.note.as_deref())),
            Int(t.and_then(|t| t.rule)),
            opt(t.and_then(|t| t.reach).map(|r| match r {
                crate::state::Reach::Machine => "machine",
                crate::state::Reach::Tags => "tags",
                crate::state::Reach::Fleet => "fleet",
            })),
            opt(t.and_then(|t| t.severity).map(severity)),
        ];
        let m = by_key.get(f.machine_key.as_str()).copied();
        row(&mut out, cells.into_iter().chain(tag_cells(m, &keys)));
    }
    out
}

/// Reads RFC 4180 back into rows of fields, strictly: a row must end CRLF,
/// and a bare LF outside quotes is an error rather than a line end. For the
/// tests, here and in `web`, which read what was written rather than trust it.
#[cfg(test)]
pub(crate) fn parse(csv: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = csv.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                c => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {
                assert_eq!(
                    chars.next(),
                    Some('\n'),
                    "a CR outside quotes not before LF"
                );
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\n' => panic!("a bare LF ends a row: {csv:?}"),
            c => field.push(c),
        }
    }
    assert!(
        field.is_empty() && row.is_empty(),
        "the last row does not end CRLF"
    );
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(cells: Vec<Cell>) -> String {
        let mut out = String::new();
        row(&mut out, cells);
        out
    }

    /// RFC 4180: a comma, a quote, a CR or an LF puts the field in quotes, a
    /// quote inside is doubled, and every row ends CRLF — not the platform's
    /// line end, which on Linux would be LF alone.
    #[test]
    fn fields_are_quoted_as_rfc_4180_says() {
        assert_eq!(one(vec![text("plain"), text("")]), "plain,\r\n");
        assert_eq!(one(vec![text("a,b")]), "\"a,b\"\r\n");
        assert_eq!(
            one(vec![text("the \"fast\" one")]),
            "\"the \"\"fast\"\" one\"\r\n"
        );
        assert_eq!(
            one(vec![text("line one\nline two")]),
            "\"line one\nline two\"\r\n"
        );
        assert_eq!(one(vec![text("cr\rhere")]), "\"cr\rhere\"\r\n");
        assert_eq!(one(vec![text("crlf\r\n")]), "\"crlf\r\n\"\r\n");
        // Nothing else is quoted: a space, a semicolon and a non-ASCII name
        // pass as they are.
        assert_eq!(one(vec![text("Zoë's PC; desk 4")]), "Zoë's PC; desk 4\r\n");
    }

    /// What is written reads back as what was meant, field for field.
    #[test]
    fn every_awkward_field_reads_back_as_itself() {
        let awkward = ["a,b", "\"", "x\ny", "x\r\ny", "", "plain", "\"\",\""];
        let written = one(awkward.iter().map(|s| text(*s)).collect());
        assert_eq!(parse(&written), vec![awkward.to_vec()]);
    }

    /// A text cell that a spreadsheet would run is defused with `'`; one that
    /// merely contains those characters further in is left alone, and a
    /// number — negative or not — is never touched.
    #[test]
    fn a_text_cell_cannot_be_a_formula_and_a_number_is_left_alone() {
        for lead in ["=1+1", "+44 131", "-2+3", "@SUM(A1)", "\tx", "\rx"] {
            let got = one(vec![text(lead)]);
            assert!(
                got.starts_with("'") || got.starts_with("\"'"),
                "{lead:?} became {got:?}"
            );
        }
        assert_eq!(
            one(vec![text("=HYPERLINK(\"http://x\",\"y\")")]),
            "\"'=HYPERLINK(\"\"http://x\"\",\"\"y\"\")\"\r\n",
            "defused first, then quoted"
        );
        assert_eq!(one(vec![text("a=b")]), "a=b\r\n");
        assert_eq!(one(vec![text("PC-01")]), "PC-01\r\n");
        assert_eq!(
            one(vec![Num(Some(-12.5)), Num(Some(-0.25)), Int(Some(-3))]),
            "-12.5,-0.25,-3\r\n"
        );
    }

    /// Plain decimals: no exponent, no separator, a `.` point; empty for a
    /// missing figure or one that is not a number at all.
    #[test]
    fn numbers_are_plain_and_missing_is_empty() {
        assert_eq!(
            one(vec![
                Num(Some(1234567.0)),
                Num(Some(0.1)),
                Num(Some(1e21)),
                Num(None),
                Num(Some(f64::NAN)),
                Int(None),
                Int(Some(16_000_000_000)),
            ]),
            "1234567,0.1,1000000000000000000000,,,,16000000000\r\n"
        );
    }

    #[test]
    fn times_are_utc_to_the_second_and_unreadable_ones_are_empty() {
        let cell = |s: &str| match utc(s) {
            Text(t) => t,
            _ => unreachable!(),
        };
        assert_eq!(
            cell("2026-09-09T16:59:48.365852386Z"),
            "2026-09-09T16:59:48Z"
        );
        assert_eq!(cell("2026-09-09T18:00:00+02:00"), "2026-09-09T16:00:00Z");
        assert_eq!(cell("last Tuesday"), "");
    }

    #[test]
    fn a_table_is_named_for_what_it_is_and_the_day() {
        let at = OffsetDateTime::parse("2026-11-13T23:30:00-02:00", &Rfc3339).unwrap();
        assert_eq!(
            Table::Machines.filename(at),
            "loadbearer-fleet-machines-2026-11-14.csv",
            "the day in UTC"
        );
        assert_eq!("findings".parse::<Table>(), Ok(Table::Findings));
        assert!("heatmap".parse::<Table>().is_err());
    }

    /// No fixed column may look like a tag column, or two would share a name.
    #[test]
    fn no_fixed_column_can_collide_with_a_tag_column() {
        for c in MACHINE_COLUMNS.iter().chain(FINDING_COLUMNS.iter()) {
            assert!(!c.contains(':'), "{c}");
        }
    }
}
