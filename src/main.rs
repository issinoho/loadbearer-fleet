//! loadbearer-fleet — a dashboard over a folder of collected loadbearer
//! results.
//!
//! The design follows from one decision: the collection folder is the source of
//! truth and this process holds nothing that can't be rebuilt from it. The
//! index is derived, the analytics are computed from the index, and deleting
//! the database costs a rescan and nothing else.
//!
//! It reads the `loadbearer.result/1` schema rather than linking loadbearer's
//! internals, because that schema is loadbearer's documented stability contract
//! and its Rust API explicitly isn't. See `schema.rs`.

mod analytics;
mod auth;
mod config;
mod index;
mod schema;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use time::OffsetDateTime;

use analytics::{FlagKind, Severity, Thresholds};
use config::Config;

#[derive(Parser, Debug)]
#[command(name = "loadbearer-fleet", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Where the index lives. Derived data — safe to delete. Overrides the
    /// config file.
    #[arg(long, global = true, value_name = "FILE")]
    index: Option<PathBuf>,

    /// Configuration file. Written by `init-config`.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[arg(long, global = true, value_name = "LEVEL", default_value = "info")]
    log_level: String,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Index a folder of result files, then report what it found.
    Scan {
        /// Folder to walk. A local path or a UNC share.
        #[arg(value_name = "DIR")]
        dir: PathBuf,
    },
    /// What's in the index right now.
    Status,
    /// Executive summary, cohorts and red flags across the whole index.
    Report {
        /// Emit the whole snapshot as JSON — the same shape the web UI will
        /// consume, so what the dashboard shows can be diffed and scripted.
        #[arg(long)]
        json: bool,
        /// Show informational flags too. Off by default: on a small or varied
        /// estate almost every machine carries one, and burying two critical
        /// findings under forty notices is how a dashboard stops being read.
        #[arg(long)]
        all: bool,
    },
    /// Serve the web dashboard.
    Serve {
        /// The collection folder. Scanned on startup, and again whenever an
        /// admin hits Rescan. Overrides the config file; omit both to serve an
        /// existing index read-only.
        #[arg(value_name = "DIR")]
        dir: Option<PathBuf>,

        /// Address to listen on. Overrides the config file.
        #[arg(long, value_name = "ADDR")]
        bind: Option<SocketAddr>,

        /// Listen on a network interface anyway, without sign-in or without
        /// TLS. The refusal message explains what you are agreeing to.
        #[arg(long)]
        allow_remote: bool,

        /// Skip the startup scan and serve whatever the index already holds.
        #[arg(long)]
        no_scan: bool,
    },
    /// Print a starter configuration file, placeholders included.
    InitConfig,
    /// Contact the identity provider and report what the settings resolve to,
    /// so a wrong tenant fails at deploy time rather than at first sign-in.
    CheckAuth,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `init-config` writes to stdout and must stay pipeable, so it runs before
    // any logging is set up.
    if matches!(cli.command, Command::InitConfig) {
        print!("{}", Config::starter());
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log_level.clone().into()),
        )
        .with_target(false)
        .init();

    // CLI over file over default, so a service can be configured in a file and
    // still be poked at by hand.
    let mut config = match &cli.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    if let Some(index) = &cli.index {
        config.server.index = index.clone();
    }

    match &cli.command {
        Command::InitConfig => unreachable!("handled above"),
        Command::Scan { dir } => {
            let mut idx = index::Index::open(&config.server.index)?;
            let report = idx.scan(dir)?;
            println!(
                "scanned {}: {} file(s), {} new, {} already indexed",
                dir.display(),
                report.seen,
                report.ingested,
                report.unchanged
            );
            for (path, why) in &report.rejected {
                // Rejections are reported, never fatal — one bad upload on a
                // large share must not stop the rest being indexed.
                eprintln!("  skipped {path}: {why}");
            }
            println!(
                "index now holds {} run(s) across {} machine(s)",
                idx.run_count()?,
                idx.machine_count()?
            );
            Ok(())
        }
        Command::Status => {
            let idx = index::Index::open(&config.server.index)?;
            println!(
                "{} run(s), {} machine(s)",
                idx.run_count()?,
                idx.machine_count()?
            );
            for (key, values) in idx.tag_values()? {
                println!("  tag {key}: {}", values.join(", "));
            }
            Ok(())
        }
        Command::Report { json, all } => {
            let idx = index::Index::open(&config.server.index)?;
            let th = Thresholds::default();
            let snap = analytics::snapshot(idx.conn(), &th, OffsetDateTime::now_utc())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
                return Ok(());
            }
            print_report(&snap, *all);
            Ok(())
        }
        Command::CheckAuth => {
            let authenticator = auth::Authenticator::new(&config);
            if !authenticator.enabled() {
                println!(
                    "auth.mode is \"none\": there is nothing to check, and the dashboard will                      treat everyone who reaches the port as an administrator."
                );
                return Ok(());
            }
            let report = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(authenticator.check())?;
            println!("{report}");
            Ok(())
        }
        Command::Serve {
            dir,
            bind,
            allow_remote,
            no_scan,
        } => {
            if let Some(dir) = dir {
                config.server.collection_dir = Some(dir.clone());
            }
            if let Some(bind) = bind {
                config.server.bind = *bind;
            }
            let mut idx = index::Index::open(&config.server.index)?;
            if let Some(dir) = config.server.collection_dir.clone()
                && !*no_scan
            {
                let report = idx.scan(&dir)?;
                tracing::info!(
                    seen = report.seen,
                    ingested = report.ingested,
                    unchanged = report.unchanged,
                    rejected = report.rejected.len(),
                    "startup scan"
                );
                for (path, why) in &report.rejected {
                    tracing::warn!(%path, %why, "skipped");
                }
            }
            let state = web::AppState::new(idx, &config, Thresholds::default())?;
            // One runtime for the server only, so every other subcommand stays
            // a plain synchronous program.
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(web::serve(state, &config, *allow_remote))
        }
    }
}

