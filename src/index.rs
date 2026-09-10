//! The SQLite index over a folder of result files.
//!
//! The **folder is the source of truth**; this index is a derived read model.
//! It can be deleted and rebuilt from the same files, which is what makes
//! schema changes here cheap and why a change to `INDEX_VERSION` rebuilds
//! rather than migrating.
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
//!
//! **Which qualifies "disposable", and it took an experiment to notice.** A row
//! per run plus a collector that overwrites one file per machine means the
//! index accumulates runs whose files no longer exist. Rebuilding it then
//! recovers the current state of the fleet and silently loses the trend — the
//! comparison this exists to make. So the index is derived exactly when the
//! collection folder keeps a file per run, and is the only copy of the history
//! when it doesn't.
//!
//! Which is why a version change renames the old file rather than dropping its
//! tables: the rebuild costs a rescan either way, and being wrong about which
//! of those two situations an operator is in should not cost them their
//! history. See `set_aside`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::schema::ResultFile;

/// Bumped when the derived tables change shape. A mismatch rebuilds from the
/// folder rather than migrating — see `set_aside` for what happens to the old
/// file, which is not simply deleted.
const INDEX_VERSION: i64 = 1;

/// The version stamp on an existing index file, or `None` if there is no file
/// yet. A file with no stamp reads as version 0, which is what a database this
/// build has only just created looks like.
///
/// Any WAL is folded back into the main file on the way out, so that if the
/// caller goes on to move it aside, the copy left behind isn't missing whatever
/// was committed since the last checkpoint.
fn stamped_version(path: &Path) -> Result<Option<i64>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open(path)
        .with_context(|| format!("reading the index version from {}", path.display()))?;
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if found != INDEX_VERSION {
        // Best effort: a database that was never WAL has nothing to check
        // point, and failing here would be a worse outcome than a slightly
        // stale copy.
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    }
    drop(conn);
    Ok(Some(found))
}

