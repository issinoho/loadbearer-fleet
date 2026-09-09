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

mod index;
mod schema;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "loadbearer-fleet", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Where the index lives. Derived data — safe to delete.
    #[arg(
        long,
        global = true,
        default_value = "fleet-index.db",
        value_name = "FILE"
    )]
    index: PathBuf,

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log_level.clone().into()),
        )
        .with_target(false)
        .init();

    match &cli.command {
        Command::Scan { dir } => {
            let mut idx = index::Index::open(&cli.index)?;
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
            let idx = index::Index::open(&cli.index)?;
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
    }
}
