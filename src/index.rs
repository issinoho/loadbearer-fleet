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
//! comparison this exists to make.
//!
//! So "derived" holds exactly when the documents are kept somewhere, and there
//! are two ways that happens: the collection folder keeps a file per run, or
//! `archive_dir` is set and this keeps one itself (see `Index::keep`). Where
//! neither is true, the index is the only copy of the history — which is why a
//! version change renames the old file rather than dropping its tables. The
//! rebuild costs a rescan either way, and being wrong about which situation an
//! operator is in should not cost them their history. See `set_aside`.

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

/// The largest document this will read, compressed or not.
///
/// A result is about 40 KB, so this is roughly 400x headroom — big enough that
/// no real document will ever meet it, small enough that a hostile one cannot
/// exhaust the machine. Both halves matter, because **anything that can write
/// to the collection folder controls this input**, and in the documented
/// deployment that is every machine in the estate.
///
/// Without a cap, gzip's ~1030:1 ratio turns 16 MB on the share into 16 GB in
/// memory, and the startup scan pays that cost too — so a single file could
/// stop the dashboard coming up at all, which is the failure that hides every
/// other one. Rejecting on size has to happen *before* the read, because the
/// schema reader rejecting the document afterwards is far too late.
const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

/// Read a result document, transparently un-gzipping a `.json.gz`.
///
/// Two reasons it handles both. The document archive is gzipped — a result is
/// about 40 KB of JSON and a fifth of that compressed — so importing an
/// archive is just scanning it. And a collection share is a fine place to
/// gzip, at which point a collector can write `.json.gz` directly and this
/// reads it without being told.
fn read_document(path: &Path) -> Result<String> {
    use std::io::Read as _;

    // Refuse on the file's own size before reading it, so a large `.json` is
    // never held in memory at all.
    let len = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    if len > MAX_DOCUMENT_BYTES {
        anyhow::bail!(
            "{} is {} bytes, over the {} byte limit for a result document — a result is about \
             40 KB, so this is not one",
            path.display(),
            len,
            MAX_DOCUMENT_BYTES
        );
    }

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if path.extension().and_then(|e| e.to_str()) != Some("gz") {
        return String::from_utf8(bytes)
            .with_context(|| format!("{} is not UTF-8", path.display()));
    }

    // Read one byte past the limit rather than up to it, so hitting the cap is
    // distinguishable from a document that happens to be exactly that long.
    let mut text = String::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("decompressing {}", path.display()))?;
    if text.len() as u64 > MAX_DOCUMENT_BYTES {
        anyhow::bail!(
            "{} decompresses to over {} bytes from {} on disk, which is a compression bomb \
             rather than a result document",
            path.display(),
            MAX_DOCUMENT_BYTES,
            len
        );
    }
    Ok(text)
}

/// Does this look like a result document rather than something else on the
/// share? Extension only — whether it *is* one is the parser's job.
fn is_document(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with(".json") || name.ends_with(".json.gz")
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
    /// Where to keep a copy of every document ingested, if anywhere.
    archive: Option<std::path::PathBuf>,
}

/// A machine that `forget` could act on.
#[derive(Debug, Clone)]
pub struct Forgettable {
    pub key: String,
    pub hostname: Option<String>,
    pub runs: i64,
}

/// What forgetting a machine removes, and what it can't.
#[derive(Debug, Default, Clone)]
pub struct ForgetPlan {
    pub runs: i64,
    pub archived: Vec<std::path::PathBuf>,
    /// Result files still in the collection folder. While any of these exist,
    /// the next scan will index the machine again — which is the one thing
    /// about removal that surprises people, so it is reported rather than
    /// assumed.
    pub sources_still_present: Vec<String>,
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