/// Move an index this build can't read out of the way, keeping it.
///
/// The obvious thing is to drop the tables and rescan, and for a long time
/// that is what this did — the index is derived, after all. It is derived
/// *only as long as the collection folder keeps a file per run*: the index
/// holds a row per run, so a collector that overwrites one file per machine
/// leaves the index as the sole record of every earlier run. Dropping it then
/// recovers the current state of the fleet and quietly destroys the trend,
/// which is the comparison this tool exists to make.
///
/// So the file is renamed instead. The rebuild costs a rescan either way; this
/// way the history is still on disk if anyone wants it back.
///
/// A failure to rename is fatal rather than a fallback to deleting. This only
/// happens during a deliberate upgrade, with somebody watching, and refusing
/// to start with an explanation is a better outcome than starting successfully
/// having thrown away the one copy of their history.
fn set_aside(path: &Path, found: i64) -> Result<()> {
    let target = free_name(path, found);
    std::fs::rename(path, &target).with_context(|| {
        format!(
            "moving the old index {} aside to {}. This build reads index format {} and that file \
             is format {}, so it cannot be used as it is — but it may hold run history that the \
             collection folder no longer has, so it is not deleted. Move or delete it yourself to \
             continue.",
            path.display(),
            target.display(),
            INDEX_VERSION,
            found
        )
    })?;

    // The sidecars belong to the file that just moved. Leaving them next to the
    // original path would hand a stale write-ahead log to the fresh database
    // about to be created there, so they travel with it.
    for suffix in ["-wal", "-shm"] {
        let from = sidecar(path, suffix);
        if from.exists() {
            let to = sidecar(&target, suffix);
            if let Err(e) = std::fs::rename(&from, &to) {
                tracing::warn!(
                    path = %from.display(),
                    error = %e,
                    "could not move this alongside the old index; removing it instead so it \
                     cannot be mistaken for the new index's log"
                );
                std::fs::remove_file(&from)
                    .with_context(|| format!("removing the stale {}", from.display()))?;
            }
        }
    }

    tracing::warn!(
        found,
        expected = INDEX_VERSION,
        preserved = %target.display(),
        "the index was built by another version; it has been kept under a new name and a fresh \
         one will be rebuilt from the collection folder"
    );
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// `fleet-index.superseded-v1-20260910T091500Z.db`, keeping the extension so
/// the preserved file still opens in anything that reads SQLite. Counts up if
/// that name is taken, so two upgrades in the same second can't collide.
fn free_name(path: &Path, found: i64) -> std::path::PathBuf {
    let now = OffsetDateTime::now_utc();
    let stamp = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "index".to_string());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for attempt in 0.. {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{}", attempt + 1)
        };
        let candidate =
            path.with_file_name(format!("{stem}.superseded-v{found}-{stamp}{suffix}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("0.. is unbounded")
}

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
        // Checked before the file is opened for use, because a version this
        // build doesn't know has to be moved out of the way rather than
        // reused — see `set_aside`.
        if let Some(found) = stamped_version(path)?
            && found != INDEX_VERSION
        {
            set_aside(path, found)?;
        }
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

        // No version check here: a file-backed index of another version never
        // reaches this point, because `open` moves it aside first, and an
        // in-memory one is always empty. The tables are created
        // `IF NOT EXISTS` so both cases land in the same place.
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
        // A share that is down must not look like a folder that is empty.
        // That is the failure which hides every other one, because "no
        // machines need attention" reads as good news, so the root is checked
        // before anything is counted.
        let meta = std::fs::metadata(root).with_context(|| {
            format!(
                "reading the collection folder {} — if this is a share, check it is reachable \
                 and that this account can read it",
                root.display()
            )
        })?;
        if !meta.is_dir() {
            anyhow::bail!("{} is not a directory", root.display());
        }

        let mut report = ScanReport::default();
        for entry in walkdir::WalkDir::new(root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    // Usually a subtree this account cannot read. Reporting it
                    // keeps a partial scan from passing for a complete one.
                    let path = e
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| root.display().to_string());
                    report.rejected.push((path, format!("{e}")));
                    continue;
                }
            };
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

    /// A directory of its own per test, since these ones touch real files.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lbf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn runs_in(path: &Path) -> i64 {
        let conn = Connection::open(path).expect("open");
        conn.query_row("SELECT COUNT(*) FROM run", [], |r| r.get(0))
            .expect("count")
    }

    /// The whole point of `set_aside`. An index this build can't read holds
    /// history the collection folder may no longer have — a collector that
    /// overwrites one file per machine leaves the index as the only record of
    /// earlier runs — so it is renamed, not dropped.
    #[test]
    fn an_index_from_another_version_is_kept_rather_than_deleted() {
        let dir = scratch("supersede");
        let db = dir.join("fleet-index.db");
        {
            let mut idx = Index::open(&db).unwrap();
            idx.ingest_file(&fixture("win-modern.json")).unwrap();
            assert_eq!(idx.run_count().unwrap(), 1);
        }
        // Stamp it as something this build has never heard of.
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "user_version", 99_i64).unwrap();
        }

        let reopened = Index::open(&db).unwrap();
        assert_eq!(
            reopened.run_count().unwrap(),
            0,
            "the working index starts empty and is rebuilt by the next scan"
        );
        drop(reopened);

        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("superseded"))
            .collect();
        assert_eq!(kept.len(), 1, "exactly one preserved file: {kept:?}");
        let name = &kept[0];
        assert!(
            name.contains("-v99-"),
            "the old version is in the name: {name}"
        );
        assert!(name.ends_with(".db"), "still openable as SQLite: {name}");
        assert_eq!(
            runs_in(&dir.join(name)),
            1,
            "and the run it held is still there"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ordinary case must not touch anything: reopening an index of the
    /// current version keeps its rows and leaves no debris behind.
    #[test]
    fn reopening_a_current_index_preserves_it_and_sets_nothing_aside() {
        let dir = scratch("reopen");
        let db = dir.join("fleet-index.db");
        {
            let mut idx = Index::open(&db).unwrap();
            idx.ingest_file(&fixture("win-modern.json")).unwrap();
        }
        for _ in 0..3 {
            let idx = Index::open(&db).unwrap();
            assert_eq!(idx.run_count().unwrap(), 1);
        }
        let debris = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("superseded"))
            .count();
        assert_eq!(debris, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stale write-ahead log left at the original path would be handed to the
    /// fresh database created there, so the sidecars have to travel with the
    /// file that owns them.
    #[test]
    fn nothing_of_the_old_index_is_left_at_the_original_path() {
        let dir = scratch("sidecars");
        let db = dir.join("fleet-index.db");
        {
            let mut idx = Index::open(&db).unwrap();
            idx.ingest_file(&fixture("low-end.json")).unwrap();
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "user_version", 42_i64).unwrap();
        }
        let _ = Index::open(&db).unwrap();

        for suffix in ["-wal", "-shm"] {
            let orphan = sidecar(&db, suffix);
            if orphan.exists() {
                // It may exist because the *new* database made it; what matters
                // is that it belongs to the new one, which holds no runs.
                assert_eq!(
                    runs_in(&db),
                    0,
                    "a sidecar at the original path must belong to the fresh index"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preserved_name_does_not_collide_with_one_already_there() {
        let dir = scratch("collide");
        let db = dir.join("fleet-index.db");
        std::fs::write(&db, b"").unwrap();
        let first = free_name(&db, 7);
        std::fs::write(&first, b"").unwrap();
        let second = free_name(&db, 7);
        assert_ne!(first, second, "the second must pick a different name");
        assert!(
            second.to_string_lossy().contains("-2"),
            "counted up: {}",
            second.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The silent failure this guards against: an unreachable share reporting
    /// an empty folder, which the dashboard would render as a healthy estate
    /// with nothing to worry about.
    #[test]
    fn a_scan_of_an_unreachable_folder_fails_rather_than_reporting_it_empty() {
        let mut idx = Index::open_in_memory().unwrap();
        let missing = std::env::temp_dir().join(format!("lbf-not-here-{}", std::process::id()));
        let err = idx
            .scan(&missing)
            .expect_err("a folder that cannot be read is not an empty folder")
            .to_string();
        assert!(err.contains("collection folder"), "{err}");

        // And a file where a folder was expected is just as wrong.
        let file = std::env::temp_dir().join(format!("lbf-file-{}.json", std::process::id()));
        std::fs::write(&file, b"{}").unwrap();
        assert!(idx.scan(&file).is_err());
        let _ = std::fs::remove_file(&file);
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
