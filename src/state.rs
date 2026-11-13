//! Operator state: what the people running the estate have said about its
//! findings — acknowledged, snoozed, suppressed.
//!
//! ## The one thing that is not derived
//!
//! Everything else this process holds can be rebuilt: the index from the
//! collection folder, the analysis from the index, the timeline from the runs.
//! A finding someone has acknowledged cannot. Nothing in a result document
//! says "Iain looked at this on Tuesday", so if this were kept in the index it
//! would be lost by exactly the rebuild the index's own documentation calls
//! safe — and the promise that the index can be deleted would quietly stop
//! being true.
//!
//! So it lives in a file of its own, the **state file**, beside the index by
//! default. It is small, it changes only when somebody presses a button, and
//! it is the one file besides the collection folder that has to be backed up.
//! `backup` writes a copy of it next to the index snapshot for that reason.
//!
//! ## What it refuses to do
//!
//! The index, on a format it does not understand, is renamed and rebuilt:
//! it is derived, and losing it costs a rescan. This file is not derived, so
//! on a version it does not understand it **refuses to open** rather than
//! setting it aside — a rebuilt state file is an empty one, and quietly
//! un-acknowledging every finding in the estate is worse than not starting.
//! It is marked with its own `application_id` too, so pointing `[server]
//! state` at the index (or anything else) is refused rather than written into.
//!
//! A single file in SQLite's default rollback-journal mode, not WAL: writes
//! are a click at a time, and one file with no `-wal` beside it is the easier
//! thing to back up, copy and reason about.
//!
//! ## Attention, not rules
//!
//! Triage never changes a finding. Every finding is still computed, still in
//! the snapshot, still part of the lifecycle verdict and of the history; what
//! triage changes is whether it counts as *needing attention* and whether the
//! queues show it by default. It is applied where the snapshot is projected for
//! a caller (`Snapshot::filtered_with`), against the clock at that moment, so a
//! snooze ends by itself without a rescan.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

use crate::analytics::{Severity, Snapshot, scopes_allow};

/// Bumped when the tables here change shape. Unlike the index there is no
/// rebuild to fall back on, so a change has to come with a migration.
const STATE_VERSION: i64 = 1;

/// `PRAGMA application_id`, so this file is recognisably this file. "LBFS".
const APPLICATION_ID: i64 = 0x4C42_4653;

/// The longest note kept with a triage action, in characters. A note is a
/// sentence of context for the next person, not a ticket.
pub const MAX_NOTE_CHARS: usize = 500;

/// How far ahead a snooze may run. A year is the outside of "not now"; past
/// that it is a suppression and should say so.
pub const MAX_SNOOZE_DAYS: i64 = 365;

/// What has been said about a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TriageState {
    Acknowledged,
    Snoozed,
    Suppressed,
}

impl TriageState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Acknowledged => "acknowledged",
            Self::Snoozed => "snoozed",
            Self::Suppressed => "suppressed",
        }
    }
}

/// How a suppression picks its machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    Machine,
    Tags,
    Fleet,
}

/// A finding's triage, as it rides on the finding in every API response.
/// `None` on the finding means it is live.
#[derive(Debug, Clone, Serialize)]
pub struct Triage {
    pub state: TriageState,
    /// Who said so — the display name of the signed-in caller.
    pub by: String,
    /// When, RFC 3339.
    pub at: String,
    pub note: Option<String>,
    /// The day, UTC, a snooze ends: the finding is back from the start of it.
    /// Snoozes only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// The severity the finding had when it was acknowledged or snoozed. It
    /// lapses if the finding becomes more severe than this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// The suppression rule's id, and what it reaches. Suppressions only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach: Option<Reach>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<BTreeMap<String, String>>,
}

/// An acknowledgement or a snooze: one finding on one machine.
///
/// Keyed on `(machine_key, code)`, which is what "the same finding" means
/// everywhere in this tool (see `Timeline::changes`). One record per key, so
/// snoozing an acknowledged finding replaces the acknowledgement: the latest
/// thing anybody said about it is what holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub machine_key: String,
    pub code: String,
    pub state: TriageState,
    /// The worst severity the finding had when this was recorded.
    pub severity: Severity,
    /// Snoozes only: the day, UTC, it ends.
    pub until: Option<Date>,
    pub note: Option<String>,
    pub by: String,
    pub by_subject: String,
    pub at: OffsetDateTime,
}

