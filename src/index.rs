//! The SQLite index over a folder of result files.
//!
//! The **folder is the source of truth**; this index is a derived read model.
//! It can be deleted and rebuilt from the same files, which is what makes
//! schema changes here cheap and why a change to `INDEX_VERSION` rebuilds
//! rather than migrating.
//!
//! That is also why nothing an operator *says* is kept here. Acknowledgements,
//! snoozes and suppressions are in no result document, so a rebuild could not
//! bring them back; they live in the state file (`state.rs`) instead, and this
//! file stays safe to delete.
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
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::schema::{Collected, Document, ResultFile, RunStatus};

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
/// What an uploaded document is called once it is in the collection folder.
///
/// **Nothing the uploader sent is used.** The name is built from the parsed
/// document and the hash of its bytes, which is what makes it impossible to
/// weaponise: a hostname of `../../etc/cron.d/x` is text in a JSON field, and
/// text is all it ever becomes.
///
/// It is *readable* rather than merely safe, because the folder is something a
/// person has to look at: `SGS-D47TDY3-20260911T163000-8f3ac1d2.json` follows
/// the same shape this project recommends for collected copies generally. The
/// hash suffix makes it unique — two runs of one machine in the same second
/// would otherwise collide — and ties the name to the contents.
/// Distinguishes temporary files written by this process from each other.
static PARTIAL: AtomicU64 = AtomicU64::new(0);

/// Write a new file, refusing to write *through* anything already at that path.
///
/// `create_new` is the whole point of not using `fs::write` here. `fs::write`
/// opens the path and truncates whatever it finds, following a symlink to
/// wherever it points; `create_new` fails if the path exists at all, symlink
/// included, because the underlying open is exclusive.
///
/// That matters more here than it would elsewhere. The upload directory has to
/// live inside the collection folder, and this project's own threat model says
/// anything that can write to that folder controls it — in the documented
/// deployment, every machine in the estate. A planted symlink is a way to make
/// the service write a chosen file to a chosen place as its own account, and
/// refusing to open an existing path removes it.
fn write_new(path: &Path, text: &str) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    // The rename that follows is atomic, but only with respect to a reader —
    // a crash between the two would otherwise leave a file whose name claims
    // contents it does not have.
    file.sync_all()
        .with_context(|| format!("flushing {}", path.display()))?;
    Ok(())
}

fn upload_name(doc: &ResultFile, hash: &str) -> String {
    let host = doc
        .machine
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or("machine");
    // Anything that is not plainly a filename character becomes a hyphen: no
    // separators, no traversal, no leading dot, no surprises on either
    // platform. Truncated because a hostname has no length limit and a path
    // does.
    let safe: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let safe = safe.trim_matches('-');
    let safe = if safe.is_empty() { "machine" } else { safe };

    // The document's own timestamp, compacted: 2026-09-11T16:30:00Z becomes
    // 20260911T163000, which sorts chronologically in a directory listing.
    let stamp: String = doc
        .timestamp
        .chars()
        .take_while(|c| *c != '.' && *c != '+')
        .filter(|c| c.is_ascii_alphanumeric())
        .take(15)
        .collect();

    format!("{safe}-{stamp}-{}.json", &hash[..8])
}

/// The file a machine's run status record lives in: `status-` and the first 16
/// hex digits of the SHA-256 of its machine key. The key is whatever the
/// machine said it was, so it never reaches a filename itself, and the prefix
/// keeps the file apart from any result file's name (`<host>-<stamp>-<hash>`)
/// while saying what it is to someone looking at the folder.
pub fn status_name(machine_key: &str) -> String {
    format!("status-{}.json", &content_hash(machine_key)[..16])
}

/// A status record's timestamp as an instant, or `None` where it is not RFC
/// 3339.
fn status_instant(at: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(at.trim(), &time::format_description::well_known::Rfc3339).ok()
}

/// A document's identity. sha2 0.11 returns a hybrid-array `Array`, which has
/// no `LowerHex`, hence the hand-rolled hex.
fn content_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A document's `u64` as SQLite's signed integer. NVMe counters are 128-bit on
/// the wire and truncated by loadbearer, so a corrupt or hostile one can be
/// anything; pinning it at the top is still "a very large number" rather than
/// a negative one or a failed ingest.
fn saturating(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// A document's string trimmed, and `None` when that leaves nothing: an empty
/// edition is not an edition called "", and should read as unrecorded.
fn present(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Columns added to an existing index in place.
///
/// Deliberately *not* an `INDEX_VERSION` bump. A version change moves the old
/// file aside and rebuilds from the collection folder, which is right when the
/// derived shape changes meaning — and wrong for a column that is simply new,
/// because a collector that overwrites one file per machine leaves history in
/// the index that the folder no longer has. An added column costs nothing to
/// an older build either: every query names its columns, so one it has never
/// heard of is invisible to it.
///
/// Rows written before the column existed keep `NULL`, which is the honest
/// answer — see `scan --reindex` for filling them in from the documents.
fn add_missing_columns(conn: &Connection) -> Result<()> {
    const ADDED: [(&str, &str, &str); 41] = [
        ("subtest", "label", "TEXT"),
        ("subtest", "direction", "TEXT"),
        // Component ids are short and lower-case — `cpu`, `memory` — so a
        // heading has to come from the document rather than from title-casing
        // an id into "Cpu".
        ("component", "label", "TEXT"),
        // Who submitted a run through the dashboard, and when. NULL for the
        // ordinary case of a file that arrived in the collection folder by
        // itself, which is most of them.
        ("run", "uploaded_by", "TEXT"),
        ("run", "uploaded_at", "TEXT"),
        // The machine-age evidence, from loadbearer 1.6.0's `machine.age`,
        // `firmware` and `drives` blocks. NULL for older documents, which carry
        // none of it. The drive columns are the first drive's, the one
        // loadbearer's own age estimate reads.
        ("run", "system_model", "TEXT"),
        ("run", "cpu_family", "TEXT"),
        ("run", "cpu_launched", "TEXT"),
        ("run", "max_age_months", "INTEGER"),
        ("run", "firmware_dated", "TEXT"),
        ("run", "firmware_likely_original", "INTEGER"),
        ("run", "drive_power_on_hours", "INTEGER"),
        ("run", "drive_wear_pct", "INTEGER"),
        ("run", "drive_hours_undercounted", "INTEGER"),
        // loadbearer 1.7.0's `machine.power`: the plan, the power mode (or
        // Linux platform profile) for the source the run was on, and the
        // settings that held it back, " | "-joined like `caveats`.
        ("run", "power_plan", "TEXT"),
        ("run", "power_mode", "TEXT"),
        ("run", "power_held_back", "TEXT"),
        // The firmware's own version and date, from `machine.firmware`
        // (1.6.0+), for comparing a machine with others of its model.
        // `firmware_dated` above is the age estimate's month, not this.
        ("run", "bios_version", "TEXT"),
        ("run", "bios_date", "TEXT"),
        // The fixed volume with the lowest free percentage — see
        // `Machine::fullest_fixed_volume`. Unlike the columns above, every
        // document version carries `machine.disks`, so a reindex fills these
        // in for old runs too.
        ("run", "disk_low_mount", "TEXT"),
        ("run", "disk_low_free_pct", "REAL"),
        ("run", "disk_low_free_bytes", "INTEGER"),
        // The `run` block of a loadbearer release after 1.8.1: its random ID,
        // when the benchmarks started and how long they took. `run_` because
        // `run.id` is this table's own row number. NULL for older documents.
        ("run", "run_uuid", "TEXT"),
        ("run", "run_started_at", "TEXT"),
        ("run", "run_duration_secs", "REAL"),
        // `machine.os_detail`, the OS release in parts beside `os`.
        ("run", "os_distro", "TEXT"),
        ("run", "os_version", "TEXT"),
        ("run", "os_build", "TEXT"),
        ("run", "os_edition", "TEXT"),
        // `machine.memory`'s two counts; the modules are the `memory_module`
        // table. NULL where the block is absent — Linux without root, or an
        // older CLI — which is unknown, not "no slots".
        ("run", "memory_slots", "INTEGER"),
        ("run", "memory_populated", "INTEGER"),
        // What `--collect` asked for (`run.collect`), as sorted, comma-joined
        // names — `elevation,security,uptime` — or NULL for nothing asked,
        // which every older run is. It is what tells the columns after it
        // apart: NULL in one of them is "not asked" when its category is
        // missing here, and "asked, could not be read" when it is listed.
        ("run", "run_collect", "TEXT"),
        // `run.elevated`, 1 / 0 / NULL. Never written as 0 for "unknown".
        ("run", "run_elevated", "INTEGER"),
        // `machine.boot`, as of the run: when the machine last booted and
        // how long it had been up then. Not a live uptime.
        ("run", "booted_at", "TEXT"),
        ("run", "uptime_secs", "INTEGER"),
        // `machine.security`. Secure Boot and TPM presence are tri-state
        // integers, the vocabularies text as the document has them.
        ("run", "firmware_mode", "TEXT"),
        ("run", "secure_boot", "INTEGER"),
        ("run", "tpm_present", "INTEGER"),
        ("run", "tpm_version", "TEXT"),
        // `disk_encryption.system_volume` and `.method`: Linux only, the root
        // filesystem only, dm-crypt only. `none_detected` is not "unencrypted".
        ("run", "disk_encryption", "TEXT"),
        ("run", "disk_encryption_method", "TEXT"),
    ];

    for (table, column, decl) in ADDED {
        let mut present = false;
        {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                if row.get::<_, String>(1)? == column {
                    present = true;
                    break;
                }
            }
        }
        if !present {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
        }
    }
    Ok(())
}

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
pub const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

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

/// A machine that `forget` could act on. `runs` is 0 for one known only from
/// its run status records.
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
    /// 1 when the machine has a run status record, which is removed with it.
    pub statuses: i64,
    pub archived: Vec<std::path::PathBuf>,
    /// Result files, and the status file, still in the collection folder. While any of these exist,
    /// the next scan will index the machine again — which is the one thing
    /// about removal that surprises people, so it is reported rather than
    /// assumed.
    pub sources_still_present: Vec<String>,
}

