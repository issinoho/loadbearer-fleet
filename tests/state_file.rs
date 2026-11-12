//! The state file through the commands that touch it from a terminal:
//! `report` reads it, `backup` copies it, `forget` clears a machine from it.
//!
//! The file is written here with SQL rather than through the server, and that
//! is deliberate: it pins the on-disk format of version 1. A change to the
//! tables that forgets to bump the version — and to migrate what operators
//! have already recorded — fails here rather than in somebody's estate.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EXE: &str = env!("CARGO_BIN_EXE_loadbearer-fleet");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lbf-state-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn run(args: &[&str]) -> Output {
    Command::new(EXE)
        .args(args)
        .output()
        .expect("run the binary")
}

fn stdout_of(args: &[&str]) -> String {
    let out = run(args);
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8")
}

/// An index of the fixtures, and the key of each machine by hostname.
fn indexed(dir: &Path) -> (String, Vec<(String, String)>) {
    let index = dir.join("i.db").to_str().expect("path").to_string();
    stdout_of(&[
        "--index",
        &index,
        "scan",
        fixtures().to_str().expect("path"),
    ]);
    let snap: serde_json::Value =
        serde_json::from_str(&stdout_of(&["--index", &index, "report", "--json"])).expect("json");
    let keys = snap["machines"]
        .as_array()
        .expect("machines")
        .iter()
        .map(|m| {
            (
                m["hostname"].as_str().unwrap_or_default().to_string(),
                m["key"].as_str().expect("key").to_string(),
            )
        })
        .collect();
    (index, keys)
}

fn key(keys: &[(String, String)], host: &str) -> String {
    keys.iter()
        .find(|(h, _)| h == host)
        .map(|(_, k)| k.clone())
        .expect("a fixture machine")
}

/// A version 1 state file: LNX-01's thermal finding acknowledged, LNX-02's
/// snoozed, LNX-01's partial run suppressed on that machine alone, and weak
/// identity suppressed across the fleet.
fn seed_state(path: &Path, lnx1: &str, lnx2: &str) {
    let conn = rusqlite::Connection::open(path).expect("state file");
    conn.execute_batch(
        "CREATE TABLE mark (
             machine_key TEXT NOT NULL, code TEXT NOT NULL,
             state TEXT NOT NULL CHECK (state IN ('acknowledged', 'snoozed')),
             severity TEXT NOT NULL, until TEXT, note TEXT,
             by_name TEXT NOT NULL, by_subject TEXT NOT NULL, at TEXT NOT NULL,
             PRIMARY KEY (machine_key, code));
         CREATE TABLE suppression (
             id INTEGER PRIMARY KEY, code TEXT NOT NULL, machine_key TEXT, tags TEXT,
             note TEXT, by_name TEXT NOT NULL, by_subject TEXT NOT NULL, at TEXT NOT NULL,
             CHECK (machine_key IS NULL OR tags IS NULL));
         PRAGMA application_id = 1279411795;
         PRAGMA user_version = 1;",
    )
    .expect("schema");
    let at = "2026-09-09T12:00:00Z";
    conn.execute(
        "INSERT INTO mark VALUES (?1, 'thermal_limited', 'acknowledged', 'warning', NULL,
                                  'fan on order', 'Iain', 'sub-1', ?2)",
        [lnx1, at],
    )
    .expect("mark");
    conn.execute(
        "INSERT INTO mark VALUES (?1, 'thermal_limited', 'snoozed', 'warning', '2999-01-01',
                                  NULL, 'Iain', 'sub-1', ?2)",
        [lnx2, at],
    )
    .expect("mark");
    conn.execute(
        "INSERT INTO suppression (code, machine_key, by_name, by_subject, at)
         VALUES ('partial_run', ?1, 'Iain', 'sub-1', ?2)",
        [lnx1, at],
    )
    .expect("rule");
    conn.execute(
        "INSERT INTO suppression (code, by_name, by_subject, at)
         VALUES ('weak_identity', 'Iain', 'sub-1', ?1)",
        [at],
    )
    .expect("rule");
}

fn rows(path: &Path, sql: &str) -> i64 {
    rusqlite::Connection::open(path)
        .expect("open")
        .query_row(sql, [], |r| r.get(0))
        .expect("count")
}