fn print_report(snap: &analytics::Snapshot, show_info: bool) {
    let s = &snap.summary;
    println!("Fleet: {} machine(s), {} run(s)", s.machines, s.runs);
    if let (Some(med), Some(p10), Some(p90)) = (s.median_score, s.p10_score, s.p90_score) {
        println!("  score   median {med:.0}   p10 {p10:.0}   p90 {p90:.0}");
    }
    let grades: Vec<String> = s
        .grades
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(g, n)| format!("{g} {n}"))
        .collect();
    if !grades.is_empty() {
        println!("  grades  {}", grades.join("   "));
    }
    println!(
        "  {} machine(s) need attention — {} critical, {} warning, {} informational",
        s.machines_flagged, s.critical, s.warnings, s.info
    );

    // Comparability first, because it bounds how much of the rest means
    // anything: a fleet split across configurations can't be ranked as one.
    if s.configurations.len() > 1 {
        println!("\nConfigurations (machines are only comparable within one)");
        for (label, n) in &s.configurations {
            println!("  {n:>4}  {label}");
        }
    }

    let comparable: Vec<&analytics::Cohort> =
        snap.cohorts.iter().filter(|c| c.comparable).collect();
    println!(
        "\nCohorts: {} of {} large enough to compare against ({} machine(s) without a peer group)",
        s.comparable_cohorts, s.cohorts, s.uncohorted
    );
    for c in comparable {
        println!(
            "  {:>4}  {}  median {:.0}  p10 {:.0}  p90 {:.0}  [{}]",
            c.members,
            c.cpu_model,
            c.median,
            c.p10,
            c.p90,
            c.comparability.label()
        );
    }

    let shown: Vec<&analytics::Flag> = snap
        .flags
        .iter()
        .filter(|f| show_info || f.severity != Severity::Info)
        .collect();
    if shown.is_empty() {
        println!("\nNo flags at or above warning.");
        return;
    }
    println!("\nFlags");
    for f in shown {
        let who = f.hostname.as_deref().unwrap_or(&f.machine_key);
        println!(
            "  [{:<8}] {:<11} {who}  {}",
            match f.severity {
                Severity::Critical => "critical",
                Severity::Warning => "warning",
                Severity::Info => "info",
            },
            match f.kind {
                FlagKind::Machine => "machine",
                FlagKind::Measurement => "measurement",
                FlagKind::Coverage => "coverage",
            },
            f.headline
        );
    }
    if !show_info && s.info > 0 {
        println!(
            "\n{} informational flag(s) hidden — pass --all to see them.",
            s.info
        );
    }
}
