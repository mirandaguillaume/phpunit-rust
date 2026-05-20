//! `pcov-rs test-discovery` — refresh the discovery cache without producing
//! coverage output. Stub for Phase 1; full implementation in Phase 1.5.

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    pub common: super::CommonOpts,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    println!("test-discovery: implementation pending (refreshes cache only)");
    Ok(())
}
