//! `notify test`, run as the real binary against a receiver on 127.0.0.1.
//!
//! The command exists so an operator can check the wiring without waiting for
//! a drive to fail, which means its exit status and what it prints are the
//! interface: zero and the status line on stdout when the receiver accepted
//! the event, non-zero and the reason on stderr when it did not.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;

const EXE: &str = env!("CARGO_BIN_EXE_loadbearer-fleet");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lbf-notify-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// One request received: its header lines, lower-cased names, and its body.
struct Request {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A receiver that answers every request with `status`, on a thread of its
/// own, and hands each request back over the channel. Plain HTTP/1.1 is all a
/// webhook needs, so this is a socket and a parser rather than a server.
fn receiver(status: &'static str) -> (String, mpsc::Receiver<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
    let addr = listener.local_addr().expect("address");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let mut headers = Vec::new();
            loop {
                line.clear();
                reader.read_line(&mut line).expect("a header");
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    break;
                }
                if let Some((k, v)) = trimmed.split_once(':') {
                    headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
            }
            let length: usize = headers
                .iter()
                .find(|(k, _)| k == "content-length")
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("the body");
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = tx.send(Request { headers, body });
        }
    });
    (format!("http://{addr}/hook?token=do-not-print"), rx)
}

fn config(dir: &std::path::Path, url: &str) -> PathBuf {
    let path = dir.join("fleet.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\nbind = \"127.0.0.1:8787\"\npublic_url = \"https://fleet.example.com\"\n\
             index = \"i.db\"\n\n[notify]\nwebhook_url = \"{url}\"\nsecret = \"cli-secret\"\n"
        ),
    )
    .expect("write the config");
    path
}

#[test]
fn notify_test_sends_a_test_event_and_reports_the_status() {
    let dir = scratch("ok");
    let (url, requests) = receiver("204 No Content");
    let cfg = config(&dir, &url);

    let out = Command::new(EXE)
        .args(["--config", cfg.to_str().expect("path"), "notify", "test"])
        .output()
        .expect("run the binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "failed: {stderr}");
    assert!(stdout.contains("204"), "{stdout}");
    assert!(
        stdout.contains("http://127.0.0.1:"),
        "it says where it went: {stdout}"
    );
    // Scheme and host only, on either stream: the rest of a webhook URL is
    // often its credential.
    assert!(!stdout.contains("do-not-print"), "{stdout}");
    assert!(!stderr.contains("do-not-print"), "{stderr}");

    let req = requests
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the request arrived");
    let v: serde_json::Value = serde_json::from_slice(&req.body).expect("a JSON body");
    assert_eq!(v["event"], "test");
    assert_eq!(v["server"], "https://fleet.example.com");
    assert!(v.get("findings").is_none(), "nothing about the fleet");
    assert_eq!(req.header("content-type"), Some("application/json"));
    assert!(
        req.header("x-loadbearer-signature")
            .is_some_and(|s| s.starts_with("sha256=") && s.len() == 7 + 64),
        "{:?}",
        req.header("x-loadbearer-signature")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn notify_test_fails_loudly_when_the_receiver_refuses() {
    let dir = scratch("refused");
    let (url, _requests) = receiver("404 Not Found");
    let cfg = config(&dir, &url);

    let out = Command::new(EXE)
        .args(["--config", cfg.to_str().expect("path"), "notify", "test"])
        .output()
        .expect("run the binary");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a refusal must not exit 0");
    assert!(stderr.contains("404"), "{stderr}");
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn notify_test_without_a_webhook_says_so() {
    let out = Command::new(EXE)
        .args(["notify", "test"])
        .output()
        .expect("run the binary");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("webhook_url"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
