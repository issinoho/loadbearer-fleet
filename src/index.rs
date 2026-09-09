//! The SQLite index over a folder of result files.
//!
//! The **folder is the source of truth**; this index is a derived, disposable
//! read model. Nothing is ever only in the database, so it can be deleted and
//! rebuilt from the same files, which is what makes schema changes here cheap
//! and means there is no data to migrate or back up separately.
//!
//! Ingest is **idempotent by content**. A collection share gets rescanned
//! constantly — on a timer, after a sweep, when someone hits refresh — and the
//! same file must not become two runs. Every row carries the SHA-256 of the
//! document it came from, so re-reading an unchanged file is a no-op and a file
//! that is *moved* or renamed doesn't duplicate its run either.
//!
//! History is kept: one row per run, not per machine. Trend, drift and
//! regression questions all need the series, and the whole point of a fleet
//! view is noticing that a machine got slower rather than that it is slow.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::schema::ResultFile;

/// Bumped when the derived tables change shape. Since the index is rebuildable
/// from the folder, a mismatch drops and rebuilds rather than migrating.
const INDEX_VERSION: i64 = 1;

pub struct Index {
    conn: Connection,
}

/// What one scan did, for logging and for the UI's "last refreshed" line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub seen: usize,
    pub ingested: usize,
    pub unchanged: usize,
    pub rejected: Vec<(String, String)>,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening index at {}", path.display()))?;
        Self::init(conn)
    }

    /// Test-only: a throwaway index that never touches disk.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL so a scan writing doesn't block the dashboard reading.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let found: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if found != 0 && found != INDEX_VERSION {
            tracing::info!(
                found,
                expected = INDEX_VERSION,
                "index built by another version; dropping and rebuilding from the folder"
            );
            conn.execute_batch(
                "DROP TABLE IF EXISTS subtest;
                 DROP TABLE IF EXISTS component;
                 DROP TABLE IF EXISTS tag;
                 DROP TABLE IF EXISTS run;",
            )?;
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run (
                 id             INTEGER PRIMARY KEY,
                 machine_key    TEXT NOT NULL,
                 key_kind       TEXT NOT NULL,
                 hostname       TEXT,
                 taken_at       TEXT NOT NULL,
                 tool_version   TEXT NOT NULL,
                 preset         TEXT NOT NULL,
                 profile        TEXT NOT NULL,
                 baseline       TEXT NOT NULL,
                 build_isa      TEXT NOT NULL,
                 overall_score  REAL,
                 overall_grade  TEXT,
                 cpu_model      TEXT NOT NULL,
                 cpu_cores      INTEGER NOT NULL,
                 ram_bytes      INTEGER NOT NULL,
                 os             TEXT,
                 arch           TEXT NOT NULL,
                 serial         TEXT,
                 asset_tag      TEXT,
                 on_ac          INTEGER,
                 thermal_limited INTEGER,
                 battery_health  REAL,
                 battery_cycles  INTEGER,
                 partial        INTEGER NOT NULL,
                 caveats        TEXT NOT NULL,
                 source_path    TEXT NOT NULL,
                 content_hash   TEXT NOT NULL UNIQUE,
                 ingested_at    TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS run_machine ON run(machine_key, taken_at DESC);
             CREATE INDEX IF NOT EXISTS run_taken   ON run(taken_at DESC);
             CREATE INDEX IF NOT EXISTS run_cpu     ON run(cpu_model);

             CREATE TABLE IF NOT EXISTS component (
                 run_id  INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
                 id      TEXT NOT NULL,
                 score   REAL NOT NULL,
                 grade   TEXT NOT NULL,
                 graded  INTEGER NOT NULL,
                 PRIMARY KEY (run_id, id)
             );

             CREATE TABLE IF NOT EXISTS subtest (
                 run_id     INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
                 component  TEXT NOT NULL,
                 id         TEXT NOT NULL,
                 value      REAL NOT NULL,
                 unit       TEXT NOT NULL,
                 score      REAL,
                 ratio      REAL,
                 cv         REAL,
                 confidence TEXT NOT NULL,
                 representative TEXT,
                 scored     INTEGER NOT NULL,
                 PRIMARY KEY (run_id, component, id)
             );

             CREATE TABLE IF NOT EXISTS tag (
                 run_id INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
                 key    TEXT NOT NULL,
                 value  TEXT NOT NULL,
                 PRIMARY KEY (run_id, key)
             );
             CREATE INDEX IF NOT EXISTS tag_lookup ON tag(key, value);",
        )?;
        conn.pragma_update(None, "user_version", INDEX_VERSION)?;
        Ok(Self { conn })
    }

    /// Walk `root` and ingest every result file under it.
    ///
    /// A file that won't parse is recorded and skipped, never fatal: one
    /// corrupt or truncated upload on a share of ten thousand must not stop the
    /// other 9 999 being indexed.
    pub fn scan(&mut self, root: &Path) -> Result<ScanReport> {
        let mut report = ScanReport::default();
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !entry.file_type().is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            report.seen += 1;
            match self.ingest_file(path) {
                Ok(true) => report.ingested += 1,
                Ok(false) => report.unchanged += 1,
                Err(e) => report
                    .rejected
                    .push((path.display().to_string(), format!("{e:#}"))),
            }
        }
        Ok(report)
    }

    /// Returns whether the file was new. `false` means it was already indexed.
    pub fn ingest_file(&mut self, path: &Path) -> Result<bool> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        self.ingest_text(&text, path)
    }

    /// Ingest a document already in memory, recording `source` as where it came
    /// from. Splitting this out from `ingest_file` keeps the identity of a run
    /// tied to its *content* rather than to having been read off a disk.
    pub fn ingest_text(&mut self, text: &str, source: &Path) -> Result<bool> {
        // sha2 0.11 returns a hybrid-array `Array`, which has no `LowerHex`.
        let hash: String = Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM run WHERE content_hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .optional()?;
        if existing.is_some() {
            return Ok(false);
        }

        let doc =
            ResultFile::from_json(text).with_context(|| format!("parsing {}", source.display()))?;
        self.insert(&doc, source, &hash)?;
        Ok(true)
    }

    fn insert(&mut self, doc: &ResultFile, path: &Path, hash: &str) -> Result<()> {
        let key = doc.machine_key();
        let ident = doc.machine.identity.clone().unwrap_or_default();
        let caveats = doc.quality_caveats().join(" | ");
        let tx = self.conn.transaction()?;

        tx.execute(
            "INSERT INTO run (
                 machine_key, key_kind, hostname, taken_at, tool_version, preset, profile,
                 baseline, build_isa, overall_score, overall_grade, cpu_model, cpu_cores,
                 ram_bytes, os, arch, serial, asset_tag, on_ac, thermal_limited,
                 battery_health, battery_cycles, partial, caveats, source_path,
                 content_hash, ingested_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,
                       ?19,?20,?21,?22,?23,?24,?25,?26, datetime('now'))",
            params![
                key.value,
                key.kind.as_str(),
                doc.machine.hostname,
                doc.timestamp,
                doc.tool_version,
                doc.config.duration_preset,
                doc.config.profile,
                doc.config.baseline,
                doc.config.build_isa,
                doc.overall.score,
                doc.overall.grade,
                doc.machine.cpu_model,
                doc.machine.cpu_logical_cores as i64,
                doc.machine.ram_bytes as i64,
                doc.machine.os,
                doc.machine.arch,
                ident.serial,
                ident.asset_tag,
                doc.gates.as_ref().and_then(|g| g.on_ac),
                doc.telemetry.as_ref().map(|t| t.thermal_limited),
                doc.machine.battery.as_ref().and_then(|b| b.health_pct),
                doc.machine.battery.as_ref().and_then(|b| b.cycle_count),
                doc.is_partial(),
                caveats,
                path.display().to_string(),
                hash,
            ],
        )?;
        let run_id = tx.last_insert_rowid();

        for c in &doc.components {
            tx.execute(
                "INSERT INTO component (run_id, id, score, grade, graded) VALUES (?1,?2,?3,?4,?5)",
                params![run_id, c.id, c.score, c.grade, c.graded],
            )?;
            for s in &c.subtests {
                tx.execute(
                    "INSERT OR REPLACE INTO subtest
                       (run_id, component, id, value, unit, score, ratio, cv, confidence,
                        representative, scored)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,?8,NULL,1)",
                    params![
                        run_id,
                        c.id,
                        s.id,
                        s.value,
                        s.unit,
                        s.score,
                        s.ratio,
                        s.confidence
                    ],
                )?;
            }
        }

        // `raw` carries the spread and which statistic produced each value —
        // the data-quality signals — including for ungraded subtests that never
        // appear in `components`.
        for c in &doc.raw {
            for s in &c.subtests {
                tx.execute(
                    "INSERT INTO subtest
                       (run_id, component, id, value, unit, score, ratio, cv, confidence,
                        representative, scored)
                     VALUES (?1,?2,?3,?4,?5,NULL,NULL,?6,?7,?8,?9)
                     ON CONFLICT(run_id, component, id) DO UPDATE SET
                       cv = excluded.cv,
                       representative = excluded.representative,
                       scored = excluded.scored",
                    params![
                        run_id,
                        c.id,
                        s.id,
                        s.value,
                        s.unit,
                        s.stats.as_ref().map(|st| st.cv),
                        s.confidence,
                        s.representative,
                        s.scored,
                    ],
                )?;
            }
        }

        for (k, v) in &doc.tags {
            tx.execute(
                "INSERT OR REPLACE INTO tag (run_id, key, value) VALUES (?1,?2,?3)",
                params![run_id, k, v],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn run_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM run", [], |r| r.get(0))?)
    }

    pub fn machine_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(DISTINCT machine_key) FROM run", [], |r| {
                r.get(0)
            })?)
    }

    /// Tag keys and values present across the fleet, for building filters.
    pub fn tag_values(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT key, value FROM tag ORDER BY key, value")?;
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (k, v) = row?;
            out.entry(k).or_default().push(v);
        }
        Ok(out)
    }

    /// Raw handle for callers that need to run their own query. The analytics
    /// layer reads through this rather than growing a method per view.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn ingesting_the_same_file_twice_yields_one_run() {
        let mut idx = Index::open_in_memory().unwrap();
        assert!(idx.ingest_file(&fixture("win-modern.json")).unwrap());
        assert!(
            !idx.ingest_file(&fixture("win-modern.json")).unwrap(),
            "a share gets rescanned constantly; the second read must be a no-op"
        );
        assert_eq!(idx.run_count().unwrap(), 1);
        assert_eq!(idx.machine_count().unwrap(), 1);
    }

    /// The same document arriving under a different name is still the same run
    /// — a collector that renames on copy must not double-count it.
    #[test]
    fn the_same_content_under_a_new_name_is_not_a_new_run() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("win-modern.json")).unwrap();

        let dir = std::env::temp_dir().join(format!("lbf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("renamed-by-the-collector.json");
        std::fs::copy(fixture("win-modern.json"), &copy).unwrap();

        assert!(!idx.ingest_file(&copy).unwrap());
        assert_eq!(idx.run_count().unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn distinct_machines_are_distinct_runs() {
        let mut idx = Index::open_in_memory().unwrap();
        for f in ["win-modern.json", "linux-throttled.json", "low-end.json"] {
            assert!(idx.ingest_file(&fixture(f)).unwrap());
        }
        assert_eq!(idx.run_count().unwrap(), 3);
        assert_eq!(idx.machine_count().unwrap(), 3);
    }

    #[test]
    fn a_scan_reports_unreadable_files_without_failing() {
        let dir = std::env::temp_dir().join(format!("lbf-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(fixture("win-modern.json"), dir.join("good.json")).unwrap();
        std::fs::write(dir.join("truncated.json"), b"{\"schema\":\"loadbea").unwrap();
        std::fs::write(dir.join("notes.txt"), b"ignored, not json").unwrap();

        let mut idx = Index::open_in_memory().unwrap();
        let report = idx.scan(&dir).unwrap();

        assert_eq!(report.seen, 2, "only .json files are considered");
        assert_eq!(report.ingested, 1);
        assert_eq!(report.rejected.len(), 1, "the truncated file is reported");
        assert_eq!(
            idx.run_count().unwrap(),
            1,
            "one bad upload must not cost the good ones"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tags_are_indexed_for_grouping() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("win-modern.json")).unwrap();
        idx.ingest_file(&fixture("linux-throttled.json")).unwrap();
        let tags = idx.tag_values().unwrap();
        assert_eq!(
            tags.get("site").map(|v| v.len()),
            Some(2),
            "two sites present: {tags:?}"
        );
        assert!(tags.contains_key("ring"));
    }

    /// The quality signals have to survive into the index, or the dashboard
    /// will rank machines on numbers we already know are compromised.
    #[test]
    fn quality_flags_reach_the_index() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("linux-throttled.json")).unwrap();
        let (partial, thermal, caveats): (bool, Option<bool>, String) = idx
            .conn()
            .query_row(
                "SELECT partial, thermal_limited, caveats FROM run",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(partial);
        assert_eq!(thermal, Some(true));
        assert!(!caveats.is_empty());
    }

    #[test]
    fn the_peak_statistic_is_recorded_per_subtest() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("win-modern.json")).unwrap();
        let peaks: i64 = idx
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM subtest WHERE representative = 'peak'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            peaks > 0,
            "1.5.0+ marks the all-core subtests peak-reported; \
             comparing a peak against a median would be meaningless"
        );
    }
}
