//! What each command writes to **stdout** must be only the document it was
//! asked for.
//!
//! `report --json`, `init-config` and `reference` all exist to be piped —
//! into `jq`, into a config file, into a wiki page. A diagnostic line in front
//! of the document turns every one of those into a parse error, and the
//! failure looks like the tool being broken rather than like a log going to
//! the wrong stream.
//!
//! This is a real regression rather than a hypothetical: adding a line logged
//! on every run put an `INFO` record — ANSI-coloured, at that — ahead of the
//! JSON, because `tracing_subscriber::fmt()` defaults to stdout. Nothing
//! caught it, so these run the actual binary and parse what comes back.

use std::path::{Path, PathBuf};
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_loadbearer-fleet");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lbf-stdout-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs the binary and returns stdout, asserting it succeeded. Deliberately
/// does not merge stderr — separating the two is the property under test.
fn stdout_of(args: &[&str]) -> String {
    let out = Command::new(EXE)
        .args(args)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("stdout should be UTF-8")
}

#[test]
fn report_json_writes_only_the_snapshot_to_stdout() {
    let dir = scratch("report");
    let index = dir.join("i.db");
    let index = index.to_str().expect("path");

    stdout_of(&["--index", index, "scan", fixtures().to_str().expect("path")]);
    let json = stdout_of(&["--index", index, "report", "--json"]);

    let snap: serde_json::Value = serde_json::from_str(&json).unwrap_or_else(|e| {
        panic!(
            "stdout did not parse as JSON ({e}). The first 200 bytes were:\n{}",
            &json[..json.len().min(200)]
        )
    });
    assert!(
        snap.get("machines").and_then(|m| m.as_array()).is_some(),
        "parsed, but this is not a snapshot"
    );
    // No escape codes either: a colourised log line would have brought them.
    assert!(!json.contains('\u{1b}'), "stdout carried ANSI escape codes");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The same property with a log file configured, which the test above cannot
/// see: without one there is nothing to announce.
///
/// A configured log file makes the binary say which *dated* file it is
/// writing, because the configured path is a stem and `tail -f` on it fails.
/// That announcement belongs on stderr — where a service's journal picks it up
/// — and if it ever moves to stdout it breaks every pipe this file exists to
/// protect.
#[test]
fn a_configured_log_file_does_not_put_its_name_on_stdout() {
    let dir = scratch("logline");
    let unixish = |p: PathBuf| p.display().to_string().replace('\\', "/");
    let config = dir.join("fleet.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nbind = \"127.0.0.1:8788\"\npublic_url = \"http://127.0.0.1:8788\"\n\
             index = '{}'\n\n[log]\nfile = '{}'\n",
            unixish(dir.join("i.db")),
            unixish(dir.join("fleet.log")),
        ),
    )
    .expect("write a config");

    // The index has to exist: a command that only reads refuses to invent one,
    // rather than answering out of an empty database it made itself.
    stdout_of(&[
        "--config",
        config.to_str().expect("path"),
        "scan",
        fixtures().to_str().expect("path"),
    ]);

    let out = Command::new(EXE)
        .args([
            "--config",
            config.to_str().expect("path"),
            "report",
            "--json",
        ])
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "report --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let json = String::from_utf8(out.stdout).expect("stdout should be UTF-8");
    serde_json::from_str::<serde_json::Value>(&json).unwrap_or_else(|e| {
        panic!(
            "stdout did not parse as JSON ({e}). The first 200 bytes were:\n{}",
            &json[..json.len().min(200)]
        )
    });

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("logging to") && err.contains("fleet.log."),
        "stderr should name the dated log file, but was:\n{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_config_writes_only_the_config_to_stdout() {
    let text = stdout_of(&["init-config"]);
    toml::from_str::<toml::Value>(&text).unwrap_or_else(|e| {
        panic!(
            "stdout did not parse as TOML ({e}). The first 200 bytes were:\n{}",
            &text[..text.len().min(200)]
        )
    });
    assert!(text.starts_with('#'), "should open with its own comment");
}

/// `service unit` prints a file for review, so it must not need write access to
/// anywhere the service will later run. It used to set logging up first, which
/// *creates the log directory* — so an ordinary user previewing a unit got
/// "creating the log directory /var/log/loadbearer-fleet" rather than a unit,
/// and the only way to read one was as root.
#[test]
fn service_unit_prints_a_unit_without_creating_anything() {
    let dir = scratch("unit");
    let log_dir = dir.join("logs-that-should-not-be-created");
    let cfg = dir.join("fleet.toml");
    std::fs::write(
        &cfg,
        format!(
            // `bind` and `index` have no serde default, so a partial [server]
            // table is rejected outright — all three have to be here.
            "[server]\n\
             bind = '127.0.0.1:8787'\n\
             public_url = 'https://fleet.example'\n\
             index = '{}'\n\
             [log]\n\
             file = '{}'\n",
            dir.join("i.db").display().to_string().replace('\\', "/"),
            log_dir
                .join("fleet.log")
                .display()
                .to_string()
                .replace('\\', "/"),
        ),
    )
    .expect("write config");

    let unit = stdout_of(&["--config", cfg.to_str().expect("path"), "service", "unit"]);

    assert!(unit.contains("[Service]"), "not a unit file:\n{unit}");
    assert!(unit.contains("ExecStart="), "no ExecStart:\n{unit}");
    assert!(!unit.contains('\u{1b}'), "stdout carried ANSI escape codes");
    assert!(
        !log_dir.exists(),
        "printing a unit created {} — reviewing a file should not need write \
         access to where the service will run",
        log_dir.display()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reference_writes_only_markdown_to_stdout() {
    let text = stdout_of(&["reference"]);
    assert!(
        text.starts_with("# Command line and configuration"),
        "stdout should open with the page's own heading, got:\n{}",
        &text[..text.len().min(200)]
    );
    assert!(!text.contains('\u{1b}'), "stdout carried ANSI escape codes");
}
