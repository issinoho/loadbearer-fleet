//! Ingest behaviour, against real result documents.
//!
//! The fixtures are genuine `loadbearer` output from real machines, with the
//! identifiers replaced — measurements intact, serials and hostnames not. They
//! cover the cases that actually differ in the field rather than a synthetic
//! ideal: a Windows machine that reports a firmware serial, Linux machines that
//! only report `machine_id` because DMI is root-only there, a run carrying
//! quality caveats, and a pre-1.3.0 file with no identity, tags or gates at
//! all.

use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(name)).expect("fixture should exist")
}

// The crate is a binary, so the test binary links the modules directly.
#[path = "../src/schema.rs"]
mod schema;

use schema::{KeyKind, ResultFile};

#[test]
fn a_real_windows_result_parses_and_keys_on_its_serial() {
    let doc = ResultFile::from_json(&read("win-modern.json")).expect("should parse");
    assert_eq!(doc.schema, "loadbearer.result/1");
    let key = doc.machine_key();
    assert_eq!(key.kind, KeyKind::Serial, "a firmware serial should win");
    assert_eq!(key.value, "SN-WIN-0001");
    assert!(!doc.components.is_empty());
    assert!(doc.overall.score > 0.0);
}

/// On Linux the DMI serial and UUID are root-only, so an unprivileged sweep
/// reports only `machine_id`. The key must fall back rather than give up — and
/// must not silently fall all the way to hostname, which is reissued between
/// machines.
#[test]
fn a_linux_result_falls_back_to_the_os_install_id() {
    let doc = ResultFile::from_json(&read("linux-throttled.json")).expect("should parse");
    let key = doc.machine_key();
    assert_eq!(key.kind, KeyKind::MachineId);
    assert!(!key.value.is_empty());
}

/// A 1.2.4 document has no `identity`, `tags`, `notes` or `gates`. Refusing it
/// would mean a dashboard that can't read its own history.
#[test]
fn a_pre_identity_result_still_parses_and_keys_on_hostname() {
    let doc = ResultFile::from_json(&read("legacy-1.2.4.json")).expect("should parse");
    assert!(doc.machine.identity.is_none());
    assert!(doc.tags.is_empty());
    assert!(doc.gates.is_none());
    assert!(!doc.is_partial());
    let key = doc.machine_key();
    assert_eq!(key.kind, KeyKind::Hostname);
    assert_eq!(key.value, "FLEET-OLD-01");
}

#[test]
fn an_unknown_field_is_ignored_rather_than_fatal() {
    // Adding an optional field is explicitly not a breaking change, so a
    // consumer that chokes on one is the thing that's broken.
    let mut doc: serde_json::Value =
        serde_json::from_str(&read("legacy-1.2.4.json")).expect("valid json");
    doc["something_from_a_later_version"] = serde_json::json!({"nested": [1, 2, 3]});
    doc["machine"]["a_new_inventory_field"] = serde_json::json!("hello");
    ResultFile::from_json(&doc.to_string()).expect("unknown fields must be tolerated");
}

#[test]
fn a_future_schema_major_is_refused_rather_than_misread() {
    let mut doc: serde_json::Value =
        serde_json::from_str(&read("legacy-1.2.4.json")).expect("valid json");
    doc["schema"] = serde_json::json!("loadbearer.result/2");
    let err = ResultFile::from_json(&doc.to_string())
        .expect_err("a removed or retyped field must not be guessed at")
        .to_string();
    assert!(err.contains("schema major 2"), "{err}");
}

#[test]
fn a_soak_document_is_not_a_result_document() {
    let mut doc: serde_json::Value =
        serde_json::from_str(&read("legacy-1.2.4.json")).expect("valid json");
    doc["schema"] = serde_json::json!("loadbearer.soak/1");
    assert!(ResultFile::from_json(&doc.to_string()).is_err());
}

#[test]
fn quality_caveats_surface_the_reasons_a_number_may_mislead() {
    let doc = ResultFile::from_json(&read("linux-throttled.json")).expect("should parse");
    let caveats = doc.quality_caveats();
    assert!(
        !caveats.is_empty(),
        "this run was thermally limited and says so"
    );
    assert!(
        caveats
            .iter()
            .any(|c| c.contains("clocks fell") || c.contains("boost") || c.contains("thermal")),
        "expected a throttling caveat, got {caveats:?}"
    );
}

#[test]
fn a_clean_run_has_nothing_to_caveat() {
    let doc = ResultFile::from_json(&read("win-modern.json")).expect("should parse");
    assert!(!doc.is_partial());
    assert!(
        doc.quality_caveats().is_empty(),
        "got {:?}",
        doc.quality_caveats()
    );
}