/// What one scan did, for logging and for the UI's "last refreshed" line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// Result files met. A run status file is counted in `statuses` and in
    /// none of these, so `seen` is still `ingested` + `unchanged` + the files
    /// among `rejected`.
    pub seen: usize,
    pub ingested: usize,
    pub unchanged: usize,
    /// Run status files met, new or not. Not a rejection: a skipped run's
    /// record is a document this reads, just not a result.
    pub statuses: usize,
    pub rejected: Vec<(String, String)>,
}

/// What reading one document came to.
enum Ingested {
    /// A result; `true` when it was new.
    Run(bool),
    /// A run status record, whether it was kept, replaced something, or was
    /// not newer than what is held.
    Status,
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
                 label   TEXT,
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
                 -- Both added after the fact; see `add_missing_columns`.
                 label      TEXT,
                 direction  TEXT,
                 PRIMARY KEY (run_id, component, id)
             );

             CREATE TABLE IF NOT EXISTS tag (
                 run_id INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
                 key    TEXT NOT NULL,
                 value  TEXT NOT NULL,
                 PRIMARY KEY (run_id, key)
             );
             CREATE INDEX IF NOT EXISTS tag_lookup ON tag(key, value);

             -- Every drive's health counters, from loadbearer 1.6.0's
             -- `machine.drives`; the `run` table keeps only the first one's,
             -- for the age line. Added after the fact, and `IF NOT EXISTS`
             -- is what gives an existing index the table without a format
             -- bump: its older runs simply have no rows here until
             -- `scan --reindex`.
             CREATE TABLE IF NOT EXISTS drive (
                 run_id              INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
                 -- Order in the document: on Windows the physical drive
                 -- number, on Linux NVMe first. 0 is usually the boot drive.
                 position            INTEGER NOT NULL,
                 device              TEXT NOT NULL,
                 interface           TEXT NOT NULL,
                 model               TEXT,
                 power_on_hours      INTEGER NOT NULL,
                 power_cycles        INTEGER,
                 unsafe_shutdowns    INTEGER,
                 percentage_used     INTEGER,
                 available_spare_pct INTEGER,
                 data_written_tb     REAL,
                 media_errors        INTEGER,
                 critical_warning    INTEGER,
                 PRIMARY KEY (run_id, position)
             );

             -- Every memory module from `machine.memory` (a loadbearer
             -- release after 1.8.1), one row per filled slot. Added after the
             -- fact like `drive`, and `IF NOT EXISTS` for the same reason: an
             -- existing index gains it on open, with no rows for the runs it
             -- already holds until `scan --reindex`.
             CREATE TABLE IF NOT EXISTS memory_module (
                 run_id               INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
                 -- Order in the document, which is the firmware table's.
                 position             INTEGER NOT NULL,
                 size_bytes           INTEGER,
                 type                 TEXT,
                 form_factor          TEXT,
                 speed_mts            INTEGER,
                 configured_speed_mts INTEGER,
                 PRIMARY KEY (run_id, position)
             );

             -- The latest run status record per machine: a skipped run, which
             -- is not a run and so is not in `run`. One row per machine key,
             -- the newest by its own timestamp. Added after the fact like
             -- `drive`, `IF NOT EXISTS` for the same reason, and derived the
             -- same way: `scan --reindex` rebuilds it from the folder.
             CREATE TABLE IF NOT EXISTS run_status (
                 machine_key  TEXT PRIMARY KEY,
                 hostname     TEXT,
                 status       TEXT,
                 reason       TEXT,
                 detail       TEXT,
                 at           TEXT NOT NULL,
                 tool_version TEXT,
                 source_path  TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 ingested_at  TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS run_status_hash ON run_status(content_hash);
             CREATE TABLE IF NOT EXISTS run_status_tag (
                 machine_key TEXT NOT NULL REFERENCES run_status(machine_key) ON DELETE CASCADE,
                 key         TEXT NOT NULL,
                 value       TEXT NOT NULL,
                 PRIMARY KEY (machine_key, key)
             );",
        )?;
        add_missing_columns(&conn)?;
        conn.pragma_update(None, "user_version", INDEX_VERSION)?;
        Ok(Self { conn, archive })
    }

    /// Walk `root` and ingest every result file under it.
    ///
    /// A file that won't parse is recorded and skipped, never fatal: one
    /// corrupt or truncated upload on a share of ten thousand must not stop the
    /// other 9 999 being indexed.
    pub fn scan(&mut self, root: &Path) -> Result<ScanReport> {
        self.scan_inner(root, false)
    }

    /// Scan, re-reading even the documents already indexed.
    ///
    /// For picking up a field this build records and the one that indexed these
    /// runs did not. Ordinary scanning skips an unchanged file by design, so
    /// without this a new column stays `NULL` until every machine happens to
    /// produce a new result.
    pub fn rescan_all(&mut self, root: &Path) -> Result<ScanReport> {
        self.scan_inner(root, true)
    }

    fn scan_inner(&mut self, root: &Path, reindex: bool) -> Result<ScanReport> {
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
            match self.ingest_path(path, reindex) {
                Ok(Ingested::Run(true)) => {
                    report.seen += 1;
                    report.ingested += 1;
                }
                Ok(Ingested::Run(false)) => {
                    report.seen += 1;
                    report.unchanged += 1;
                }
                Ok(Ingested::Status) => report.statuses += 1,
                Err(e) => {
                    report.seen += 1;
                    report
                        .rejected
                        .push((path.display().to_string(), format!("{e:#}")));
                }
            }
        }
        Ok(report)
    }

    /// Read one file of the collection folder as whichever document it is.
    /// With `reindex`, what is indexed from these same bytes is dropped
    /// first, so it is rebuilt rather than skipped.
    ///
    /// The ordinary path short-circuits on the content hash, which is what
    /// makes a rescan of ten thousand files cheap — and also means a *new
    /// column* is never filled in for a document that has not changed. The
    /// rebuild is the way to pick one up: the run is deleted and rebuilt from
    /// the same bytes, so nothing about it changes except what this build now
    /// records.
    fn ingest_path(&mut self, path: &Path, reindex: bool) -> Result<Ingested> {
        let text = read_document(path)?;
        let hash = content_hash(&text);
        if reindex {
            // Children go with a run: `run` is the parent of component,
            // subtest, tag, drive and memory_module, all ON DELETE CASCADE.
            self.conn
                .execute("DELETE FROM run WHERE content_hash = ?1", params![hash])?;
            self.conn.execute(
                "DELETE FROM run_status WHERE content_hash = ?1",
                params![hash],
            )?;
        }
        self.ingest_document(&text, path, &hash)
    }

    fn ingest_document(&mut self, text: &str, source: &Path, hash: &str) -> Result<Ingested> {
        // Known by its bytes, as either kind, before anything is parsed: the
        // rescan of a share is mostly this.
        let known_run: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM run WHERE content_hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .optional()?;
        if known_run.is_some() {
            return Ok(Ingested::Run(false));
        }
        let known_status: Option<String> = self
            .conn
            .query_row(
                "SELECT machine_key FROM run_status WHERE content_hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .optional()?;
        if known_status.is_some() {
            return Ok(Ingested::Status);
        }
        match Document::from_json(text).with_context(|| format!("parsing {}", source.display()))? {
            Document::Result(doc) => {
                self.keep(text, hash)?;
                self.insert(&doc, source, hash)?;
                Ok(Ingested::Run(true))
            }
            Document::Status(status) => {
                self.ingest_status(&status, source, hash)?;
                Ok(Ingested::Status)
            }
        }
    }

    /// Keep a run status record if it is the newest held for its machine.
    ///
    /// One row per machine, replaced by a newer record and left alone by an
    /// older or equal one, so scanning a folder holding several files from one
    /// machine ends on the newest whatever order they are met in. Timestamps
    /// are compared as instants, not text. Returns whether the row changed.
    fn ingest_status(&mut self, st: &RunStatus, source: &Path, hash: &str) -> Result<bool> {
        let key = st.machine_key().value;
        let at = st.timestamp.trim();
        let held: Option<String> = self
            .conn
            .query_row(
                "SELECT at FROM run_status WHERE machine_key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(held) = held
            && let (Some(held), Some(new)) = (status_instant(&held), status_instant(at))
            && new <= held
        {
            return Ok(false);
        }
        let tx = self.conn.transaction()?;
        // Its tags go with it, by the cascade.
        tx.execute(
            "DELETE FROM run_status WHERE machine_key = ?1",
            params![key],
        )?;
        tx.execute(
            "INSERT INTO run_status
                 (machine_key, hostname, status, reason, detail, at, tool_version,
                  source_path, content_hash, ingested_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))",
            params![
                key,
                present(st.hostname.as_deref()),
                present(st.status.as_deref()),
                present(st.reason.as_deref()),
                present(st.detail.as_deref()),
                at,
                present(st.tool_version.as_deref()),
                source.display().to_string(),
                hash,
            ],
        )?;
        for (k, v) in &st.tags {
            tx.execute(
                "INSERT OR REPLACE INTO run_status_tag (machine_key, key, value)
                 VALUES (?1, ?2, ?3)",
                params![key, k, v],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Take a run status record submitted through the dashboard.
    ///
    /// Written to `dir` first, as one file per machine under a name made from
    /// the machine key's hash alone ([`status_name`]) — never from anything
    /// the sender chose — and replaced by a newer record through a temporary
    /// file and a rename, like a result. A record not newer than the file
    /// already there is dropped, and the sender is told it was recorded: the
    /// answer to "the server has this or something better" is yes. Nothing is
    /// archived: the archive is for results, and a status record is replaced.
    ///
    /// Returns whether the record replaced what was held.
    pub fn accept_status(&mut self, status: &RunStatus, text: &str, dir: &Path) -> Result<bool> {
        let key = status.machine_key();
        let target = dir.join(status_name(&key.value));
        if let Ok(held) = std::fs::read_to_string(&target)
            && let Ok(held) = RunStatus::from_json(&held)
            && let (Some(held), Some(new)) = (
                status_instant(&held.timestamp),
                status_instant(&status.timestamp),
            )
            && new <= held
        {
            return Ok(false);
        }
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating the upload directory {}", dir.display()))?;
        let hash = content_hash(text);
        let partial = dir.join(format!(
            "{hash}.{}-{}.partial",
            std::process::id(),
            PARTIAL.fetch_add(1, Ordering::Relaxed)
        ));
        write_new(&partial, text)?;
        std::fs::rename(&partial, &target)
            .with_context(|| format!("moving {} into place", partial.display()))?;
        self.ingest_status(status, &target, &hash)
    }

    /// Returns whether the file was new. `false` means it was already indexed.
    /// A result only: the scan reads a file through `ingest_path`, which also
    /// takes a run status record, so this is what the tests use.
    #[cfg(test)]
    pub fn ingest_file(&mut self, path: &Path) -> Result<bool> {
        let text = read_document(path)?;
        self.ingest_text(&text, path)
    }

    /// Take a document submitted through the dashboard.
    ///
    /// It becomes a file in `dir` **before** it is indexed, named from the
    /// document itself — never from anything the uploader sent, which is what
    /// makes a filename impossible to weaponise. See [`upload_name`].
    /// Configuration requires `dir` to sit inside the collection folder, so
    /// what lands there is an ordinary result file: the next scan finds it,
    /// and rebuilding the index from the folder keeps it. An upload that
    /// existed only in the index would be destroyed by the rebuild this
    /// project calls safe.
    ///
    /// Returns `false` when the document was already indexed, which is not an
    /// error. The same result submitted twice is a question already answered,
    /// and saying so is more useful than either a duplicate or a failure.
    ///
    /// **`uploaded_by` does not survive a rebuild**, and cannot: once the
    /// index is deleted and the folder rescanned, the run arrived as a file
    /// like any other and nothing in the document says who sent it. The column
    /// is a convenience for the drilldown. The durable record is the log line
    /// written here, which names the subject and the machine.
    pub fn accept_upload(&mut self, text: &str, dir: &Path, by: &str) -> Result<bool> {
        // Parsed before anything is written, so a document that is not a
        // result never reaches the disk at all.
        let doc = ResultFile::from_json(text).context("this is not a loadbearer result file")?;
        let hash = content_hash(text);

        let target = dir.join(upload_name(&doc, &hash));
        if !target.exists() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating the upload directory {}", dir.display()))?;
            // Temporary name, then rename: a failed write cannot leave a
            // truncated file behind whose name is a claim about its contents.
            //
            // The name carries the process and a counter as well as the hash,
            // so two callers submitting the same document at the same moment
            // do not write to one file and then race to rename it. It keeps
            // the `.partial` suffix, which a scan ignores — it takes `.json`
            // and `.json.gz` and nothing else.
            let partial = dir.join(format!(
                "{hash}.{}-{}.partial",
                std::process::id(),
                PARTIAL.fetch_add(1, Ordering::Relaxed)
            ));
            write_new(&partial, text)?;
            std::fs::rename(&partial, &target)
                .with_context(|| format!("moving {} into place", partial.display()))?;
        }

        let fresh = self.ingest_text(text, &target)?;
        if fresh {
            // Recorded afterwards rather than threaded through `insert`, which
            // is shared with the scan path where there is no uploader.
            self.conn.execute(
                "UPDATE run SET uploaded_by = ?1, uploaded_at = datetime('now')
                   WHERE content_hash = ?2",
                params![by, hash],
            )?;
        }
        Ok(fresh)
    }

    /// Ingest a document already in memory, recording `source` as where it came
    /// from. Splitting this out from `ingest_file` keeps the identity of a run
    /// tied to its *content* rather than to having been read off a disk.
    pub fn ingest_text(&mut self, text: &str, source: &Path) -> Result<bool> {
        // The bytes, never the document's own `run.id`: see `insert` for why
        // the run ID is recorded but is no identity.
        let hash = content_hash(text);

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
        let age = doc.machine.age.clone().unwrap_or_default();
        let drive = doc.machine.drives.first();
        let power = doc.machine.power.clone().unwrap_or_default();
        let firmware = doc.machine.firmware.clone().unwrap_or_default();
        let fullest = doc.machine.fullest_fixed_volume();
        // Recorded, and deliberately **not** an identity: the content hash is
        // what makes two documents the same run, here and in `ingest_text`.
        // The ID is something the document says about itself — a copy edited
        // by hand, or a re-score, keeps it — so keying on it would let one
        // document replace another that differs in what it measured.
        let run = doc.run.clone().unwrap_or_default();
        let os = doc.machine.os_detail.clone().unwrap_or_default();
        let memory = doc.machine.memory.clone().unwrap_or_default();
        // The opt-in facts, each recorded only when its category is in
        // `run.collect`. No CLI writes a block it was not asked for, so one
        // without its name is a document something else made; recording it
        // would put a value where the index promises "not asked", and every
        // view downstream reads the three states from these columns alone.
        let asked = Collected::of(&run.collect);
        let boot = doc
            .machine
            .boot
            .clone()
            .filter(|_| asked.has("uptime"))
            .unwrap_or_default();
        let security = doc
            .machine
            .security
            .clone()
            .filter(|_| asked.has("security"))
            .unwrap_or_default();
        let tpm = security.tpm.clone().unwrap_or_default();
        let disk = security.disk_encryption.clone().unwrap_or_default();
        // The fixed vocabularies, lower-cased so a filter matches however a
        // future CLI capitalises them; a value never heard of is kept.
        let word = |s: Option<&str>| present(s).map(str::to_lowercase);
        let tx = self.conn.transaction()?;

        tx.execute(
            "INSERT INTO run (
                 machine_key, key_kind, hostname, taken_at, tool_version, preset, profile,
                 baseline, build_isa, overall_score, overall_grade, cpu_model, cpu_cores,
                 ram_bytes, os, arch, serial, asset_tag, on_ac, thermal_limited,
                 battery_health, battery_cycles, partial, caveats, source_path,
                 content_hash, system_model, cpu_family, cpu_launched, max_age_months,
                 firmware_dated, firmware_likely_original, drive_power_on_hours,
                 drive_wear_pct, drive_hours_undercounted, power_plan, power_mode,
                 power_held_back, bios_version, bios_date, disk_low_mount,
                 disk_low_free_pct, disk_low_free_bytes, run_uuid, run_started_at,
                 run_duration_secs, os_distro, os_version, os_build, os_edition,
                 memory_slots, memory_populated, run_collect, run_elevated, booted_at,
                 uptime_secs, firmware_mode, secure_boot, tpm_present, tpm_version,
                 disk_encryption, disk_encryption_method, ingested_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,
                       ?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,
                       ?35,?36,?37,?38,?39,?40,?41,?42,?43,?44,?45,?46,?47,?48,?49,?50,
                       ?51,?52,?53,?54,?55,?56,?57,?58,?59,?60,?61,?62, datetime('now'))",
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
                doc.on_ac(),
                doc.telemetry.as_ref().map(|t| t.thermal_limited),
                doc.machine.battery.as_ref().and_then(|b| b.health_pct),
                doc.machine.battery.as_ref().and_then(|b| b.cycle_count),
                doc.is_partial(),
                caveats,
                path.display().to_string(),
                hash,
                doc.machine.firmware.as_ref().and_then(|f| f.system_name()),
                age.cpu_family,
                age.cpu_launched,
                age.max_age_months,
                age.firmware_dated,
                age.firmware_likely_original,
                drive.map(|d| d.power_on_hours as i64),
                drive.and_then(|d| d.percentage_used),
                age.drive_hours_undercounted,
                power.plan,
                power.mode_in_force(),
                Some(power.held_back.join(" | ")).filter(|h| !h.is_empty()),
                firmware.bios_version,
                firmware.bios_date,
                fullest.map(|(d, _)| d.mount_point.as_str()),
                fullest.map(|(_, pct)| pct),
                fullest.map(|(d, _)| saturating(d.available_bytes)),
                present(run.id.as_deref()),
                present(run.started_at.as_deref()),
                // A negative span is a clock that went wrong, not a duration.
                run.duration_secs.filter(|s| s.is_finite() && *s >= 0.0),
                present(os.distro.as_deref()),
                present(os.version.as_deref()),
                present(os.build.as_deref()),
                present(os.edition.as_deref()),
                memory.slots,
                memory.populated,
                asked.text(),
                run.elevated.filter(|_| asked.has("elevation")),
                present(boot.booted_at.as_deref()),
                boot.uptime_secs.map(saturating),
                word(security.firmware_mode.as_deref()),
                security.secure_boot,
                tpm.present,
                present(tpm.version.as_deref()),
                word(disk.system_volume.as_deref()),
                word(disk.method.as_deref()),
            ],
        )?;
        let run_id = tx.last_insert_rowid();

        for (position, m) in memory.modules.iter().enumerate() {
            tx.execute(
                "INSERT INTO memory_module
                   (run_id, position, size_bytes, type, form_factor, speed_mts,
                    configured_speed_mts)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    run_id,
                    position as i64,
                    m.size_bytes.map(saturating),
                    present(m.kind.as_deref()),
                    present(m.form_factor.as_deref()),
                    m.speed_mts,
                    m.configured_speed_mts,
                ],
            )?;
        }

        for (position, d) in doc.machine.drives.iter().enumerate() {
            tx.execute(
                "INSERT INTO drive
                   (run_id, position, device, interface, model, power_on_hours, power_cycles,
                    unsafe_shutdowns, percentage_used, available_spare_pct, data_written_tb,
                    media_errors, critical_warning)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    run_id,
                    position as i64,
                    d.device,
                    d.interface,
                    d.model.as_deref().map(str::trim).filter(|m| !m.is_empty()),
                    saturating(d.power_on_hours),
                    d.power_cycles.map(saturating),
                    d.unsafe_shutdowns.map(saturating),
                    d.percentage_used,
                    d.available_spare_pct,
                    d.data_written_tb,
                    d.media_errors.map(saturating),
                    d.critical_warning,
                ],
            )?;
        }

        for c in &doc.components {
            tx.execute(
                "INSERT INTO component (run_id, id, score, grade, graded, label)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    run_id,
                    c.id,
                    c.score,
                    c.grade,
                    c.graded,
                    Some(c.label.as_str()).filter(|l| !l.is_empty()),
                ],
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
                        representative, scored, label, direction)
                     VALUES (?1,?2,?3,?4,?5,NULL,NULL,?6,?7,?8,?9,?10,?11)
                     ON CONFLICT(run_id, component, id) DO UPDATE SET
                       cv = excluded.cv,
                       representative = excluded.representative,
                       scored = excluded.scored,
                       label = excluded.label,
                       direction = excluded.direction",
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
                        // Empty is not a label; a missing one should read as
                        // missing rather than as a subtest called "".
                        Some(s.label.as_str()).filter(|l| !l.is_empty()),
                        s.direction.as_deref(),
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
        //
        // A machine known only from its run status records is as forgettable
        // as one with runs, so the keys come from both tables.
        let mut stmt = self.conn.prepare(
            "SELECT machine_key FROM run
               WHERE machine_key = ?1 COLLATE NOCASE OR hostname = ?1 COLLATE NOCASE
             UNION
             SELECT machine_key FROM run_status
               WHERE machine_key = ?1 COLLATE NOCASE OR hostname = ?1 COLLATE NOCASE
             ORDER BY machine_key",
        )?;
        let keys = stmt
            .query_map([needle], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(keys.len());
        for key in keys {
            let (hostname, runs): (Option<String>, i64) = self.conn.query_row(
                "SELECT hostname, COUNT(*) FROM run WHERE machine_key = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let hostname = match hostname {
                Some(h) => Some(h),
                None => self
                    .conn
                    .query_row(
                        "SELECT hostname FROM run_status WHERE machine_key = ?1",
                        [&key],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten(),
            };
            rows.push(Forgettable {
                key,
                hostname,
                runs,
            });
        }
        Ok(rows)
    }

    /// Machines `forget` could be asked about: those with a run, and those
    /// known only from a run status record.
    pub fn forgettable_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM (SELECT machine_key FROM run
                                   UNION SELECT machine_key FROM run_status)",
            [],
            |r| r.get(0),
        )?)
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
        // The run status record: a row, and a file in the collection folder
        // that is reported like a result file, because it brings the machine
        // back to the list of those tried and never measured all the same.
        let status_source: Option<String> = self
            .conn
            .query_row(
                "SELECT source_path FROM run_status WHERE machine_key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(source) = status_source {
            plan.statuses = 1;
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
        if plan.runs == 0 && plan.statuses == 0 {
            return Ok(plan);
        }

        // The child tables declare ON DELETE CASCADE and `foreign_keys` is on,
        // so components, subtests, tags, drives and memory modules go with
        // their runs.
        let removed = self
            .conn
            .execute("DELETE FROM run WHERE machine_key = ?1", [key])?;
        debug_assert_eq!(removed as i64, plan.runs);
        // Its tags go with it.
        self.conn
            .execute("DELETE FROM run_status WHERE machine_key = ?1", [key])?;

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

    /// An uploaded file is named from the *document*, never from the uploader.
    ///
    /// The hostname is attacker-controlled text, so this is the one place a
    /// submitted result could reach outside the directory it belongs in. It
    /// cannot: every character that is not plainly a filename becomes a
    /// hyphen, which leaves a name that is readable and inert.
    /// The temporary file is *created*, never opened. Anything already at
    /// that path is somebody else's, and writing through it is how a service
    /// account gets used to put a chosen file somewhere it did not choose.
    #[test]
    fn a_temporary_file_is_never_written_through_something_already_there() {
        let dir = std::env::temp_dir().join(format!("lbf-writenew-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");

        let taken = dir.join("taken");
        std::fs::write(&taken, "not yours").expect("plant");
        assert!(
            write_new(&taken, "mine").is_err(),
            "an existing file is not a place to write"
        );
        assert_eq!(
            std::fs::read_to_string(&taken).expect("read"),
            "not yours",
            "and it is left exactly as it was"
        );

        // The case that matters: a symlink. `fs::write` follows one and lands
        // on whatever it points at; this must not.
        #[cfg(unix)]
        {
            let elsewhere = dir.join("elsewhere");
            std::fs::write(&elsewhere, "untouched").expect("target");
            let planted = dir.join("planted");
            std::os::unix::fs::symlink(&elsewhere, &planted).expect("symlink");
            assert!(
                write_new(&planted, "mine").is_err(),
                "a symlink is not ours"
            );
            assert_eq!(
                std::fs::read_to_string(&elsewhere).expect("read"),
                "untouched",
                "nothing was written through it"
            );
        }

        let fresh = dir.join("fresh");
        write_new(&fresh, "mine").expect("a new path is fine");
        assert_eq!(std::fs::read_to_string(&fresh).expect("read"), "mine");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_uploaded_file_is_named_from_the_document_and_cannot_escape_its_directory() {
        let mut doc =
            ResultFile::from_json(&std::fs::read_to_string(fixture("win-modern.json")).unwrap())
                .unwrap();

        let name = upload_name(&doc, "8f3ac1d2deadbeef");
        assert!(name.starts_with("FLEET-WIN-01-"), "readable: {name}");
        assert!(
            name.ends_with("-8f3ac1d2.json"),
            "and tied to its bytes: {name}"
        );

        for hostile in [
            "../../etc/cron.d/x",
            "..\\..\\windows\\system32\\evil",
            "/etc/passwd",
            "con",
            "",
            "   ",
            "a".repeat(400).as_str(),
        ] {
            doc.machine.hostname = Some(hostile.to_string());
            let name = upload_name(&doc, "8f3ac1d2deadbeef");
            assert!(!name.contains('/'), "{hostile:?} produced {name}");
            assert!(!name.contains('\\'), "{hostile:?} produced {name}");
            assert!(!name.contains(".."), "{hostile:?} produced {name}");
            assert!(!name.starts_with('.'), "{hostile:?} produced {name}");
            assert!(name.ends_with(".json"), "{hostile:?} produced {name}");
            assert!(name.len() < 80, "{hostile:?} produced {} chars", name.len());
            // And the path it builds stays where it was put.
            let dir = Path::new("/srv/collection/uploaded");
            assert_eq!(
                dir.join(&name).parent(),
                Some(dir),
                "{hostile:?} escaped to {name}"
            );
        }
    }

    /// `direction` is what makes two measurements comparable — without it a
    /// lower-is-better latency reads as a loss when it improves. It is in the
    /// document's `raw` block, so the only question is whether ingest keeps it.
    #[test]
    fn direction_and_label_survive_ingest() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("win-modern.json")).unwrap();

        let (dir, label): (Option<String>, Option<String>) = idx
            .conn
            .query_row(
                "SELECT direction, label FROM subtest WHERE id = 'int_single'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(dir.as_deref(), Some("higher_is_better"));
        assert_eq!(label.as_deref(), Some("Integer, single-core"));

        // And a lower-is-better one, since that is the case the column exists
        // for: without it, an improvement in latency looks like a regression.
        let lower: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM subtest WHERE direction = 'lower_is_better'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(lower > 0, "no lower-is-better subtest was recorded");
    }

    /// The migration, and the reason `--reindex` has to exist.
    ///
    /// An index written before these columns existed keeps its rows, gains the
    /// columns as `NULL`, and — because ingest short-circuits on the content
    /// hash — stays `NULL` through any number of ordinary rescans. Only a
    /// re-read fills it in.
    #[test]
    fn an_older_index_gains_the_columns_but_only_a_reindex_fills_them() {
        let dir = std::env::temp_dir().join(format!("lbf-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("i.db");
        let collection = dir.join("collection");
        std::fs::create_dir_all(&collection).unwrap();
        std::fs::copy(fixture("win-modern.json"), collection.join("win.json")).unwrap();

        // Build an index, then take the new columns away again: this is what a
        // database written by 0.5.2 or earlier looks like.
        {
            let mut idx = Index::open(&db).unwrap();
            idx.scan(&collection).unwrap();
            idx.conn
                .execute_batch("ALTER TABLE subtest DROP COLUMN direction")
                .unwrap();
        }

        let mut idx = Index::open(&db).unwrap();
        let nulls: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM subtest WHERE direction IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(nulls > 0, "the column should be back, and empty");

        // An ordinary rescan sees nothing to do — the bytes have not changed.
        let report = idx.scan(&collection).unwrap();
        assert_eq!(report.unchanged, 1);
        assert_eq!(report.ingested, 0);
        let still: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM subtest WHERE direction IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still, nulls, "a plain rescan cannot fill a new column");

        // A reindex rebuilds the run from the same document.
        let report = idx.rescan_all(&collection).unwrap();
        assert_eq!(report.ingested, 1, "the document should be re-read");
        let left: i64 = idx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM subtest WHERE direction IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "every subtest should now carry its direction");
        assert_eq!(idx.run_count().unwrap(), 1, "and not be duplicated");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real fixture with loadbearer 1.6.0's firmware and two drives added,
    /// since none of the fixtures was taken with admin rights on 1.6.0.
    fn with_drives() -> String {
        let text = std::fs::read_to_string(fixture("win-modern.json")).unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        doc["machine"]["firmware"] = serde_json::json!({
            "system_vendor": "LENOVO", "system_model": "20XW004BUK",
            "bios_version": "N32ET91W (1.67)", "bios_date": "2025-06-01"
        });
        doc["machine"]["drives"] = serde_json::json!([
            {"device": "PhysicalDrive0", "interface": "nvme", "model": "  Boot SSD  ",
             "power_on_hours": 12881, "unsafe_shutdowns": 195, "percentage_used": 19,
             "available_spare_pct": 100, "media_errors": 0, "critical_warning": 0},
            {"device": "PhysicalDrive1", "interface": "sata", "power_on_hours": 40000}
        ]);
        doc.to_string()
    }

    fn drive_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM drive", [], |r| r.get(0))
            .unwrap()
    }

    /// `with_drives`, plus the run, OS-detail and memory blocks of a
    /// loadbearer release after 1.8.1, Windows-shaped, and the opt-in blocks
    /// of a run made with `--collect uptime,elevation,security`.
    fn with_everything() -> String {
        let mut doc: serde_json::Value = serde_json::from_str(&with_drives()).unwrap();
        crate::schema::samples::windows(&mut doc);
        crate::schema::samples::optin_windows(&mut doc);
        doc.to_string()
    }

    /// The opt-in columns of the one run in an index, in `ADDED`'s order.
    type OptIn = (
        Option<String>,
        Option<bool>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<bool>,
        Option<bool>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    fn opt_in_row(conn: &Connection, run_id: i64) -> OptIn {
        conn.query_row(
            "SELECT run_collect, run_elevated, booted_at, uptime_secs, firmware_mode,
                    secure_boot, tpm_present, tpm_version, disk_encryption,
                    disk_encryption_method FROM run WHERE id = ?1",
            [run_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .unwrap()
    }

    /// Each shape the CLI writes, indexed as it says: what was asked, and
    /// NULL for what was not read — never 0 or "none" for either.
    #[test]
    fn the_opt_in_facts_are_indexed_in_their_three_states() {
        use crate::schema::samples;
        let index_one = |enrich: fn(&mut serde_json::Value)| -> OptIn {
            let mut idx = Index::open_in_memory().unwrap();
            let mut doc = samples::fixture("linux-throttled.json");
            enrich(&mut doc);
            idx.ingest_text(&doc.to_string(), Path::new("x.json"))
                .unwrap();
            let id: i64 = idx
                .conn
                .query_row("SELECT id FROM run", [], |r| r.get(0))
                .unwrap();
            opt_in_row(&idx.conn, id)
        };
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            index_one(samples::optin_windows),
            (
                s("elevation,security,uptime"),
                Some(false),
                s("2026-09-09T10:41:28Z"),
                Some(14492),
                s("uefi"),
                Some(true),
                Some(true),
                s("2.0"),
                None,
                None
            )
        );
        assert_eq!(
            index_one(samples::optin_linux_luks),
            (
                s("elevation,security,uptime"),
                Some(true),
                s("2026-09-06T08:00:00Z"),
                Some(266400),
                s("uefi"),
                Some(false),
                Some(true),
                s("2.0"),
                s("encrypted"),
                s("luks2")
            )
        );
        assert_eq!(
            index_one(samples::optin_linux_none_detected),
            (
                s("security"),
                None,
                None,
                None,
                s("uefi"),
                Some(true),
                Some(false),
                None,
                s("none_detected"),
                None
            )
        );
        let bios = index_one(samples::optin_legacy_bios);
        assert_eq!((bios.4.as_deref(), bios.5), (Some("bios"), None));
        assert_eq!(
            index_one(samples::optin_unreadable),
            (
                s("elevation,security,uptime"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None
            )
        );
        // Unknown values carried, lower-cased where they are a vocabulary.
        let future = index_one(samples::optin_future);
        assert_eq!(future.0.as_deref(), Some("network,security"));
        assert_eq!(
            (
                future.4.as_deref(),
                future.7.as_deref(),
                future.8.as_deref()
            ),
            (Some("coreboot"), Some("3.0"), Some("partial"))
        );
        // Not asked: an older run, and blocks no `run.collect` names.
        let none: OptIn = Default::default();
        assert_eq!(index_one(|_| {}), none);
        assert_eq!(index_one(samples::optin_blocks_without_collect), none);
    }

    fn module_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM memory_module", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn the_run_os_detail_and_every_memory_module_are_indexed() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_text(&with_everything(), Path::new("x.json"))
            .unwrap();
        type Row = (
            Option<String>,
            Option<String>,
            Option<f64>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        );
        let row: Row = idx
            .conn
            .query_row(
                "SELECT run_uuid, run_started_at, run_duration_secs, os_distro, os_version,
                        os_build, os_edition, memory_slots, memory_populated FROM run",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                Some("0f8b5a52-7c1e-4c39-9d0e-5a1f3b2c4d6e".into()),
                Some("2026-09-09T14:42:01.1234567Z".into()),
                Some(472.318),
                None,
                Some("24H2".into()),
                Some("26100.2314".into()),
                Some("Enterprise".into()),
                Some(4),
                Some(2),
            )
        );
        type ModuleRow = (i64, Option<i64>, Option<String>, Option<i64>, Option<i64>);
        let modules: Vec<ModuleRow> = idx
            .conn
            .prepare(
                "SELECT position, size_bytes, type, speed_mts, configured_speed_mts
                   FROM memory_module ORDER BY position",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let one = (
            Some(17_179_869_184),
            Some("DDR5".to_string()),
            Some(5600),
            Some(4800),
        );
        assert_eq!(
            modules,
            [
                (0, one.0, one.1.clone(), one.2, one.3),
                (1, one.0, one.1, one.2, one.3)
            ]
        );
    }

    /// A document without the blocks — every fixture — records nothing for
    /// them: NULL and no rows, never a zero.
    #[test]
    fn a_document_without_the_blocks_records_them_as_unknown() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("legacy-1.2.4.json")).unwrap();
        let (uuid, slots, edition): (Option<String>, Option<i64>, Option<String>) = idx
            .conn
            .query_row(
                "SELECT run_uuid, memory_slots, os_edition FROM run",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((uuid, slots, edition), (None, None, None));
        assert_eq!(module_rows(&idx.conn), 0);
    }

    /// Two documents with the same run ID are still two runs when their bytes
    /// differ — a re-score carries the block over — and one document seen
    /// twice is still one. The content hash is the identity, not the ID.
    #[test]
    fn the_run_id_is_not_what_makes_two_documents_the_same() {
        let mut idx = Index::open_in_memory().unwrap();
        let first = with_everything();
        let mut rescored: serde_json::Value = serde_json::from_str(&first).unwrap();
        rescored["overall"]["score"] = serde_json::json!(1.0);
        assert!(idx.ingest_text(&first, Path::new("a.json")).unwrap());
        assert!(
            idx.ingest_text(&rescored.to_string(), Path::new("b.json"))
                .unwrap()
        );
        assert!(!idx.ingest_text(&first, Path::new("c.json")).unwrap());
        assert_eq!(idx.run_count().unwrap(), 2);
    }

    #[test]
    fn every_drive_and_the_firmware_are_indexed() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_text(&with_drives(), Path::new("x.json"))
            .unwrap();
        let rows: Vec<(i64, String, Option<String>, Option<i64>)> = idx
            .conn
            .prepare(
                "SELECT position, device, model, unsafe_shutdowns FROM drive ORDER BY position",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (
                    0,
                    "PhysicalDrive0".to_string(),
                    Some("Boot SSD".to_string()),
                    Some(195)
                ),
                (1, "PhysicalDrive1".to_string(), None, None),
            ]
        );
        let (version, date, mount): (Option<String>, Option<String>, Option<String>) = idx
            .conn
            .query_row(
                "SELECT bios_version, bios_date, disk_low_mount FROM run",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(version.as_deref(), Some("N32ET91W (1.67)"));
        assert_eq!(date.as_deref(), Some("2025-06-01"));
        // The fixture's C:\ is 29% free and D:\ 80%.
        assert_eq!(mount.as_deref(), Some("C:\\"));
    }

    /// The same additive migration as the columns above, for a whole table:
    /// an index from before it gains the table and the columns on open, keeps
    /// its runs, and has nothing in them until a reindex.
    ///
    /// Two generations at once: what 0.9.0 lacked (the drive table, the
    /// firmware and free-space columns) and what 0.11.0 lacked (the
    /// memory-module table, the run and OS-detail columns, the memory counts)
    /// — and, a third, what 0.11.0 and the release after it lacked: the
    /// `--collect` columns.
    #[test]
    fn an_older_index_gains_the_drive_and_memory_tables_and_their_columns() {
        let dir = scratch("migrate-drives");
        let db = dir.join("i.db");
        let collection = dir.join("collection");
        std::fs::create_dir_all(&collection).unwrap();
        std::fs::write(collection.join("x1.json"), with_everything()).unwrap();

        // What an index written by 0.9.0 looks like — and, for the second
        // half of the list, one written by 0.11.0.
        {
            let mut idx = Index::open(&db).unwrap();
            idx.scan(&collection).unwrap();
            idx.conn
                .execute_batch(
                    "DROP TABLE drive;
                     ALTER TABLE run DROP COLUMN bios_version;
                     ALTER TABLE run DROP COLUMN bios_date;
                     ALTER TABLE run DROP COLUMN disk_low_mount;
                     ALTER TABLE run DROP COLUMN disk_low_free_pct;
                     ALTER TABLE run DROP COLUMN disk_low_free_bytes;

                     DROP TABLE memory_module;
                     ALTER TABLE run DROP COLUMN run_uuid;
                     ALTER TABLE run DROP COLUMN run_started_at;
                     ALTER TABLE run DROP COLUMN run_duration_secs;
                     ALTER TABLE run DROP COLUMN os_distro;
                     ALTER TABLE run DROP COLUMN os_version;
                     ALTER TABLE run DROP COLUMN os_build;
                     ALTER TABLE run DROP COLUMN os_edition;
                     ALTER TABLE run DROP COLUMN memory_slots;
                     ALTER TABLE run DROP COLUMN memory_populated;

                     ALTER TABLE run DROP COLUMN run_collect;
                     ALTER TABLE run DROP COLUMN run_elevated;
                     ALTER TABLE run DROP COLUMN booted_at;
                     ALTER TABLE run DROP COLUMN uptime_secs;
                     ALTER TABLE run DROP COLUMN firmware_mode;
                     ALTER TABLE run DROP COLUMN secure_boot;
                     ALTER TABLE run DROP COLUMN tpm_present;
                     ALTER TABLE run DROP COLUMN tpm_version;
                     ALTER TABLE run DROP COLUMN disk_encryption;
                     ALTER TABLE run DROP COLUMN disk_encryption_method;",
                )
                .unwrap();
        }

        let later = |idx: &Index| -> (Option<String>, Option<String>, Option<f64>, Option<i64>) {
            idx.conn
                .query_row(
                    "SELECT bios_date, os_build, run_duration_secs, memory_slots FROM run",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap()
        };

        let mut idx = Index::open(&db).unwrap();
        assert_eq!(idx.run_count().unwrap(), 1, "the history is kept");
        assert_eq!(drive_rows(&idx.conn), 0, "the table is back, and empty");
        assert_eq!(module_rows(&idx.conn), 0, "this one too");
        assert_eq!(later(&idx), (None, None, None, None));
        let none: OptIn = Default::default();
        assert_eq!(
            opt_in_row(&idx.conn, 1),
            none,
            "not asked, as far as it knows"
        );
        // And the views read an index in that state without stumbling.
        let snap = crate::analytics::snapshot(
            &idx.conn,
            &crate::analytics::Tuning::default(),
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        assert_eq!(snap.machines[0].memory_slots, None);
        assert!(snap.machines[0].run_collect.is_empty());
        assert_eq!(snap.machines[0].secure_boot, None);

        idx.scan(&collection).unwrap();
        assert_eq!(drive_rows(&idx.conn), 0, "a plain rescan cannot fill it");
        assert_eq!(module_rows(&idx.conn), 0);
        assert_eq!(later(&idx), (None, None, None, None));
        assert_eq!(opt_in_row(&idx.conn, 1), none);

        idx.rescan_all(&collection).unwrap();
        assert_eq!(drive_rows(&idx.conn), 2);
        assert_eq!(module_rows(&idx.conn), 2);
        assert_eq!(
            later(&idx),
            (
                Some("2025-06-01".into()),
                Some("26100.2314".into()),
                Some(472.318),
                Some(4)
            )
        );
        assert_eq!(idx.run_count().unwrap(), 1, "and not duplicated");
        let id: i64 = idx
            .conn
            .query_row("SELECT id FROM run", [], |r| r.get(0))
            .unwrap();
        let filled = opt_in_row(&idx.conn, id);
        assert_eq!(filled.0.as_deref(), Some("elevation,security,uptime"));
        assert_eq!(
            (filled.1, filled.5, filled.8),
            (Some(false), Some(true), None)
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_a_machine_takes_its_drive_and_memory_rows_and_its_posture() {
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_text(&with_everything(), Path::new("x.json"))
            .unwrap();
        // Another machine with modules of its own, which must stay.
        let mut other = crate::schema::samples::fixture("low-end.json");
        crate::schema::samples::linux_root(&mut other);
        crate::schema::samples::optin_linux_luks(&mut other);
        idx.ingest_text(&other.to_string(), Path::new("y.json"))
            .unwrap();
        assert_eq!(drive_rows(&idx.conn), 2);
        assert_eq!(module_rows(&idx.conn), 3);

        let key = idx.machines_matching("FLEET-WIN-01").unwrap()[0]
            .key
            .clone();
        idx.forget(&key).unwrap();
        assert_eq!(drive_rows(&idx.conn), 0);
        assert_eq!(
            module_rows(&idx.conn),
            1,
            "only the other machine's module is left"
        );
        assert_eq!(idx.run_count().unwrap(), 1);
        // Its posture went with its runs — the columns are on `run` — and
        // the other machine's stays.
        let left: Vec<(String, Option<bool>)> = idx
            .conn
            .prepare("SELECT hostname, secure_boot FROM run WHERE run_collect IS NOT NULL")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(left, [("FLEET-LNX-02".to_string(), Some(false))]);
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
        for table in ["component", "subtest", "tag", "drive"] {
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
    // --- run status records ------------------------------------------------

    /// A run status record for a machine known only by hostname, so it shares
    /// no key with any fixture result.
    fn status_for(host: &str, at: &str, reason: &str) -> String {
        let mut v = crate::schema::samples::run_status();
        v.as_object_mut().unwrap().remove("identity");
        v["hostname"] = serde_json::json!(host);
        v["timestamp"] = serde_json::json!(at);
        v["reason"] = serde_json::json!(reason);
        v.to_string()
    }

    fn status_rows(idx: &Index) -> Vec<(String, String, String)> {
        let mut stmt = idx
            .conn
            .prepare("SELECT machine_key, at, reason FROM run_status ORDER BY machine_key")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn a_scan_reads_a_status_file_into_its_own_table_and_count_and_does_not_reject_it() {
        let dir = scratch("status-scan");
        let results = dir.join("results");
        std::fs::create_dir_all(&results).unwrap();
        std::fs::copy(fixture("win-modern.json"), results.join("PC-01.json")).unwrap();
        std::fs::write(
            results.join("dropped-by-a-collector.json"),
            status_for("PC-77", "2026-09-10T22:00:00Z", "busy"),
        )
        .unwrap();

        let mut idx = Index::open_in_memory().unwrap();
        let report = idx.scan(&results).unwrap();
        assert_eq!(report.statuses, 1);
        assert_eq!((report.seen, report.ingested, report.unchanged), (1, 1, 0));
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        assert_eq!(idx.run_count().unwrap(), 1, "it is not a run");
        assert_eq!(status_rows(&idx).len(), 1);

        // Idempotent: the same file again is no new row and not a rejection.
        let again = idx.scan(&results).unwrap();
        assert_eq!(again.statuses, 1);
        assert_eq!((again.seen, again.ingested, again.unchanged), (1, 0, 1));
        assert!(again.rejected.is_empty());
        assert_eq!(status_rows(&idx).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reindexing_rebuilds_the_status_table_from_the_folder() {
        let dir = scratch("status-reindex");
        std::fs::write(
            dir.join("a.json"),
            status_for("PC-77", "2026-09-10T22:00:00Z", "busy"),
        )
        .unwrap();
        let mut idx = Index::open_in_memory().unwrap();
        idx.scan(&dir).unwrap();

        // A row this build once wrote differently: a plain scan leaves it,
        // because the bytes are unchanged, and a reindex corrects it.
        idx.conn
            .execute("UPDATE run_status SET reason = 'stale-shape'", [])
            .unwrap();
        idx.scan(&dir).unwrap();
        assert_eq!(status_rows(&idx)[0].2, "stale-shape");
        let report = idx.rescan_all(&dir).unwrap();
        assert_eq!(report.statuses, 1);
        assert_eq!(status_rows(&idx)[0].2, "busy");
        assert_eq!(status_rows(&idx).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newest_record_per_machine_wins_whatever_order_the_files_are_met_in() {
        let dir = scratch("status-newest");
        std::fs::write(
            dir.join("a-newer.json"),
            status_for("PC-77", "2026-09-10T22:00:00Z", "busy"),
        )
        .unwrap();
        std::fs::write(
            dir.join("z-older.json"),
            status_for("PC-77", "2026-09-09T22:00:00Z", "on_battery"),
        )
        .unwrap();
        // Offsets are compared as instants: this one is the earliest of the three.
        std::fs::write(
            dir.join("m-offset.json"),
            status_for("PC-77", "2026-09-10T23:30:00+05:00", "on_battery"),
        )
        .unwrap();
        let mut idx = Index::open_in_memory().unwrap();
        idx.scan(&dir).unwrap();
        let rows = status_rows(&idx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "busy");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_uploaded_status_record_is_one_file_per_machine_replaced_by_a_newer_one() {
        let dir = scratch("status-upload");
        let uploads = dir.join("uploads");
        let archive = dir.join("archive");
        let mut idx = Index::open_in_memory()
            .unwrap()
            .with_archive(Some(&archive))
            .unwrap();
        let accept = |idx: &mut Index, host: &str, at: &str, reason: &str| {
            let text = status_for(host, at, reason);
            let status = crate::schema::RunStatus::from_json(&text).unwrap();
            idx.accept_status(&status, &text, &uploads).unwrap()
        };

        assert!(accept(&mut idx, "PC-77", "2026-09-10T22:00:00Z", "busy"));
        let files = |d: &Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            names
        };
        assert_eq!(files(&uploads), [status_name("PC-77")]);
        assert!(status_name("PC-77").starts_with("status-"));

        // Newer: replaces, still one file, and the file holds the newer text.
        assert!(accept(
            &mut idx,
            "PC-77",
            "2026-09-11T22:00:00Z",
            "on_battery"
        ));
        assert_eq!(files(&uploads).len(), 1);
        let held = std::fs::read_to_string(uploads.join(status_name("PC-77"))).unwrap();
        assert!(held.contains("on_battery"));
        assert_eq!(status_rows(&idx)[0].2, "on_battery");

        // Older, and equal: acknowledged and dropped, file and row untouched.
        assert!(!accept(&mut idx, "PC-77", "2026-09-10T01:00:00Z", "busy"));
        assert!(!accept(
            &mut idx,
            "PC-77",
            "2026-09-11T22:00:00Z",
            "on_battery"
        ));
        assert_eq!(
            std::fs::read_to_string(uploads.join(status_name("PC-77"))).unwrap(),
            held
        );
        assert_eq!(status_rows(&idx)[0].2, "on_battery");

        // Another machine, another file. Nothing is archived, and no partial
        // file is left behind.
        accept(&mut idx, "PC-78", "2026-09-10T22:00:00Z", "busy");
        assert_eq!(files(&uploads).len(), 2);
        assert!(files(&uploads).iter().all(|n| n.ends_with(".json")));
        assert!(!archive.exists() || files(&archive).is_empty());

        // And the folder alone brings the table back.
        let mut fresh = Index::open_in_memory().unwrap();
        let report = fresh.scan(&uploads).unwrap();
        assert_eq!(report.statuses, 2);
        assert_eq!(status_rows(&fresh).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_takes_a_machine_known_only_from_its_status_record() {
        let dir = scratch("status-forget");
        let source = dir.join("s.json");
        std::fs::write(&source, status_for("PC-77", "2026-09-10T22:00:00Z", "busy")).unwrap();
        let mut idx = Index::open_in_memory().unwrap();
        idx.scan(&dir).unwrap();

        let found = idx.machines_matching("pc-77").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].runs, found[0].hostname.as_deref()),
            (0, Some("PC-77"))
        );
        assert_eq!(idx.forgettable_count().unwrap(), 1);

        let plan = idx.forget_plan(&found[0].key).unwrap();
        assert_eq!((plan.runs, plan.statuses), (0, 1));
        assert_eq!(plan.sources_still_present.len(), 1, "{plan:?}");
        assert_eq!(status_rows(&idx).len(), 1, "a plan changes nothing");

        idx.forget(&found[0].key).unwrap();
        assert!(status_rows(&idx).is_empty());
        let tags: i64 = idx
            .conn
            .query_row("SELECT COUNT(*) FROM run_status_tag", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tags, 0);
        assert!(idx.machines_matching("PC-77").unwrap().is_empty());

        // Like a result file, the one in the folder brings it back until it goes.
        idx.scan(&dir).unwrap();
        assert_eq!(status_rows(&idx).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_a_machine_with_runs_takes_its_status_record_too() {
        let dir = scratch("status-forget-both");
        let mut idx = Index::open_in_memory().unwrap();
        idx.ingest_file(&fixture("win-modern.json")).unwrap();
        // The status fixture shares win-modern's identity, so one key.
        std::fs::write(
            dir.join("s.json"),
            crate::schema::samples::run_status().to_string(),
        )
        .unwrap();
        idx.scan(&dir).unwrap();
        let found = idx.machines_matching("FLEET-WIN-01").unwrap();
        assert_eq!(found.len(), 1, "one machine, not two");
        assert_eq!(found[0].runs, 1);

        let plan = idx.forget(&found[0].key).unwrap();
        assert_eq!((plan.runs, plan.statuses), (1, 1));
        assert_eq!(idx.run_count().unwrap(), 0);
        assert!(status_rows(&idx).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_index_from_before_run_status_gains_the_tables_and_keeps_its_history() {
        let dir = scratch("status-migrate");
        let db = dir.join("i.db");
        {
            let mut idx = Index::open(&db).unwrap();
            idx.ingest_file(&fixture("win-modern.json")).unwrap();
            idx.conn
                .execute_batch("DROP TABLE run_status_tag; DROP TABLE run_status;")
                .unwrap();
        }
        let mut idx = Index::open(&db).unwrap();
        assert_eq!(idx.run_count().unwrap(), 1, "the history is kept");
        assert!(status_rows(&idx).is_empty(), "the table is back, and empty");
        let text = status_for("PC-77", "2026-09-10T22:00:00Z", "busy");
        let status = crate::schema::RunStatus::from_json(&text).unwrap();
        idx.accept_status(&status, &text, &dir.join("uploads"))
            .unwrap();
        assert_eq!(status_rows(&idx).len(), 1);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