/// `report` shows what a server would: live findings listed, quieted ones
/// counted, and listed with what was said under `--all` and in `--json` —
/// without writing to the state file it reads.
#[test]
fn report_applies_the_state_file_and_leaves_it_alone() {
    let dir = scratch("report");
    let (index, keys) = indexed(&dir);
    let state = dir.join("i.state.db");
    seed_state(
        &state,
        &key(&keys, "FLEET-LNX-01"),
        &key(&keys, "FLEET-LNX-02"),
    );
    let before = std::fs::read(&state).expect("bytes");

    let text = stdout_of(&["--index", &index, "report"]);
    assert!(
        text.contains("4 finding(s) quieted — 1 acknowledged, 1 snoozed, 2 suppressed"),
        "{text}"
    );
    assert!(text.contains("pass --all to list them"), "{text}");
    assert!(
        !text.contains("fan on order"),
        "quieted findings are counted, not listed: {text}"
    );
    // One live warning left: LNX-02's partial run.
    assert!(
        text.contains("1 machine(s) need attention — 0 critical, 1 warning"),
        "{text}"
    );

    let all = stdout_of(&["--index", &index, "report", "--all"]);
    assert!(
        all.contains("acknowledged by Iain on 2026-09-09: fan on order"),
        "{all}"
    );
    assert!(
        all.contains("snoozed by Iain on 2026-09-09 until 2999-01-01"),
        "{all}"
    );
    assert!(all.contains("fleet-wide"), "{all}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout_of(&["--index", &index, "report", "--json"])).expect("json");
    assert_eq!(json["summary"]["quieted"]["suppressed"], 2);
    let states: Vec<&str> = json["flags"]
        .as_array()
        .expect("flags")
        .iter()
        .filter_map(|f| f["triage"]["state"].as_str())
        .collect();
    assert_eq!(
        states.len(),
        4,
        "every quieted finding is in --json: {states:?}"
    );

    assert_eq!(
        std::fs::read(&state).expect("bytes"),
        before,
        "report only reads"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Operator state cannot be rebuilt, so a version this build does not know
/// stops the command — on stderr, with nothing half-printed on stdout.
#[test]
fn a_state_file_of_an_unknown_version_stops_report() {
    let dir = scratch("version");
    let (index, keys) = indexed(&dir);
    let state = dir.join("i.state.db");
    seed_state(
        &state,
        &key(&keys, "FLEET-LNX-01"),
        &key(&keys, "FLEET-LNX-02"),
    );
    rusqlite::Connection::open(&state)
        .expect("open")
        .pragma_update(None, "user_version", 99_i64)
        .expect("bump");

    let out = run(&["--index", &index, "report"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("version 99"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A backup without the state file is not a backup of the server: it is the
/// one thing a rescan cannot bring back.
#[test]
fn backup_writes_the_state_file_beside_the_index_copy() {
    let dir = scratch("backup");
    let (index, keys) = indexed(&dir);
    let target = dir.join("backup-2026-09-10.db");
    let target_s = target.to_str().expect("path");

    // Nothing triaged yet: said, not silently skipped.
    let out = stdout_of(&["--index", &index, "backup", target_s]);
    assert!(out.contains("no state file at"), "{out}");
    std::fs::remove_file(&target).expect("tidy");

    seed_state(
        &dir.join("i.state.db"),
        &key(&keys, "FLEET-LNX-01"),
        &key(&keys, "FLEET-LNX-02"),
    );
    let out = stdout_of(&["--index", &index, "backup", target_s]);
    let copy = dir.join("backup-2026-09-10.state.db");
    assert!(target.exists() && copy.exists(), "{out}");
    assert!(
        out.contains("2 acknowledgement(s) or snooze(s) and 2 suppression rule(s)"),
        "{out}"
    );
    assert_eq!(rows(&copy, "SELECT COUNT(*) FROM mark"), 2);
    assert_eq!(rows(&copy, "PRAGMA user_version"), 1);

    // Both refusals before either copy: no index snapshot without its state.
    std::fs::remove_file(&target).expect("tidy");
    let again = run(&["--index", &index, "backup", target_s]);
    assert!(!again.status.success(), "the state copy is already there");
    assert!(
        !target.exists(),
        "and so the index copy was not written either"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `forget` takes a machine's acknowledgements, snoozes and own suppression
/// rules with it, says so, and leaves the rules that are about more than it.
#[test]
fn forget_clears_the_machines_triage_and_says_so() {
    let dir = scratch("forget");
    let (index, keys) = indexed(&dir);
    let state = dir.join("i.state.db");
    let lnx1 = key(&keys, "FLEET-LNX-01");
    seed_state(&state, &lnx1, &key(&keys, "FLEET-LNX-02"));

    let plan = stdout_of(&["--index", &index, "forget", "FLEET-LNX-01", "--dry-run"]);
    assert!(
        plan.contains("would remove 1 acknowledgement(s) or snooze(s)"),
        "{plan}"
    );
    assert!(
        plan.contains("would remove 1 suppression rule(s)"),
        "{plan}"
    );
    assert_eq!(rows(&state, "SELECT COUNT(*) FROM mark"), 2, "a dry run");

    let done = stdout_of(&["--index", &index, "forget", "FLEET-LNX-01"]);
    assert!(done.contains("removed 1 acknowledgement(s)"), "{done}");
    let gone = format!("SELECT COUNT(*) FROM mark WHERE machine_key = '{lnx1}'");
    assert_eq!(rows(&state, &gone), 0);
    assert_eq!(
        rows(&state, "SELECT COUNT(*) FROM mark"),
        1,
        "LNX-02's stays"
    );
    assert_eq!(
        rows(&state, "SELECT COUNT(*) FROM suppression"),
        1,
        "the fleet-wide rule stays"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
