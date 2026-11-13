//! `report` and the server must judge the fleet by the same lines.
//!
//! Both build their thresholds from `[thresholds]` in the config file, but
//! they are separate entry points — `report` in `main`, the server in
//! `AppState::new`, which the Windows service shares — and a threshold read in
//! one and forgotten in the other would make the command line and the
//! dashboard disagree about the same machine without any error. So this runs
//! the real binary both ways over one index and one config, and compares what
//! each says — the snapshot, and the CSV tables `report --csv` prints and the
//! dashboard downloads.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const EXE: &str = env!("CARGO_BIN_EXE_loadbearer-fleet");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lbf-thresholds-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn run(args: &[&str]) -> String {
    let out = Command::new(EXE)
        .args(args)
        .output()
        .expect("run the binary");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8")
}

/// Stopped however the test ends, so a failed assertion does not leave a
/// server holding the port and the index.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `GET path` over HTTP/1.0, so the body is neither chunked nor compressed.
fn get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    let (head, body) = reply.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.0 200").then(|| body.to_string())
}

/// Every finding as (machine, rule, severity): the age in a staleness
/// headline moves with the clock between the two runs, the judgement does not.
fn findings(snap: &Value) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = snap["flags"]
        .as_array()
        .expect("flags")
        .iter()
        .map(|f| {
            (
                f["machine_key"].as_str().unwrap_or_default().to_string(),
                f["code"].as_str().unwrap_or_default().to_string(),
                f["severity"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

fn overrides_by_host(snap: &Value) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = snap["machines"]
        .as_array()
        .expect("machines")
        .iter()
        .map(|m| {
            (
                m["hostname"].as_str().unwrap_or_default().to_string(),
                m["overridden_by"].clone(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn report_and_the_server_judge_the_fleet_by_the_same_lines() {
    let dir = scratch("agree");
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("a free port")
        .local_addr()
        .expect("address")
        .port();
    let config = dir.join("fleet.toml");
    // Edinburgh stale on a line every fixture is past, whatever day this
    // runs; Glasgow's 29%-free disk nearly full on a raised line.
    std::fs::write(
        &config,
        format!(
            "[server]\nbind = \"127.0.0.1:{port}\"\npublic_url = \"http://127.0.0.1:{port}\"\n\
             index = \"i.db\"\ncollection_dir = '{}'\n\
             [thresholds]\nstale_days = 400\n\
             [[thresholds.overrides]]\nname = \"edinburgh, measured monthly\"\n\
             tags = {{ site = \"edinburgh\" }}\nstale_days = 30\n\
             [[thresholds.overrides]]\nname = \"glasgow\"\ntags = {{ site = \"glasgow\" }}\n\
             disk_free_pct = 40\ndisk_free_critical_pct = 20\n",
            fixtures().display()
        ),
    )
    .expect("config");
    let config = config.to_str().expect("path");

    run(&["--config", config, "scan"]);
    let report: Value =
        serde_json::from_str(&run(&["--config", config, "report", "--json"])).expect("JSON");

    let server = Server(
        Command::new(EXE)
            .args(["--config", config, "serve", "--no-scan"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the server"),
    );
    let started = Instant::now();
    let served = loop {
        if let Some(body) = get(port, "/api/snapshot") {
            break body;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the server did not answer on port {port}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let served: Value = serde_json::from_str(&served).expect("JSON");

    // The overrides did something, so agreeing is not agreeing about nothing.
    let by_host = overrides_by_host(&report);
    let named = |host: &str| -> Vec<String> {
        by_host
            .iter()
            .find(|(h, _)| h == host)
            .and_then(|(_, by)| by.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o["name"].as_str().map(str::to_string))
            .collect()
    };
    assert_eq!(named("FLEET-LNX-01"), ["edinburgh, measured monthly"]);
    assert_eq!(named("FLEET-WIN-01"), ["glasgow"]);
    assert!(named("FLEET-OLD-01").is_empty());
    let mine = findings(&report);
    assert_eq!(
        mine.iter().filter(|f| f.1 == "stale").count(),
        2,
        "the two Edinburgh machines, on 30 days; nobody else on 400: {mine:?}"
    );
    assert_eq!(
        mine.iter().filter(|f| f.1 == "disk_nearly_full").count(),
        1,
        "{mine:?}"
    );

    assert_eq!(served["thresholds"], report["thresholds"]);
    assert_eq!(served["threshold_overrides"], report["threshold_overrides"]);
    assert_eq!(overrides_by_host(&served), by_host);
    assert_eq!(findings(&served), mine);
    for field in [
        "stale",
        "machines_flagged",
        "critical",
        "warnings",
        "info",
        "lifecycle",
    ] {
        assert_eq!(
            served["summary"][field], report["summary"][field],
            "summary.{field}"
        );
    }

    // The CSV through both doors is the same file, the download's
    // byte-order mark aside — row for row, cell for cell, but for the figures
    // that move with the clock between the two reads: a machine's age in
    // days, and a staleness finding's wording of it.
    for table in ["machines", "findings"] {
        let printed = run(&["--config", config, "report", "--csv", table]);
        let served = get(port, &format!("/api/{table}.csv")).expect("the download");
        let served = served
            .strip_prefix('\u{feff}')
            .expect("the download carries a BOM");
        let (printed, served) = (rows_of(&printed), rows_of(served));
        assert_eq!(printed[0], served[0], "{table}: the same columns");
        assert_eq!(printed.len(), served.len(), "{table}: the same rows");
        assert!(printed.len() > 1, "{table}: agreeing about nothing");
        let head = &printed[0];
        let col = |name: &str| head.iter().position(|c| c == name);
        let moving: Vec<usize> = ["age_days", "headline", "detail"]
            .iter()
            .filter_map(|c| col(c))
            .collect();
        let code = col("code");
        for (p, s) in printed.iter().zip(&served).skip(1) {
            for (i, (a, b)) in p.iter().zip(s).enumerate() {
                let clocked = moving.contains(&i)
                    && (head[i] == "age_days" || code.is_some_and(|c| p[c] == "stale"));
                if !clocked {
                    assert_eq!(a, b, "{table}: {} differs", head[i]);
                }
            }
        }
    }

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// CSV rows, read as RFC 4180 writes them: quoted fields may hold commas,
/// quotes doubled, and line breaks; rows end CRLF.
fn rows_of(csv: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut field) = (Vec::new(), Vec::new(), String::new());
    let mut quoted = false;
    let mut chars = csv.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {
                assert_eq!(chars.next(), Some('\n'), "CR without LF");
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => field.push(c),
        }
    }
    rows
}
