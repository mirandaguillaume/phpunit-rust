//! `pcov-rs cache` — manage the on-disk cache. Stubs for Phase 1.

use clap::Subcommand;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub op: Op,
}

#[derive(Subcommand)]
pub enum Op {
    /// Show cache size and entry count
    Status,
    /// Remove all cache entries
    Clear,
    /// GC entries older than LRU cap
    Prune,
}

pub fn run(args: Args) -> anyhow::Result<()> {
    match args.op {
        Op::Status => println!("cache status: implementation pending"),
        Op::Clear => println!("cache clear: implementation pending"),
        Op::Prune => println!("cache prune: implementation pending"),
    }
    Ok(())
}
