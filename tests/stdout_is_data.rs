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

/// `report --by` prints a table, and with `--json` the same document
/// `/api/groups` sends — pipeable like the snapshot. An unknown dimension is
/// refused, on stderr, rather than answered with an empty table.
#[test]
fn report_by_writes_the_groups_to_stdout() {
    let dir = scratch("report-by");
    let index = dir.join("i.db");
    let index = index.to_str().expect("path");
    stdout_of(&["--index", index, "scan", fixtures().to_str().expect("path")]);

    let json = stdout_of(&["--index", index, "report", "--by", "tag:site", "--json"]);
    let g: serde_json::Value = serde_json::from_str(&json).unwrap_or_else(|e| {
        panic!(
            "stdout did not parse as JSON ({e}). The first 200 bytes were:\n{}",
            &json[..json.len().min(200)]
        )
    });
    assert_eq!(g["by"], "tag:site");
    let groups = g["groups"].as_array().expect("groups");
    let total: u64 = groups
        .iter()
        .map(|grp| grp["machines"].as_u64().expect("count"))
        .sum();
    assert_eq!(Some(total), g["machines"].as_u64(), "the groups add up");
    assert!(!json.contains('\u{1b}'), "stdout carried ANSI escape codes");

    let text = stdout_of(&["--index", index, "report", "--by", "tag:site"]);
    assert!(
        text.starts_with("Grouped by tag:site:"),
        "stdout should open with the table's own heading, got:\n{}",
        &text[..text.len().min(200)]
    );
    assert!(text.contains("glasgow"), "{text}");
    assert!(
        text.contains("(untagged)"),
        "the untagged fixture is a group: {text}"
    );
    assert!(!text.contains('\u{1b}'), "stdout carried ANSI escape codes");

    let refused = Command::new(EXE)
        .args(["--index", index, "report", "--by", "hostname"])
        .output()
        .expect("run the binary");
    assert!(!refused.status.success(), "an unknown dimension must fail");
    assert!(refused.stdout.is_empty(), "and print nothing to stdout");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("tag:<key>"),
        "the refusal says what is accepted: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `report --csv` writes the table and nothing else: no byte-order mark (that
/// is the download's, for Excel), no log line, a header first and every row
/// CRLF-terminated whatever platform this runs on — so the bytes are the same
/// on Windows and Linux, and the file the same as one the server would send.
/// An unknown table is refused on stderr; `--csv` with `--json` is refused
/// rather than one of them silently winning.
#[test]
fn report_csv_writes_only_the_table_to_stdout() {
    let dir = scratch("report-csv");
    let index = dir.join("i.db");
    let index = index.to_str().expect("path");
    stdout_of(&["--index", index, "scan", fixtures().to_str().expect("path")]);

    let machines = stdout_of(&["--index", index, "report", "--csv", "machines"]);
    assert!(
        machines.starts_with("hostname,machine_key,"),
        "stdout should open with the header, got:\n{}",
        &machines[..machines.len().min(200)]
    );
    assert!(!machines.starts_with('\u{feff}'), "no BOM on stdout");
    assert!(
        !machines.contains('\u{1b}'),
        "stdout carried ANSI escape codes"
    );
    assert!(machines.ends_with("\r\n"));
    // Every line end is CRLF, and there is no line that is not a row.
    let lines: Vec<&str> = machines.split_terminator("\r\n").collect();
    assert!(lines.iter().all(|l| !l.contains('\n')), "a bare LF");
    assert_eq!(lines.len(), 1 + 4, "a header and the four fixtures");
    assert!(lines[0].ends_with(",tag:ring,tag:site"), "{}", lines[0]);

    let findings = stdout_of(&["--index", index, "report", "--csv", "findings"]);
    assert!(findings.starts_with("severity,queue,code,"), "{findings}");
    let report: serde_json::Value =
        serde_json::from_str(&stdout_of(&["--index", index, "report", "--json"])).expect("JSON");
    assert_eq!(
        findings.split_terminator("\r\n").count(),
        1 + report["flags"].as_array().expect("flags").len(),
        "every finding, informational ones included, without --all"
    );

    for args in [
        &["--index", index, "report", "--csv", "heatmap"][..],
        &["--index", index, "report", "--csv", "machines", "--json"][..],
    ] {
        let refused = Command::new(EXE)
            .args(args)
            .output()
            .expect("run the binary");
        assert!(!refused.status.success(), "{args:?} must fail");
        assert!(refused.stdout.is_empty(), "and print nothing to stdout");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `report --distribution` and `report --heatmap` print a summary, and with
/// `--json` the documents `/api/distribution` and `/api/heatmap` send. A
/// measure no machine has is refused, on stderr, with what there is.
#[test]
fn report_distribution_and_heatmap_write_to_stdout() {
    let dir = scratch("report-components");
    let index = dir.join("i.db");
    let index = index.to_str().expect("path");
    stdout_of(&["--index", index, "scan", fixtures().to_str().expect("path")]);

    let parse = |json: &str| -> serde_json::Value {
        serde_json::from_str(json).unwrap_or_else(|e| {
            panic!(
                "stdout did not parse as JSON ({e}). The first 200 bytes were:\n{}",
                &json[..json.len().min(200)]
            )
        })
    };
    let json = stdout_of(&[
        "--index",
        index,
        "report",
        "--distribution",
        "disk/seq_read",
        "--json",
    ]);
    let d = parse(&json);
    assert_eq!(d["measure"], "disk/seq_read");
    let binned: u64 = d["bins"]
        .as_array()
        .expect("bins")
        .iter()
        .map(|b| b["machines"].as_u64().expect("count"))
        .sum();
    assert_eq!(Some(binned), d["included"].as_u64(), "the bins add up");
    assert!(!json.contains('\u{1b}'), "stdout carried ANSI escape codes");

    let json = stdout_of(&["--index", index, "report", "--heatmap", "--json"]);
    let h = parse(&json);
    assert_eq!(h["machines"], 4);
    assert_eq!(h["rows"].as_array().expect("rows").len(), 4);

    let text = stdout_of(&["--index", index, "report", "--distribution", "overall"]);
    assert!(
        text.starts_with("Overall score"),
        "stdout should open with the measure, got:\n{}",
        &text[..text.len().min(200)]
    );
    let text = stdout_of(&["--index", index, "report", "--heatmap"]);
    assert!(text.starts_with("Against identical machines"), "{text}");
    assert!(text.contains("FLEET-WIN-01"), "{text}");

    let refused = Command::new(EXE)
        .args(["--index", index, "report", "--distribution", "floppy"])
        .output()
        .expect("run the binary");
    assert!(!refused.status.success(), "an unknown measure must fail");
    assert!(refused.stdout.is_empty(), "and print nothing to stdout");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("cpu"),
        "the refusal says what there is: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `report --changes` prints a summary, and with `--json` the same document
/// `/api/changes` sends. A window that is not one of the three is refused, on
/// stderr, rather than answered with a comparison against a date nobody asked
/// for.
#[test]
fn report_changes_writes_the_changes_to_stdout() {
    let dir = scratch("report-changes");
    let index = dir.join("i.db");
    let index = index.to_str().expect("path");
    stdout_of(&["--index", index, "scan", fixtures().to_str().expect("path")]);

    let json = stdout_of(&["--index", index, "report", "--changes", "4", "--json"]);
    let c: serde_json::Value = serde_json::from_str(&json).unwrap_or_else(|e| {
        panic!(
            "stdout did not parse as JSON ({e}). The first 200 bytes were:\n{}",
            &json[..json.len().min(200)]
        )
    });
    assert_eq!(c["window_weeks"], 4);
    assert!(c["counts"].is_object(), "parsed, but these are not changes");
    assert!(c["new_findings"].is_array());
    assert!(!json.contains('\u{1b}'), "stdout carried ANSI escape codes");

    // No number means a week.
    let text = stdout_of(&["--index", index, "report", "--changes"]);
    assert!(
        text.starts_with("Changes over 1 week:"),
        "stdout should open with the summary's own heading, got:\n{}",
        &text[..text.len().min(200)]
    );
    assert!(text.contains("judged by today's rules"), "{text}");
    assert!(!text.contains('\u{1b}'), "stdout carried ANSI escape codes");

    let refused = Command::new(EXE)
        .args(["--index", index, "report", "--changes", "2"])
        .output()
        .expect("run the binary");
    assert!(!refused.status.success(), "an unknown window must fail");
    assert!(refused.stdout.is_empty(), "and print nothing to stdout");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("1, 4 or 13"),
        "the refusal says what is accepted: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `report --lifecycle` prints the candidates, and with `--json` a document of
/// them whose counts add up to the machines. It refuses to be combined with
/// `--by` or `--changes`, on stderr, rather than quietly answering one of them.
#[test]
fn report_lifecycle_writes_the_candidates_to_stdout() {
    let dir = scratch("report-lifecycle");
    let index = dir.join("i.db");
    let index = index.to_str().expect("path");
    stdout_of(&["--index", index, "scan", fixtures().to_str().expect("path")]);

    let json = stdout_of(&["--index", index, "report", "--lifecycle", "--json"]);
    let r: serde_json::Value = serde_json::from_str(&json).unwrap_or_else(|e| {
        panic!(
            "stdout did not parse as JSON ({e}). The first 200 bytes were:\n{}",
            &json[..json.len().min(200)]
        )
    });
    let counts = r["counts"].as_object().expect("counts");
    let total: u64 = counts.values().map(|n| n.as_u64().expect("count")).sum();
    assert_eq!(Some(total), r["machines"].as_u64(), "the counts add up");
    let candidates = r["candidates"].as_array().expect("candidates");
    assert_eq!(
        candidates.len() as u64,
        total - r["counts"]["keep"].as_u64().expect("keep"),
        "every machine but those to keep"
    );
    for c in candidates {
        assert!(
            c["reasons"].as_array().is_some_and(|a| !a.is_empty()),
            "{c}"
        );
    }
    assert!(!json.contains('\u{1b}'), "stdout carried ANSI escape codes");

    // The fixtures have machines to watch and nothing costlier: counted, and
    // listed only with --all.
    let text = stdout_of(&["--index", index, "report", "--lifecycle"]);
    assert!(
        text.starts_with("Lifecycle:"),
        "stdout should open with the list's own heading, got:\n{}",
        &text[..text.len().min(200)]
    );
    assert!(text.contains("Age alone is never a reason"), "{text}");
    assert!(text.contains("pass --all to list them"), "{text}");
    let all = stdout_of(&["--index", index, "report", "--lifecycle", "--all"]);
    assert!(all.contains("\nWatch · "), "{all}");
    assert!(all.contains("years old, by its CPU's launch date"), "{all}");
    assert!(!all.contains('\u{1b}'), "stdout carried ANSI escape codes");

    for other in [["--by", "os"], ["--changes", "4"]] {
        let refused = Command::new(EXE)
            .args(["--index", index, "report", "--lifecycle"])
            .args(other)
            .output()
            .expect("run the binary");
        assert!(
            !refused.status.success(),
            "--lifecycle with {other:?} must fail"
        );
        assert!(refused.stdout.is_empty(), "and print nothing to stdout");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("cannot be used with"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }

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

/// A machine that has only ever been skipped is one line of the printed
/// report and a list in `--json`, and `forget` finds it by hostname — all
/// through the real binary, with stdout still only the document.
#[test]
fn a_machine_only_ever_skipped_is_counted_in_report_and_forgettable() {
    let dir = scratch("skipped");
    let index = dir.join("i.db");
    let index = index.to_str().expect("path");
    let folder = dir.join("results");
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::copy(fixtures().join("win-modern.json"), folder.join("a.json")).expect("copy");
    std::fs::write(
        folder.join("status-x.json"),
        serde_json::json!({
            "schema": "loadbearer.run-status/1",
            "timestamp": "2026-09-12T22:00:00Z",
            "reason": "busy",
            "detail": "busy: the CPU was at 80%",
            "hostname": "NEVER-MEASURED-1",
            "tags": { "site": "glasgow" },
        })
        .to_string(),
    )
    .expect("status file");

    let scanned = stdout_of(&["--index", index, "scan", folder.to_str().expect("path")]);
    assert!(scanned.contains("1 run status record(s) read"), "{scanned}");
    assert!(
        !scanned.contains("skipped "),
        "not a rejected file: {scanned}"
    );

    let json = stdout_of(&["--index", index, "report", "--json"]);
    let snap: serde_json::Value = serde_json::from_str(&json).expect("JSON");
    assert_eq!(snap["summary"]["unmeasured"], 1);
    assert_eq!(snap["unmeasured"][0]["hostname"], "NEVER-MEASURED-1");
    assert_eq!(snap["summary"]["machines"], 1, "not a machine");

    let text = stdout_of(&["--index", index, "report"]);
    assert!(
        text.contains("1 machine(s) tried and never measured"),
        "{text}"
    );
    assert!(
        !text.contains("NEVER-MEASURED-1"),
        "the line is a count: {text}"
    );
    assert!(!text.contains('\u{1b}'));

    let plan = stdout_of(&["--index", index, "forget", "never-measured-1", "--dry-run"]);
    assert!(plan.contains("would remove 0 run(s)"), "{plan}");
    assert!(
        plan.contains("would remove its run status record"),
        "{plan}"
    );
    assert!(plan.contains("status-x.json"), "{plan}");
    let done = stdout_of(&["--index", index, "forget", "NEVER-MEASURED-1"]);
    assert!(done.contains("removed its run status record"), "{done}");

    let after = stdout_of(&["--index", index, "report", "--json"]);
    let snap: serde_json::Value = serde_json::from_str(&after).expect("JSON");
    assert_eq!(snap["summary"]["unmeasured"], 0);
    assert!(!after.contains("NEVER-MEASURED-1"));

    let _ = std::fs::remove_dir_all(&dir);
}
