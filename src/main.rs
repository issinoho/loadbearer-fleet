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
mod metrics;
mod schema;
mod service;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use time::OffsetDateTime;

use analytics::{FlagKind, Severity, Thresholds};
use config::{Config, LogFormat};

#[derive(Parser, Debug)]
// `version` from build.rs rather than Cargo.toml: the long form names the
// commit, the target and the profile, which is what a bug report needs.
#[command(name = "loadbearer-fleet", version = env!("FLEET_VERSION"), about)]
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

    /// Overrides the config file. `RUST_LOG` overrides both.
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<String>,
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
    /// Remove a machine from the index, and the documents archived for it.
    ///
    /// For a decommissioned machine, or a removal request. Deleting its result
    /// file from the collection folder is not enough on its own: a scan only
    /// ever adds, so the run stays indexed until something removes it.
    Forget {
        /// Hostname, or the machine key shown in the dashboard.
        #[arg(value_name = "MACHINE")]
        machine: String,
        /// Say what would go, change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Take a consistent copy of the index, safe to run while it is serving.
    ///
    /// For a backup agent to call. The collection folder is the thing actually
    /// worth backing up — this covers the run history the index holds that the
    /// folder may not, when a collector overwrites one file per machine.
    Backup {
        /// Where to write the snapshot. Must not already exist.
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Print a starter configuration file, placeholders included.
    InitConfig,
    /// Contact the identity provider and report what the settings resolve to,
    /// so a wrong tenant fails at deploy time rather than at first sign-in.
    CheckAuth,
    /// Run as a service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand, Debug)]
enum ServiceAction {
    /// The entry point the Windows service controller calls. Not meant to be
    /// run by hand — use `serve` for that.
    Run {
        #[arg(long)]
        allow_remote: bool,
    },
    /// Register the Windows service. Needs an elevated prompt.
    Install {
        /// The account to run as. Omitted means LocalSystem, which reaches a
        /// network share as the machine account.
        #[arg(long, value_name = r"DOMAIN\USER")]
        account: Option<String>,
        /// Password for --account. Prompted for by the service controller if
        /// omitted for an account that needs one.
        #[arg(long, value_name = "PASSWORD")]
        password: Option<String>,
        #[arg(long)]
        allow_remote: bool,
    },
    /// Remove the Windows service. Leaves the index and the config alone.
    Uninstall,
    /// Print a systemd unit for this configuration.
    Unit {
        /// The user to run as.
        #[arg(long, default_value = "loadbearer-fleet", value_name = "USER")]
        user: String,
    },
}

/// Set up logging.
///
/// Precedence is `RUST_LOG`, then `--log-level`, then the config file: the
/// environment wins because it is what someone reaches for while debugging a
/// service they cannot easily reconfigure.
fn init_logging(log: &config::Log, cli_level: Option<&str>) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    let level = cli_level.unwrap_or(&log.level);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    // Every branch ends in the same place; they differ only in the writer and
    // the formatter, and those are different types, so the arms cannot be
    // collapsed.
    match (&log.file, log.format) {
        (None, LogFormat::Text) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(false)
                .init();
        }
        (None, LogFormat::Json) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        (Some(path), format) => {
            let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
            if let Some(dir) = dir {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating the log directory {}", dir.display()))?;
            }
            let name = path
                .file_name()
                .context("log.file has no file name")?
                .to_string_lossy()
                .to_string();
            // Rotated daily with the date appended, because a service log that
            // is never rotated is a disk that eventually fills.
            let writer = tracing_appender::rolling::daily(
                dir.unwrap_or_else(|| std::path::Path::new(".")),
                name,
            );
            match format {
                LogFormat::Text => tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_target(false)
                    // No escape codes in a file somebody will open in Notepad.
                    .with_ansi(false)
                    .with_writer(writer)
                    .init(),
                LogFormat::Json => tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .json()
                    .with_writer(writer)
                    .init(),
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `init-config` writes to stdout and must stay pipeable, so it runs before
    // any logging is set up.
    if matches!(cli.command, Command::InitConfig) {
        print!("{}", Config::starter());
        return Ok(());
    }
    // CLI over file over default, so a service can be configured in a file and
    // still be poked at by hand. The config is loaded before logging is set up
    // because it says where the log goes; a failure here is returned and
    // printed rather than logged.
    let mut config = match &cli.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    if let Some(index) = &cli.index {
        config.server.index = index.clone();
    }
    init_logging(&config.log, cli.log_level.as_deref())?;
    // First line of every run, before any work, because a service log rotates
    // daily and outlives upgrades: without this, a line from six weeks ago
    // can't be attributed to the build that wrote it. Here rather than in
    // `serve` so that a `scan` from a scheduled task says it too. The pid is
    // what distinguishes a restart from a reload in a file being appended to
    // by successive processes.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        build = env!("FLEET_BUILD"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
        "loadbearer-fleet starting"
    );

    match &cli.command {
        Command::InitConfig => unreachable!("handled above"),
        Command::Scan { dir } => {
            let mut idx = index::Index::open(&config.server.index)?
                .with_archive(config.server.archive_dir.as_deref())?;
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
        Command::Forget { machine, dry_run } => {
            let mut idx = index::Index::open(&config.server.index)?
                .with_archive(config.server.archive_dir.as_deref())?;

            let found = idx.machines_matching(machine)?;
            let target = match found.as_slice() {
                [] => anyhow::bail!(
                    "no machine in the index matches {machine:?}. Try the hostname, or the \
                     machine key the dashboard shows on the drilldown."
                ),
                [one] => one.clone(),
                // Hostnames get reissued, so two machines can share one. Which
                // of them to forget is not a guess worth making.
                many => {
                    let list = many
                        .iter()
                        .map(|m| {
                            format!(
                                "  {}  ({} run(s), hostname {})",
                                m.key,
                                m.runs,
                                m.hostname.as_deref().unwrap_or("none recorded")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(
                            "
",
                        );
                    anyhow::bail!(
                        "{machine:?} matches {} machines — hostnames get reissued between \
                         machines, so pick one by its key:
{list}",
                        many.len()
                    )
                }
            };

            let plan = if *dry_run {
                idx.forget_plan(&target.key)?
            } else {
                idx.forget(&target.key)?
            };

            let verb = if *dry_run { "would remove" } else { "removed" };
            println!(
                "{verb} {} run(s) for {} ({})",
                plan.runs,
                target.hostname.as_deref().unwrap_or("no hostname recorded"),
                target.key
            );
            if !plan.archived.is_empty() {
                println!("{verb} {} archived document(s)", plan.archived.len());
            }
            if !plan.sources_still_present.is_empty() {
                println!(
                    "\nStill in the collection folder — the next scan will index this \
                     machine again unless these go:"
                );
                for path in &plan.sources_still_present {
                    println!("  {path}");
                }
            }
            Ok(())
        }
        Command::Backup { file } => {
            let idx = index::Index::open(&config.server.index)?;
            let bytes = idx.backup_to(file)?;
            println!(
                "wrote {} ({:.1} MB) from {} run(s)",
                file.display(),
                bytes as f64 / (1024.0 * 1024.0),
                idx.run_count()?
            );
            if config.server.archive_dir.is_none() {
                println!(
                    "note: no archive_dir is set, so this snapshot is the only copy of any \
                     run whose result file has since been overwritten. See \"Upgrading\" \
                     and \"Backup and restore\" in the README."
                );
            }
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
                    "auth.mode is \"none\": there is nothing to check, and the dashboard will \
                     treat everyone who reaches the port as an administrator."
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
        Command::Service { action } => match action {
            ServiceAction::Run { allow_remote } => service::run(config, *allow_remote),
            ServiceAction::Install {
                account,
                password,
                allow_remote,
            } => {
                let path = service::preflight(cli.config.as_deref(), &config)?;
                let exe = std::env::current_exe().context("finding this executable")?;
                service::install(
                    &exe,
                    path,
                    *allow_remote,
                    account.as_deref(),
                    password.as_deref(),
                )
            }
            ServiceAction::Uninstall => service::uninstall(),
            ServiceAction::Unit { user } => {
                let path = service::preflight(cli.config.as_deref(), &config)?;
                let exe = std::env::current_exe().context("finding this executable")?;
                print!("{}", service::systemd_unit(&exe, path, &config, user));
                Ok(())
            }
        },
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
            let idx = index::Index::open(&config.server.index)?
                .with_archive(config.server.archive_dir.as_deref())?;
            let state = web::AppState::new(idx, &config, Thresholds::default())?;
            if config.server.collection_dir.is_some() && !*no_scan {
                // Through the same path the timer and the button use, so the
                // startup scan is counted and stamped like any other. It used
                // not to be, which left `scan_last_success_timestamp_seconds`
                // missing until the first tick — an alert firing on a service
                // that had just started successfully.
                web::report_startup_scan(&state);
            }
            // One runtime for the server only, so every other subcommand stays
            // a plain synchronous program.
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(web::serve(state, &config, *allow_remote, None))
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
