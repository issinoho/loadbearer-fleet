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
