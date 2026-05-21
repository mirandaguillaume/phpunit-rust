//! Dump every (class, method) the discovery crate finds under given dirs.
//! Used for debugging vanilla-vs-rust test-count gaps.
//!
//! cargo run -p discovery --example list_discovered -- <dir1> [dir2 ...]
use std::env;
use std::path::PathBuf;

fn main() {
    let dirs: Vec<PathBuf> = env::args().skip(1).map(PathBuf::from).collect();
    if dirs.is_empty() { eprintln!("usage: list_discovered <dir1> [dir2 ...]"); std::process::exit(1); }
    let cases = discovery::discover_in_dirs(&dirs, &[], &[]).expect("discover_in_dirs");
    for c in &cases { println!("{}::{}", c.class, c.method); }
    eprintln!("Found {} methods across {} classes",
        cases.len(),
        cases.iter().map(|c| c.class.as_str()).collect::<std::collections::BTreeSet<_>>().len());
}