    /// Open an index that has to exist already.
    ///
    /// `Connection::open` creates the file, which is right for `scan` and
    /// `serve` and wrong for everything else: a command pointed at the wrong
    /// path otherwise answers confidently out of an empty database it has just
    /// invented. That is not a hypothetical — `forget <machine>` run without
    /// `--config` created `/tmp/fleet-index.db` and reported that the machine
    /// did not exist, while the service's index held it all along.
    pub fn open_existing(path: &Path) -> Result<Self> {
        if !path.exists() {
            anyhow::bail!(
                "there is no index at {}, so there is nothing to read. A service keeps its \
                 index where its configuration says — pass --config <file> to work on that \
                 one. For a new setup, `scan <folder>` creates it.",
                path.display()
            );
        }
        Self::open(path)
    }

    /// Keep a copy of every document ingested from here on, content-addressed
    /// under `dir`. See `Server::archive_dir` for when that is worth doing.
    pub fn with_archive(mut self, dir: Option<&Path>) -> Result<Self> {
        if let Some(dir) = dir {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating the document archive at {}", dir.display()))?;
            self.archive = Some(dir.to_path_buf());
        }
        Ok(self)
    }

    /// A consistent copy of the index, safe to take while the dashboard is
    /// running.
    ///
    /// `VACUUM INTO` rather than a file copy, and that is the whole point: a
    /// plain copy of a live SQLite database is a torn read, and nobody stops a
    /// dashboard nightly so a backup agent can have it. WAL lets this read
    /// while `serve` writes.
    ///
    /// The result is a compacted database of the current format, not an
    /// archive format — restore is putting it back where the index goes.
    pub fn backup_to(&self, target: &Path) -> Result<u64> {
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // SQLite refuses to overwrite, and so should this: silently replacing
        // somebody's last good copy is not a thing a backup command should do.
        if target.exists() {
            anyhow::bail!(
                "{} already exists; move it aside or pick another name",
                target.display()
            );
        }
        self.conn
            .execute("VACUUM INTO ?1", params![target.to_string_lossy()])
            .with_context(|| format!("writing a snapshot of the index to {}", target.display()))?;
        Ok(std::fs::metadata(target)?.len())
    }

    /// Test-only: a throwaway index that never touches disk.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        let archive = None;
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
        Ok(Self { conn, archive })
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
            if !is_document(path) {
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
        let text = read_document(path)?;
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

        // Archived before it is indexed, so that "in the index" always implies
        // "kept". If the archive can't be written, the file is left unindexed
        // and reported, and the next scan retries it — which surfaces as a
        // rejected file in the log, the metrics and the UI, rather than as
        // retention that quietly stopped working.
        self.keep(text, &hash)?;
        self.insert(&doc, source, &hash)?;
        Ok(true)
    }

