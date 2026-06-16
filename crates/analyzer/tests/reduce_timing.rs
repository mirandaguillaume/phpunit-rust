//! Honest resource/time measurement of the reducer vs the PHP runtime, on the
//! doctrine/collections ArrayCollectionTest file. Splits the one-time codebase
//! scan from the per-test native reduction. Skips if the clone is absent.
//!
//! Run: `cargo test -p analyzer --test reduce_timing -- --ignored --nocapture`

use std::path::Path;
use std::time::Instant;

use analyzer::mago_bridge::MagoProject;

#[test]
#[ignore]
fn time_reducer_on_doctrine_collections() {
    let root = Path::new("/tmp/doctrine-collections");
    let file = Path::new("/tmp/doctrine-collections/tests/ArrayCollectionTest.php");
    if !file.exists() {
        eprintln!("[timing] doctrine-collections absent; skipping");
        return;
    }

    // Phase 1: the one-time codebase scan (mago parse of the whole root).
    let t0 = Instant::now();
    let project = MagoProject::load(root).expect("load");
    let scan = t0.elapsed();
    println!(
        "[timing] codebase scan (MagoProject::load, {} classes): {:?}",
        project.class_like_count(),
        scan
    );

    // Phase 2: warm native reduction — re-parse + reduce the test file N times to
    // get a stable per-run number (the scan above is NOT repeated).
    let runs = 3u32;
    let t1 = Instant::now();
    let mut last = 0usize;
    for _ in 0..runs {
        // reduce_file re-loads; to isolate the reduction we re-run the discovery
        // + reduce path without reloading the whole codebase would need internals,
        // so here we measure the full reduce_file (load+reduce) once and the
        // amortized cost, and report both honestly.
        let res = analyzer::reduce::driver::reduce_in_root(root, file).expect("reduce");
        last = res.len();
    }
    let total = t1.elapsed();
    println!(
        "[timing] reduce_in_root (load+discover+reduce {last} rows) x{runs}: total {:?}, per-run {:?}",
        total,
        total / runs
    );
    println!(
        "[timing] NOTE: reduce_in_root re-scans the codebase each call; the pure native\n\
         [timing] reduction of the rows (after scan) is the per-run time MINUS the scan above."
    );
}