/// A standing rule: this finding, on these machines, is not news.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suppression {
    pub id: i64,
    pub code: String,
    /// Exactly one of these two, or neither for the whole fleet.
    pub machine_key: Option<String>,
    pub tags: Option<BTreeMap<String, String>>,
    pub note: Option<String>,
    pub by: String,
    #[serde(skip)]
    pub by_subject: String,
    /// When, RFC 3339.
    pub at: String,
}

impl Suppression {
    pub fn reach(&self) -> Reach {
        match (&self.machine_key, &self.tags) {
            (Some(_), _) => Reach::Machine,
            (None, Some(_)) => Reach::Tags,
            (None, None) => Reach::Fleet,
        }
    }

    /// Whether this rule reaches a machine with this key and these tags,
    /// regardless of the finding.
    pub fn reaches(&self, machine_key: &str, tags: &BTreeMap<String, String>) -> bool {
        match (&self.machine_key, &self.tags) {
            (Some(k), _) => k == machine_key,
            // The same matching a grant's tags use — one rule for "these
            // tags pick this machine", so a scope and a suppression can't
            // come to disagree about it. An empty set is refused when the
            // rule is made; it would otherwise mean the whole fleet here.
            (None, Some(t)) => !t.is_empty() && scopes_allow(std::slice::from_ref(t), tags),
            (None, None) => true,
        }
    }

    /// Most specific first: one machine over a tag rule over the fleet, then
    /// the oldest. Only decides which rule a finding names, never whether it
    /// is suppressed.
    fn specificity(&self) -> (u8, i64) {
        let rank = match self.reach() {
            Reach::Machine => 0,
            Reach::Tags => 1,
            Reach::Fleet => 2,
        };
        (rank, self.id)
    }
}

/// Why a mark stopped applying, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spent {
    /// The finding was no longer there: what was acknowledged is resolved, and
    /// a recurrence is news.
    Resolved,
    /// The finding got worse than it was when somebody said they had seen it.
    Escalated,
    /// A snooze that has run out.
    Expired,
}

impl Spent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Escalated => "escalated",
            Self::Expired => "expired",
        }
    }
}

/// Everything in the state file, in memory, so applying it to a view is a
/// lookup rather than a query.
#[derive(Debug, Clone, Default)]
pub struct Book {
    marks: HashMap<(String, String), Mark>,
    suppressions: Vec<Suppression>,
}