    /// Keep the document under its own hash: `ab/abcdef….json.gz`.
    ///
    /// Content-addressed, so re-ingesting the same document is a no-op and the
    /// same bytes are never stored twice. The two-character prefix keeps
    /// directory sizes sane — a million runs across 256 directories rather
    /// than in one.
    fn keep(&self, text: &str, hash: &str) -> Result<()> {
        let Some(root) = &self.archive else {
            return Ok(());
        };
        let dir = root.join(&hash[..2]);
        let target = dir.join(format!("{hash}.json.gz"));
        // Immutable by construction: same name, same bytes.
        if target.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        // Written under a temporary name and renamed into place, so a crash or
        // a full disk can't leave a truncated file sitting there for ever
        // afterwards looking complete. The name is a claim about the contents.
        let partial = dir.join(format!("{hash}.json.gz.partial"));
        let mut encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&partial)
                .with_context(|| format!("creating {}", partial.display()))?,
            flate2::Compression::default(),
        );
        std::io::Write::write_all(&mut encoder, text.as_bytes())
            .and_then(|()| encoder.finish().map(|_| ()))
            .with_context(|| format!("writing {}", partial.display()))?;
        std::fs::rename(&partial, &target)
            .with_context(|| format!("moving {} into place", partial.display()))?;
        Ok(())
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

    /// One machine, as `forget` needs to see it before deciding anything.
    pub fn machines_matching(&self, needle: &str) -> Result<Vec<Forgettable>> {
        // Key first, then hostname, because the key is what the index is
        // actually organised by and an operator will usually type a hostname.
        let mut stmt = self.conn.prepare(
            "SELECT machine_key, hostname, COUNT(*) FROM run
               WHERE machine_key = ?1 COLLATE NOCASE OR hostname = ?1 COLLATE NOCASE
               GROUP BY machine_key ORDER BY machine_key",
        )?;
        let rows = stmt
            .query_map([needle], |r| {
                Ok(Forgettable {
                    key: r.get(0)?,
                    hostname: r.get(1)?,
                    runs: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// What `forget` would do, without doing it.
    pub fn forget_plan(&self, key: &str) -> Result<ForgetPlan> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_hash, source_path FROM run WHERE machine_key = ?1")?;
        let rows = stmt
            .query_map([key], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut plan = ForgetPlan::default();
        for (hash, source) in rows {
            plan.runs += 1;
            if let Some(root) = &self.archive {
                let doc = root.join(&hash[..2]).join(format!("{hash}.json.gz"));
                if doc.exists() {
                    plan.archived.push(doc);
                }
            }
            // Only what is still there: a path recorded on another server, or
            // a file already deleted, is not something to tell them about.
            if Path::new(&source).exists() {
                plan.sources_still_present.push(source);
            }
        }
        plan.sources_still_present.sort();
        plan.sources_still_present.dedup();
        Ok(plan)
    }

    /// Remove a machine: its runs, and the documents this archived for them.
    ///
    /// Deliberately **not** the files in the collection folder. This tool has
    /// no code that writes there, and deleting an estate's authoritative
    /// results is not a thing it should start doing on the strength of a
    /// hostname typed at a prompt. The plan reports which files are still
    /// there instead, because while they are, the next scan will index this
    /// machine straight back.
    pub fn forget(&mut self, key: &str) -> Result<ForgetPlan> {
        let plan = self.forget_plan(key)?;
        if plan.runs == 0 {
            return Ok(plan);
        }

        // The child tables declare ON DELETE CASCADE and `foreign_keys` is on,
        // so components, subtests and tags go with their runs.
        let removed = self
            .conn
            .execute("DELETE FROM run WHERE machine_key = ?1", [key])?;
        debug_assert_eq!(removed as i64, plan.runs);

        for doc in &plan.archived {
            std::fs::remove_file(doc)
                .with_context(|| format!("removing the archived document {}", doc.display()))?;
        }
        // Leave the two-character shard directories: empty ones cost nothing
        // and removing them races another scan writing into them.
        Ok(plan)
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

    /// A number that changes whenever **another connection** commits.
    ///
    /// SQLite's `data_version` is per-connection and deliberately does not move
    /// for this connection's own writes, which makes it exactly the question
    /// worth asking: has somebody else changed the database under us? A
    /// `forget` run from a terminal is another process, so the dashboard's
    /// in-memory snapshot would otherwise keep showing a machine that no longer
    /// exists until the next scan.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
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

    /// Anything that can write to the collection folder controls this input,
    /// and in the documented deployment that is every machine in the estate.
    /// gzip reaches about 1030:1, so without a cap a few megabytes on the share
    /// become gigabytes in memory — and the startup scan pays the same cost, so
    /// one file could stop the dashboard coming up at all.
    ///
    /// Rejecting has to happen before the whole thing is in memory. The schema
    /// reader refusing it afterwards is far too late, which is exactly what it
    /// used to do.
    #[test]
    fn a_compression_bomb_is_refused_before_it_is_decompressed() {
        use std::io::Write as _;

        let dir = scratch("bomb");
        let path = dir.join("bomb.json.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).expect("create"),
            flate2::Compression::best(),
        );
        // Compresses to a few kilobytes; well over the cap once expanded.
        let chunk = vec![b'A'; 1024 * 1024];
        for _ in 0..(MAX_DOCUMENT_BYTES / chunk.len() as u64 + 2) {
            gz.write_all(&chunk).expect("write");
        }
        gz.finish().expect("finish");

        let on_disk = std::fs::metadata(&path).expect("metadata").len();
        assert!(
            on_disk < MAX_DOCUMENT_BYTES,
            "the point of the test is a small file that expands past the cap; this one is \
             {on_disk} bytes on disk"
        );

        // Deliberately not `expect_err`: on a regression that would print the
        // whole decompressed document, and 17 MB of "AAAA..." in a CI log is
        // its own small outage.
        let err = match read_document(&path) {
            Ok(text) => panic!("read {} bytes instead of refusing", text.len()),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            err.contains("compression bomb"),
            "refused, but not for the right reason: {err}"
        );

        // And through the real scan path, where it must be reported and
        // survived rather than taking the process with it.
        let mut idx = Index::open_in_memory().unwrap();
        let report = idx.scan(&dir).expect("a bomb must not fail the whole scan");
        assert_eq!(report.ingested, 0);
        assert_eq!(report.rejected.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The uncompressed path has the same exposure without needing gzip at all.
    #[test]
    fn an_oversized_document_is_refused_on_its_size_alone() {
        let dir = scratch("oversize");
        let path = dir.join("big.json");
        std::fs::write(&path, vec![b'A'; MAX_DOCUMENT_BYTES as usize + 1]).expect("write");

        // Deliberately not `expect_err`: on a regression that would print the
        // whole decompressed document, and 17 MB of "AAAA..." in a CI log is
        // its own small outage.
        let err = match read_document(&path) {
            Ok(text) => panic!("read {} bytes instead of refusing", text.len()),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("over the"), "wrong refusal: {err}");

        let _ = std::fs::remove_dir_all(&dir);
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

    /// A scan only ever adds, so removing a machine has to be asked for. This
    /// checks the whole of it goes: runs, and the rows that hang off them.
    #[test]
    fn forgetting_a_machine_takes_its_runs_and_everything_under_them() {
        let mut idx = Index::open_in_memory().unwrap();
        for f in ["win-modern.json", "linux-throttled.json", "low-end.json"] {
            idx.ingest_file(&fixture(f)).unwrap();
        }
        let before: i64 = idx
            .conn
            .query_row("SELECT COUNT(*) FROM subtest", [], |r| r.get(0))
            .unwrap();
        assert!(before > 0);

        let found = idx.machines_matching("FLEET-WIN-01").unwrap();
        assert_eq!(found.len(), 1, "matched by hostname");
        assert_eq!(found[0].runs, 1);

        let plan = idx.forget(&found[0].key).unwrap();
        assert_eq!(plan.runs, 1);
        assert_eq!(idx.run_count().unwrap(), 2, "the other two are untouched");
        assert_eq!(idx.machine_count().unwrap(), 2);

        // The cascade is the part worth asserting: components, subtests and
        // tags are separate tables and could easily be orphaned.
        for table in ["component", "subtest", "tag"] {
            let orphans: i64 = idx
                .conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE run_id NOT IN (SELECT id FROM run)"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(orphans, 0, "{table} rows left behind");
        }
        assert!(
            idx.machines_matching("FLEET-WIN-01").unwrap().is_empty(),
            "and it is gone"
        );
    }

    #[test]
    fn forgetting_removes_the_archived_documents_too() {
        let dir = scratch("forget-archive");
        let archive = dir.join("archive");
        let mut idx = Index::open(&dir.join("i.db"))
            .unwrap()
            .with_archive(Some(&archive))
            .unwrap();
        idx.ingest_file(&fixture("win-modern.json")).unwrap();
        idx.ingest_file(&fixture("low-end.json")).unwrap();

        let count_docs = || {
            walkdir::WalkDir::new(&archive)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .count()
        };
        assert_eq!(count_docs(), 2);

        let key = idx.machines_matching("FLEET-WIN-01").unwrap()[0]
            .key
            .clone();
        let plan = idx.forget(&key).unwrap();
        assert_eq!(plan.archived.len(), 1);
        assert_eq!(
            count_docs(),
            1,
            "only the forgotten machine's document goes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The surprise worth reporting: while the result file is still on the
    /// share, the next scan indexes the machine straight back.
    #[test]
    fn forgetting_reports_the_source_files_that_would_bring_it_back() {
        let dir = scratch("forget-source");
        let results = dir.join("results");
        std::fs::create_dir_all(&results).unwrap();
        let source = results.join("PC-01.json");
        std::fs::copy(fixture("win-modern.json"), &source).unwrap();

        let mut idx = Index::open(&dir.join("i.db")).unwrap();
        idx.scan(&results).unwrap();
        let key = idx.machines_matching("FLEET-WIN-01").unwrap()[0]
            .key
            .clone();

        let plan = idx.forget(&key).unwrap();
        assert_eq!(plan.sources_still_present.len(), 1, "{plan:?}");
        assert!(plan.sources_still_present[0].contains("PC-01.json"));

        // And it does come back, which is why that is worth saying out loud.
        idx.scan(&results).unwrap();
        assert_eq!(idx.run_count().unwrap(), 1);

        // Once the file is gone, forgetting sticks.
        let key = idx.machines_matching("FLEET-WIN-01").unwrap()[0]
            .key
            .clone();
        std::fs::remove_file(&source).unwrap();
        let plan = idx.forget(&key).unwrap();
        assert!(plan.sources_still_present.is_empty());
        idx.scan(&results).unwrap();
        assert_eq!(idx.run_count().unwrap(), 0, "stays forgotten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plan_alone_changes_nothing() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("win-modern.json")).unwrap();
        let key = idx.machines_matching("FLEET-WIN-01").unwrap()[0]
            .key
            .clone();
        let plan = idx.forget_plan(&key).unwrap();
        assert_eq!(plan.runs, 1);
        assert_eq!(idx.run_count().unwrap(), 1, "dry run must not remove");
    }

    /// Hostnames are reissued between machines, so one can match two. Guessing
    /// which to delete is not a thing to do.
    #[test]
    fn one_hostname_can_match_two_machines() {
        let mut idx = Index::open_in_memory().unwrap();
        let text = std::fs::read_to_string(fixture("win-modern.json")).unwrap();
        for serial in ["SN-FIRST", "SN-SECOND"] {
            let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
            doc["machine"]["identity"]["serial"] = serde_json::json!(serial);
            idx.ingest_text(&doc.to_string(), Path::new("x.json"))
                .unwrap();
        }
        let found = idx.machines_matching("FLEET-WIN-01").unwrap();
        assert_eq!(found.len(), 2, "two machines, one reissued hostname");
        assert!(
            idx.machines_matching("SN-FIRST").unwrap().len() == 1,
            "the key is unambiguous"
        );
    }

    #[test]
    fn forgetting_something_that_was_never_there_is_not_an_error() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("win-modern.json")).unwrap();
        let plan = idx.forget("no-such-machine").unwrap();
        assert_eq!(plan.runs, 0);
        assert_eq!(idx.run_count().unwrap(), 1);
        assert!(idx.machines_matching("no-such-machine").unwrap().is_empty());
    }

    /// The point of the archive: the run survives its own result file being
    /// overwritten, so the index is genuinely derived again and a rebuild
    /// doesn't cost the trend.
    #[test]
    fn an_archived_run_outlives_the_file_it_came_from() {
        let dir = scratch("archive");
        let results = dir.join("results");
        let archive = dir.join("archive");
        std::fs::create_dir_all(&results).unwrap();

        let write = |month: &str, score: f64| {
            let mut doc: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(fixture("win-modern.json")).unwrap())
                    .unwrap();
            doc["timestamp"] = serde_json::json!(format!("2026-{month}-01T10:00:00Z"));
            doc["overall"]["score"] = serde_json::json!(score);
            // The same filename every time, as an overwriting collector does.
            std::fs::write(results.join("PC-01.json"), doc.to_string()).unwrap();
        };

        {
            let mut idx = Index::open(&dir.join("i.db"))
                .unwrap()
                .with_archive(Some(&archive))
                .unwrap();
            write("07", 1500.0);
            idx.scan(&results).unwrap();
            write("09", 1200.0);
            idx.scan(&results).unwrap();
            assert_eq!(idx.run_count().unwrap(), 2, "both runs indexed");
        }

        // The folder has lost July. Rebuild from the folder alone and it stays
        // lost; rebuild from the archive and it comes back.
        {
            let mut idx = Index::open(&dir.join("from-folder.db")).unwrap();
            idx.scan(&results).unwrap();
            assert_eq!(idx.run_count().unwrap(), 1, "the folder only has September");
        }
        {
            let mut idx = Index::open(&dir.join("from-archive.db")).unwrap();
            idx.scan(&archive).unwrap();
            assert_eq!(
                idx.run_count().unwrap(),
                2,
                "the archive still has both, so importing it restores the history"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_archive_stores_a_document_once_and_names_it_by_its_hash() {
        let dir = scratch("archive-dedup");
        let archive = dir.join("archive");
        let mut idx = Index::open(&dir.join("i.db"))
            .unwrap()
            .with_archive(Some(&archive))
            .unwrap();

        idx.ingest_file(&fixture("win-modern.json")).unwrap();
        // The same document again, under another name: one run, one file.
        let copy = dir.join("renamed.json");
        std::fs::copy(fixture("win-modern.json"), &copy).unwrap();
        idx.ingest_file(&copy).unwrap();

        let kept: Vec<_> = walkdir::WalkDir::new(&archive)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(kept.len(), 1, "content-addressed, so stored once: {kept:?}");
        assert!(kept[0].ends_with(".json.gz"), "{}", kept[0]);

        let hash: String = idx
            .conn
            .query_row("SELECT content_hash FROM run", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept[0], format!("{hash}.json.gz"), "named by its hash");
        assert!(
            archive.join(&hash[..2]).is_dir(),
            "sharded by the first two characters"
        );
        // Nothing half-written left behind.
        assert!(!kept[0].ends_with(".partial"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gzipped document has to be readable wherever a plain one is, or
    /// importing an archive would need a second code path.
    #[test]
    fn a_gzipped_document_is_ingested_like_any_other() {
        let dir = scratch("gz");
        let text = std::fs::read_to_string(fixture("win-modern.json")).unwrap();
        let path = dir.join("PC-01.json.gz");
        let mut enc = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        std::io::Write::write_all(&mut enc, text.as_bytes()).unwrap();
        enc.finish().unwrap();

        let mut idx = Index::open_in_memory().unwrap();
        let report = idx.scan(&dir).unwrap();
        assert_eq!(report.seen, 1, "a .json.gz counts as a document");
        assert_eq!(report.ingested, 1);
        assert_eq!(idx.run_count().unwrap(), 1);

        // And it is the same run as the uncompressed original, by content.
        let mut plain = Index::open_in_memory().unwrap();
        plain.ingest_file(&fixture("win-modern.json")).unwrap();
        let gz_hash: String = idx
            .conn
            .query_row("SELECT content_hash FROM run", [], |r| r.get(0))
            .unwrap();
        let plain_hash: String = plain
            .conn
            .query_row("SELECT content_hash FROM run", [], |r| r.get(0))
            .unwrap();
        assert_eq!(gz_hash, plain_hash);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A snapshot has to be a real database with the same contents, not a
    /// file-shaped hope.
    #[test]
    fn a_backup_is_a_complete_and_openable_copy() {
        let dir = scratch("backup");
        let db = dir.join("i.db");
        let mut idx = Index::open(&db).unwrap();
        for f in ["win-modern.json", "linux-throttled.json", "low-end.json"] {
            idx.ingest_file(&fixture(f)).unwrap();
        }

        let snapshot = dir.join("snapshots").join("fleet.db");
        let bytes = idx.backup_to(&snapshot).unwrap();
        assert!(bytes > 0, "wrote nothing");
        assert!(snapshot.exists(), "did not create the parent directory");

        let restored = Index::open(&snapshot).unwrap();
        assert_eq!(restored.run_count().unwrap(), 3);
        assert_eq!(restored.machine_count().unwrap(), 3);
        assert_eq!(
            restored.tag_values().unwrap(),
            idx.tag_values().unwrap(),
            "the whole thing, not just the run table"
        );

        // Refuses to clobber: overwriting somebody's last good copy is not a
        // thing a backup command should do quietly.
        let err = idx
            .backup_to(&snapshot)
            .expect_err("must refuse")
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
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
