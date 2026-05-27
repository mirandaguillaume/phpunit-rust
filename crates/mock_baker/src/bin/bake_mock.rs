//! `bake-mock --test FILE --interface FILE` reads a PHPUnit test source
//! plus the interface source that is mocked inside it, and prints the
//! "baked" version of the test to stdout. Errors are routed to stderr.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about = "Bake PHPUnit createMock() expectation chains into anonymous classes")]
struct Cli {
    #[arg(long)]
    test: PathBuf,
    #[arg(long)]
    interface: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let test_src  = std::fs::read_to_string(&cli.test)
        .with_context(|| format!("reading test {:?}", cli.test))?;
    let iface_src = std::fs::read_to_string(&cli.interface)
        .with_context(|| format!("reading interface {:?}", cli.interface))?;

    // Discover the interface name from the test file so we can key the map.
    let blocks = mock_baker::parse_test(&test_src)?;
    if blocks.is_empty() {
        anyhow::bail!("no createMock pattern found in {:?}", cli.test);
    }
    let iface = mock_baker::parse_interface(&iface_src)?;
    // The interface name comes verbatim from `createMock(X::class)`.
    // The first block drives the key; all blocks sharing the same iface_name reuse it.
    let iface_name = blocks[0].iface_name.clone();
    let ifaces = std::collections::HashMap::from([(iface_name, iface)]);
    let baked = mock_baker::bake(&test_src, &ifaces)?;
    print!("{}", baked);
    Ok(())
}