impl Book {
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty() && self.suppressions.is_empty()
    }

    pub fn suppressions(&self) -> &[Suppression] {
        &self.suppressions
    }

    #[cfg(test)]
    pub fn mark(&self, machine_key: &str, code: &str) -> Option<&Mark> {
        self.marks.get(&(machine_key.to_string(), code.to_string()))
    }

    /// What applies to one finding right now, or `None` if it is live.
    ///
    /// `worst` is the worst severity the rule currently raises on this
    /// machine. Usually there is one finding per rule per machine; the
    /// exception is `component_weak`, raised once per weak component, and
    /// those are triaged together, so it is their worst that an
    /// acknowledgement is compared with.
    ///
    /// Precedence is suppress, then snooze, then acknowledge — and the last
    /// two cannot both hold, since one record per finding keeps only the
    /// latest. A suppression never lapses; an acknowledgement or a snooze
    /// lapses the moment the finding is more severe than when it was recorded,
    /// and a snooze ends at the start of `until`, UTC.
    pub fn triage_for(
        &self,
        machine_key: &str,
        code: &str,
        worst: Severity,
        tags: &BTreeMap<String, String>,
        today: Date,
    ) -> Option<Triage> {
        if let Some(rule) = self
            .suppressions
            .iter()
            .filter(|s| s.code == code && s.reaches(machine_key, tags))
            .min_by_key(|s| s.specificity())
        {
            return Some(Triage {
                state: TriageState::Suppressed,
                by: rule.by.clone(),
                at: rule.at.clone(),
                note: rule.note.clone(),
                until: None,
                severity: None,
                rule: Some(rule.id),
                reach: Some(rule.reach()),
                tags: rule.tags.clone(),
            });
        }
        let mark = self
            .marks
            .get(&(machine_key.to_string(), code.to_string()))?;
        // `Severity` orders most severe first, so "worse than recorded" is
        // "less than".
        if worst < mark.severity {
            return None;
        }
        if mark.state == TriageState::Snoozed && mark.until.is_none_or(|u| today >= u) {
            return None;
        }
        Some(Triage {
            state: mark.state,
            by: mark.by.clone(),
            at: rfc3339(mark.at),
            note: mark.note.clone(),
            until: mark.until.map(|d| d.to_string()),
            severity: Some(mark.severity),
            rule: None,
            reach: None,
            tags: None,
        })
    }

    /// The marks that no longer apply to the fleet as `snap` sees it, and why.
    ///
    /// **This is what makes an acknowledgement end.** It is keyed on the
    /// finding rather than on any one occurrence of it, so without this a
    /// battery acknowledged in March, replaced in April and worn again in
    /// November would come back already acknowledged. Instead, the first time
    /// the full, unscoped snapshot is computed without the finding on a
    /// machine that *is* there, the mark is spent and dropped.
    ///
    /// A machine that is absent altogether keeps its marks. That is the
    /// transient case: an index superseded by an upgrade is empty until its
    /// first scan, and a share that is briefly unreachable is not a fleet
    /// that has been fixed. A machine that is really gone is removed with
    /// `forget`, which clears its marks itself.
    ///
    /// Judged only when the snapshot is rebuilt — after a scan, an upload or
    /// a change by another process — so a finding resolved and recurring
    /// between two rebuilds was never seen resolved, and its mark stands.
    pub fn spent(&self, snap: &Snapshot, today: Date) -> Vec<(String, String, Spent)> {
        let present: std::collections::HashSet<&str> =
            snap.machines.iter().map(|m| m.key.as_str()).collect();
        let worst = worst_by_finding(snap);
        let mut out = Vec::new();
        for ((key, code), mark) in &self.marks {
            if !present.contains(key.as_str()) {
                continue;
            }
            let why = match worst.get(&(key.as_str(), code.as_str())) {
                None => Some(Spent::Resolved),
                Some(w) if *w < mark.severity => Some(Spent::Escalated),
                _ if mark.state == TriageState::Snoozed
                    && mark.until.is_none_or(|u| today >= u) =>
                {
                    Some(Spent::Expired)
                }
                _ => None,
            };
            if let Some(why) = why {
                out.push((key.clone(), code.clone(), why));
            }
        }
        out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        out
    }

    /// Drop marks from the in-memory copy only. `report` uses this so that
    /// what it prints agrees with what a running server would have done,
    /// without writing to a file it only reads.
    pub fn without(&self, spent: &[(String, String, Spent)]) -> Book {
        let mut out = self.clone();
        for (key, code, _) in spent {
            out.marks.remove(&(key.clone(), code.clone()));
        }
        out
    }
}

/// The worst severity each rule raises on each machine.
pub fn worst_by_finding(snap: &Snapshot) -> HashMap<(&str, &str), Severity> {
    let mut worst: HashMap<(&str, &str), Severity> = HashMap::new();
    for f in &snap.flags {
        worst
            .entry((f.machine_key.as_str(), f.code))
            .and_modify(|s| *s = (*s).min(f.severity))
            .or_insert(f.severity);
    }
    worst
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn parse_severity(s: &str) -> Result<Severity> {
    Ok(match s {
        "critical" => Severity::Critical,
        "warning" => Severity::Warning,
        "info" => Severity::Info,
        other => bail!("the state file holds a severity this build does not know: {other:?}"),
    })
}

/// A `YYYY-MM-DD`, as a snooze's `until` is written and read.
pub fn parse_date(s: &str) -> Option<Date> {
    Date::parse(
        s.trim(),
        time::macros::format_description!("[year]-[month]-[day]"),
    )
    .ok()
}

/// What `forget` removes from the state file for one machine.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ForgetTriage {
    pub marks: usize,
    pub suppressions: usize,
}

