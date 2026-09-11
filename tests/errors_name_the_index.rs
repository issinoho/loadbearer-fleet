//! When a command cannot find what it was asked about, it has to say *which
//! index it looked in*.
//!
//! `Connection::open` creates the file, so a command pointed at the wrong path
//! used to answer confidently out of an empty database it had just invented:
//! `forget SGS-27GX4N4` run without `--config` created `/tmp/fleet-index.db`
//! and reported that no machine matched, while the service's index held that
//! machine the whole time. Both halves of that are tested here — the message,
//! and the file that should not appear.

use std::path::{Path, PathBuf};
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_loadbearer-fleet");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lbf-err-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs in `dir`, so the default relative index would land there.
fn run(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(EXE)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run the binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn a_missing_index_is_said_out_loud_and_not_invented() {
    let dir = scratch("missing");

    let (ok, err) = run(&dir, &["forget", "SGS-27GX4N4"]);
    assert!(!ok, "this should fail:\n{err}");
    assert!(
        err.contains("no index at"),
        "it should say the index is missing:\n{err}"
    );
    assert!(
        err.contains("--config"),
        "it should name the way to point at the right one:\n{err}"
    );
    assert!(
        !dir.join("fleet-index.db").exists(),
        "reading from a missing index must not create one"
    );

    // Same for the other read-only commands, which have the same failure mode.
    for cmd in [vec!["status"], vec!["report"]] {
        let (ok, err) = run(&dir, &cmd);
        assert!(!ok, "{cmd:?} should fail on a missing index:\n{err}");
        assert!(err.contains("no index at"), "{cmd:?}:\n{err}");
    }
    assert!(
        !dir.join("fleet-index.db").exists(),
        "no command that only reads may create an index"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `scan` takes its folder from the configuration when it is not given one.
///
/// It used to insist on being told, having just read a config that named the
/// folder — worst of all for `--reindex`, whose whole job is to re-read the
/// folder it already knows about. The failure when there is genuinely nowhere
/// to look has to name both ways of supplying it.
#[test]
fn scan_falls_back_to_the_configured_collection_folder() {
    let dir = scratch("scanfolder");
    let unixish = |p: PathBuf| p.display().to_string().replace('\\', "/");
    let config = dir.join("fleet.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nbind = \"127.0.0.1:8787\"\npublic_url = \"http://127.0.0.1:8787\"\n\
             index = '{}'\ncollection_dir = '{}'\n",
            unixish(dir.join("i.db")),
            unixish(fixtures()),
        ),
    )
    .expect("write a config");

    let cfg = config.to_str().expect("path");
    let (ok, err) = run(&dir, &["--config", cfg, "scan"]);
    assert!(ok, "a configured folder should be enough:\n{err}");

    // And the same for the flag that most needs it.
    let (ok, err) = run(&dir, &["--config", cfg, "scan", "--reindex"]);
    assert!(ok, "--reindex should not need the folder repeating:\n{err}");

    // Nowhere to look at all: both ways out, named.
    let (ok, err) = run(
        &dir,
        &["--index", unixish(dir.join("x.db")).as_str(), "scan"],
    );
    assert!(!ok, "this cannot succeed");
    assert!(err.contains("collection_dir"), "{err}");
    assert!(err.contains("as an argument"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// With a real index, the failure has to distinguish "wrong name" from "wrong
/// database" — which is what the count is for.
#[test]
fn an_unmatched_name_reports_what_the_index_does_hold() {
    let dir = scratch("populated");
    let db = dir.join("i.db");
    let db = db.to_str().expect("path");

    let (ok, err) = run(
        &dir,
        &["--index", db, "scan", fixtures().to_str().expect("path")],
    );
    assert!(ok, "the fixture scan should succeed:\n{err}");

    let (ok, err) = run(&dir, &["--index", db, "forget", "NOT-A-MACHINE"]);
    assert!(!ok, "this should fail:\n{err}");
    assert!(
        err.contains("out of the 4 it holds"),
        "it should say how many machines it has, so an empty index is \
         distinguishable from a mistyped name:\n{err}"
    );
    assert!(
        err.contains("i.db"),
        "it should name the index it looked in:\n{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
