//! Emit (file, class, method) triples. Used by list_planned2.php to drive
//! MethodPlanner with explicit method names — avoiding the "test*"-prefix
//! filter inside MethodPlanner::allTestMethods which misses #[Test] attrs.
use std::env;
use std::path::PathBuf;

fn main() {
    let dirs: Vec<PathBuf> = env::args().skip(1).map(PathBuf::from).collect();
    let cases = discovery::discover_in_dirs(&dirs, &[], &[]).expect("discover_in_dirs");
    for c in cases {
        println!("{}\t{}\t{}", c.file.display(), c.class, c.method);
    }
}