/// The state file.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open the state file, creating it if there is none. For `serve`, which
    /// is the only thing that writes to it in the ordinary course.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening the state file at {}", path.display()))?;
        Self::init(conn, path, true)
    }

    /// Open a state file that may not exist, for a command that changes it
    /// only if there is something to change (`forget`). `None` when there is
    /// no file: nothing has ever been triaged, and creating one now would put
    /// an empty file somewhere nobody asked for one.
    pub fn open_existing(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening the state file at {}", path.display()))?;
        Self::init(conn, path, true).map(Some)
    }

    /// The same, read-only, for `report` and `backup`, which never write it.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening the state file at {}", path.display()))?;
        Self::init(conn, path, false).map(Some)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, Path::new(":memory:"), true)
    }

    fn init(conn: Connection, path: &Path, writable: bool) -> Result<Self> {
        // The server holds this file open while `forget` or `backup` run from
        // a terminal; a write is a single row, so a short wait beats an error.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let app: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;

        if app == 0 && version == 0 && tables == 0 {
            // A file nothing has been written to — just created, or empty.
            if writable {
                conn.execute_batch(&format!(
                    "BEGIN;
                     CREATE TABLE mark (
                         machine_key TEXT NOT NULL,
                         code        TEXT NOT NULL,
                         state       TEXT NOT NULL CHECK (state IN ('acknowledged', 'snoozed')),
                         severity    TEXT NOT NULL,
                         until       TEXT,
                         note        TEXT,
                         by_name     TEXT NOT NULL,
                         by_subject  TEXT NOT NULL,
                         at          TEXT NOT NULL,
                         PRIMARY KEY (machine_key, code)
                     );
                     CREATE TABLE suppression (
                         id          INTEGER PRIMARY KEY,
                         code        TEXT NOT NULL,
                         machine_key TEXT,
                         -- A JSON object of tag key to value, for a tag rule.
                         tags        TEXT,
                         note        TEXT,
                         by_name     TEXT NOT NULL,
                         by_subject  TEXT NOT NULL,
                         at          TEXT NOT NULL,
                         CHECK (machine_key IS NULL OR tags IS NULL)
                     );
                     PRAGMA application_id = {APPLICATION_ID};
                     PRAGMA user_version = {STATE_VERSION};
                     COMMIT;"
                ))
                .with_context(|| format!("creating the state file at {}", path.display()))?;
            }
            return Ok(Self { conn });
        }
        if app != APPLICATION_ID {
            bail!(
                "{} is not a loadbearer-fleet state file (it may be the index, or another \
                 program's database). [server] state has to name a file of its own — it holds \
                 the acknowledgements, snoozes and suppressions people have recorded, and this \
                 will not write them into something else.",
                path.display()
            );
        }
        if version != STATE_VERSION {
            bail!(
                "the state file at {} is version {version}, and this build understands version \
                 {STATE_VERSION}. It holds what people have recorded about findings, which \
                 cannot be rebuilt from anything, so it is neither set aside nor recreated: run \
                 the release that wrote it, or restore the copy `backup` took before the upgrade.",
                path.display()
            );
        }
        Ok(Self { conn })
    }

    /// Everything, for holding in memory.
    pub fn load(&self) -> Result<Book> {
        let mut book = Book::default();
        if !self.has_tables()? {
            return Ok(book);
        }
        let mut stmt = self.conn.prepare(
            "SELECT machine_key, code, state, severity, until, note, by_name, by_subject, at
               FROM mark",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (machine_key, code, state, severity, until, note, by, by_subject, at) in rows {
            let state = match state.as_str() {
                "acknowledged" => TriageState::Acknowledged,
                "snoozed" => TriageState::Snoozed,
                other => bail!("the state file holds a mark in state {other:?}"),
            };
            let mark = Mark {
                state,
                severity: parse_severity(&severity)?,
                until: until.as_deref().and_then(parse_date),
                note,
                by,
                by_subject,
                at: OffsetDateTime::parse(&at, &Rfc3339)
                    .with_context(|| format!("an unreadable time in the state file: {at:?}"))?,
                machine_key: machine_key.clone(),
                code: code.clone(),
            };
            book.marks.insert((machine_key, code), mark);
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, code, machine_key, tags, note, by_name, by_subject, at
               FROM suppression ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, code, machine_key, tags, note, by, by_subject, at) in rows {
            book.suppressions.push(Suppression {
                id,
                code,
                machine_key,
                tags: tags
                    .map(|t| serde_json::from_str(&t))
                    .transpose()
                    .context("unreadable tags on a suppression in the state file")?,
                note,
                by,
                by_subject,
                at,
            });
        }
        Ok(book)
    }

    /// A read-only handle on a file that was empty when opened has no tables.
    fn has_tables(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'mark'",
            [],
            |r| r.get::<_, i64>(0),
        )? > 0)
    }

    /// Record an acknowledgement or a snooze, replacing whatever was said
    /// about this finding before.
    pub fn put_mark(&self, m: &Mark) -> Result<()> {
        self.conn.execute(
            "INSERT INTO mark (machine_key, code, state, severity, until, note, by_name, by_subject, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (machine_key, code) DO UPDATE SET
                 state = excluded.state, severity = excluded.severity, until = excluded.until,
                 note = excluded.note, by_name = excluded.by_name,
                 by_subject = excluded.by_subject, at = excluded.at",
            params![
                m.machine_key,
                m.code,
                m.state.as_str(),
                severity_str(m.severity),
                m.until.map(|d| d.to_string()),
                m.note,
                m.by,
                m.by_subject,
                rfc3339(m.at),
            ],
        )?;
        Ok(())
    }

    /// Remove an acknowledgement or snooze. Whether there was one.
    pub fn clear_mark(&self, machine_key: &str, code: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM mark WHERE machine_key = ?1 AND code = ?2",
            params![machine_key, code],
        )? > 0)
    }

    /// Drop marks that have stopped applying. See `Book::spent`.
    pub fn spend(&self, spent: &[(String, String, Spent)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (key, code, _) in spent {
            tx.execute(
                "DELETE FROM mark WHERE machine_key = ?1 AND code = ?2",
                params![key, code],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Add a suppression and hand it back with the id it was given. The `id`
    /// of the one passed in is ignored.
    pub fn add_suppression(&self, s: &Suppression) -> Result<Suppression> {
        self.conn.execute(
            "INSERT INTO suppression (code, machine_key, tags, note, by_name, by_subject, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                s.code,
                s.machine_key,
                s.tags.as_ref().map(serde_json::to_string).transpose()?,
                s.note,
                s.by,
                s.by_subject,
                s.at,
            ],
        )?;
        Ok(Suppression {
            id: self.conn.last_insert_rowid(),
            ..s.clone()
        })
    }

    /// Remove a suppression. Whether there was one.
    pub fn delete_suppression(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM suppression WHERE id = ?1", [id])?
            > 0)
    }

    /// What `forget` would remove for one machine: its marks, and the
    /// suppressions that name it. Tag and fleet-wide rules are about more than
    /// one machine and stay.
    pub fn forget_plan(&self, machine_key: &str) -> Result<ForgetTriage> {
        if !self.has_tables()? {
            return Ok(ForgetTriage::default());
        }
        let count = |sql: &str| -> Result<usize> {
            Ok(self
                .conn
                .query_row(sql, [machine_key], |r| r.get::<_, i64>(0))? as usize)
        };
        Ok(ForgetTriage {
            marks: count("SELECT COUNT(*) FROM mark WHERE machine_key = ?1")?,
            suppressions: count("SELECT COUNT(*) FROM suppression WHERE machine_key = ?1")?,
        })
    }

    pub fn forget(&self, machine_key: &str) -> Result<ForgetTriage> {
        let plan = self.forget_plan(machine_key)?;
        if plan == ForgetTriage::default() {
            return Ok(plan);
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM mark WHERE machine_key = ?1", [machine_key])?;
        tx.execute(
            "DELETE FROM suppression WHERE machine_key = ?1",
            [machine_key],
        )?;
        tx.commit()?;
        Ok(plan)
    }

    /// A consistent copy, safe to take while the server is writing: the same
    /// `VACUUM INTO` the index backup uses, for the same reason.
    pub fn backup_to(&self, target: &Path) -> Result<u64> {
        if target.exists() {
            bail!(
                "{} already exists; move it aside or pick another name",
                target.display()
            );
        }
        self.conn
            .execute("VACUUM INTO ?1", params![target.to_string_lossy()])
            .with_context(|| format!("writing a copy of the state file to {}", target.display()))?;
        Ok(std::fs::metadata(target)?.len())
    }

    /// Changes whenever another connection commits — `forget` from a
    /// terminal, most obviously — so the server can reload its copy. See
    /// `Index::data_version` for why this pragma is the right question.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    /// How many records there are, for `backup` to say what it copied.
    pub fn counts(&self) -> Result<(usize, usize)> {
        if !self.has_tables()? {
            return Ok((0, 0));
        }
        let marks: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM mark", [], |r| r.get(0))?;
        let rules: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM suppression", [], |r| r.get(0))?;
        Ok((marks as usize, rules as usize))
    }
}

/// Where the state file's copy goes, beside an index backup: the backup's
/// name with `.state` before its extension, the same rule the default state
/// path follows beside the index — `fleet-2026-09-10.db` and
/// `fleet-2026-09-10.state.db` restore to `fleet-index.db` and
/// `fleet-index.state.db`.
pub fn sibling(path: &Path) -> std::path::PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.state.{}", ext.to_string_lossy()),
        None => format!("{stem}.state"),
    };
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lbf-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn mark(key: &str, code: &str, state: TriageState) -> Mark {
        Mark {
            machine_key: key.to_string(),
            code: code.to_string(),
            state,
            severity: Severity::Warning,
            until: (state == TriageState::Snoozed).then_some(date!(2026 - 10 - 01)),
            note: Some("in hand".to_string()),
            by: "Iain".to_string(),
            by_subject: "sub-1".to_string(),
            at: datetime!(2026-09-09 12:00:00 UTC),
        }
    }

    pub(crate) fn rule(
        code: &str,
        machine_key: Option<&str>,
        tags: Option<BTreeMap<String, String>>,
    ) -> Suppression {
        Suppression {
            id: 0,
            code: code.to_string(),
            machine_key: machine_key.map(str::to_string),
            tags,
            note: None,
            by: "Iain".to_string(),
            by_subject: "sub-1".to_string(),
            at: "2026-09-09T12:00:00Z".to_string(),
        }
    }

    /// The whole point of a separate file: what was recorded is still there
    /// after the process that recorded it has gone.
    #[test]
    fn what_is_recorded_survives_reopening_the_file() {
        let dir = scratch("reopen");
        let path = dir.join("fleet-index.state.db");
        {
            let store = Store::open(&path).expect("created");
            store
                .put_mark(&mark("SN-1", "battery_worn", TriageState::Acknowledged))
                .expect("mark");
            store
                .put_mark(&mark("SN-2", "stale", TriageState::Snoozed))
                .expect("mark");
            let mut tags = BTreeMap::new();
            tags.insert("site".to_string(), "glasgow".to_string());
            store
                .add_suppression(&rule("unstable_run", None, Some(tags)))
                .expect("rule");
        }
        let store = Store::open(&path).expect("reopened");
        let book = store.load().expect("load");
        assert_eq!(
            book.mark("SN-1", "battery_worn").map(|m| m.state),
            Some(TriageState::Acknowledged)
        );
        let snoozed = book.mark("SN-2", "stale").expect("snooze");
        assert_eq!(snoozed.until, Some(date!(2026 - 10 - 01)));
        assert_eq!(snoozed.note.as_deref(), Some("in hand"));
        assert_eq!(book.suppressions().len(), 1);
        assert_eq!(book.suppressions()[0].reach(), Reach::Tags);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Operator state cannot be rebuilt, so a version this build does not
    /// know is a refusal — never a rename, and never an empty replacement.
    #[test]
    fn an_unknown_version_refuses_rather_than_starting_afresh() {
        let dir = scratch("version");
        let path = dir.join("s.db");
        Store::open(&path)
            .expect("created")
            .put_mark(&mark("SN-1", "stale", TriageState::Acknowledged))
            .expect("mark");
        {
            let conn = Connection::open(&path).expect("raw");
            conn.pragma_update(None, "user_version", 99_i64).unwrap();
        }
        let err = Store::open(&path).err().expect("refused").to_string();
        assert!(err.contains("version 99"), "{err}");
        assert!(
            Store::open_read_only(&path).is_err(),
            "read-only too: report must not print a view built on a file it cannot read"
        );
        // And the file is untouched, so the release that wrote it still can.
        let conn = Connection::open(&path).expect("raw");
        let marks: i64 = conn
            .query_row("SELECT COUNT(*) FROM mark", [], |r| r.get(0))
            .unwrap();
        assert_eq!(marks, 1);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[server] state` pointed at the index by mistake must not write
    /// triage tables into it.
    #[test]
    fn a_database_that_is_not_a_state_file_is_refused() {
        let dir = scratch("foreign");
        let path = dir.join("i.db");
        drop(crate::index::Index::open(&path).expect("an index"));
        let err = Store::open(&path).err().expect("refused").to_string();
        assert!(err.contains("not a loadbearer-fleet state file"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_not_created_by_the_commands_that_only_read_it() {
        let dir = scratch("missing");
        let path = dir.join("s.db");
        assert!(Store::open_read_only(&path).expect("fine").is_none());
        assert!(Store::open_existing(&path).expect("fine").is_none());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_a_machine_takes_its_marks_and_its_own_rules_only() {
        let store = Store::open_in_memory().expect("store");
        store
            .put_mark(&mark("SN-1", "stale", TriageState::Acknowledged))
            .unwrap();
        store
            .put_mark(&mark("SN-1", "battery_worn", TriageState::Snoozed))
            .unwrap();
        store
            .put_mark(&mark("SN-2", "stale", TriageState::Acknowledged))
            .unwrap();
        store
            .add_suppression(&rule("stale", Some("SN-1"), None))
            .unwrap();
        store.add_suppression(&rule("stale", None, None)).unwrap();

        assert_eq!(
            store.forget_plan("SN-1").unwrap(),
            ForgetTriage {
                marks: 2,
                suppressions: 1
            }
        );
        store.forget("SN-1").unwrap();
        let book = store.load().unwrap();
        assert!(book.mark("SN-1", "stale").is_none());
        assert!(book.mark("SN-2", "stale").is_some(), "another machine's");
        assert_eq!(
            book.suppressions().len(),
            1,
            "the fleet-wide rule is about more than one machine and stays"
        );
        assert_eq!(book.suppressions()[0].reach(), Reach::Fleet);
    }

    #[test]
    fn a_backup_copy_is_named_beside_the_index_backup() {
        assert_eq!(
            sibling(Path::new("backups/fleet-2026-09-10.db")),
            Path::new("backups/fleet-2026-09-10.state.db")
        );
        assert_eq!(
            sibling(Path::new("fleet-index.db")),
            Path::new("fleet-index.state.db")
        );
        assert_eq!(sibling(Path::new("snap")), Path::new("snap.state"));
    }

    #[test]
    fn the_backup_copy_is_a_state_file_with_everything_in_it() {
        let dir = scratch("backup");
        let live = Store::open(&dir.join("live.db")).expect("store");
        live.put_mark(&mark("SN-1", "stale", TriageState::Acknowledged))
            .unwrap();
        let copy = dir.join("copy.state.db");
        live.backup_to(&copy).expect("backed up");
        assert!(
            live.backup_to(&copy).is_err(),
            "never overwrites a previous copy"
        );
        let restored = Store::open_read_only(&copy)
            .expect("opens")
            .expect("exists");
        assert!(restored.load().unwrap().mark("SN-1", "stale").is_some());
        drop((live, restored));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
