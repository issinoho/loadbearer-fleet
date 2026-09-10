//! Stamps the build's identity into the binary.
//!
//! The crate version alone cannot answer "which build am I looking at", which
//! is the question the dashboard footer, the startup log line and
//! `--version` all exist to answer: every commit between two releases reports
//! the same `0.2.1`, so a bug report naming a version can be any of them —
//! including a local build with uncommitted changes.
//!
//! Two variables rather than one, because the two audiences differ:
//!
//! - `FLEET_BUILD` is the identity — `<commit>[-dirty]` — and is what gets
//!   shown beside the version and used as a Prometheus label. Short enough to
//!   sit in a footer.
//! - `FLEET_VERSION` is the long human form for `--version`, with the target
//!   and profile as well, because "is this the release build?" is the next
//!   question after "which commit?".
//!
//! `CARGO_PKG_VERSION` stays the semver everywhere else, so anything comparing
//! versions still gets a plain `0.2.1` rather than something with spaces in it.

use std::process::Command;
use std::{env, fmt::Write as _};

fn main() {
    // Rebuild when the checked-out commit, the index (a commit or a staged
    // change), or a reproducible-build date changes. Without the index entry a
    // build that goes from clean to dirty keeps claiming it is clean.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let build = build_id();
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    let profile = env::var("PROFILE").unwrap_or_else(|_| "unknown".into());

    println!("cargo:rustc-env=FLEET_BUILD={build}");
    println!(
        "cargo:rustc-env=FLEET_VERSION={} ({build} {}, {target}, {profile})",
        env!("CARGO_PKG_VERSION"),
        build_date(),
    );
}

/// `<commit>[-dirty]`, or `unknown` where git can't answer — a source tarball
/// build has no repository, and that is not a reason to fail the build.
fn build_id() -> String {
    let commit = git(&["rev-parse", "--short=9", "HEAD"]).filter(|s| !s.is_empty());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    match (commit, dirty) {
        (Some(c), true) => format!("{c}-dirty"),
        (Some(c), false) => c,
        (None, _) => "unknown".to_string(),
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `YYYY-MM-DD` UTC, honouring `SOURCE_DATE_EPOCH` so a reproducible build
/// stamps the source's date rather than the day it happened to be built.
///
/// Done by hand rather than by pulling `time` in as a build dependency: this
/// is the only date a build script needs, and the civil-from-days conversion
/// below is a fixed algorithm rather than something that benefits from a
/// crate.
fn build_date() -> String {
    let secs = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
        });
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let mut s = String::new();
    let _ = write!(s, "{y:04}-{m:02}-{d:02}");
    s
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a proleptic
/// Gregorian date. Shifts the era so that March is month 0, which is what
/// makes the leap day fall at the end of a cycle instead of the middle.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}
