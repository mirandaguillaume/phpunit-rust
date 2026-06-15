//! Increment-1 moment-of-truth: run the reducer (native, no PHP) on a real
//! brick-math test file and report the reduced fraction + bail histogram + time.
//!
//! Requires brick-math 0.17.2 cloned + composer-installed at /tmp/brick-math
//! (vendor produced via the php:8.4 container). Skips if absent.
//!
//! Run: `cargo test -p analyzer --test reduce_brickmath -- --nocapture`

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use analyzer::reduce::driver::reduce_in_root;
use analyzer::reduce::eval::{BailReason, Outcome};

#[test]
fn reduce_brickmath_biginteger() {
    let root = Path::new("/tmp/brick-math");
    let file = Path::new("/tmp/brick-math/tests/BigIntegerTest.php");
    if !file.exists() {
        eprintln!("[reduce-bm] brick-math not present at {file:?}; skipping");
        return;
    }

    let t0 = Instant::now();
    let results = reduce_in_root(root, file).expect("reduce_in_root");
    let dt = t0.elapsed();

    let total = results.len();
    let (mut pass, mut fail, mut bailed) = (0usize, 0usize, 0usize);
    let mut bail_hist: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut construct_hist: BTreeMap<String, usize> = BTreeMap::new();
    for r in &results {
        match &r.outcome {
            Outcome::Pass => pass += 1,
            Outcome::Fail(_) => fail += 1,
            Outcome::Bailed(reason) => {
                bailed += 1;
                *bail_hist.entry(reason.tag()).or_default() += 1;
                let payload = match reason {
                    BailReason::UnsupportedConstruct(s) => s.clone(),
                    BailReason::UnknownCall(s) => format!("call:{s}"),
                    BailReason::Other(s) => format!("other:{s}"),
                    other => format!("{other:?}"),
                };
                *construct_hist.entry(payload).or_default() += 1;
            }
        }
    }
    let reduced = pass + fail;

    println!("[reduce-bm] file: BigIntegerTest.php");
    println!("[reduce-bm] total invocations (rows): {total}");
    println!("[reduce-bm] REDUCED: {reduced} (pass={pass} fail={fail})   BAILED: {bailed}");
    if total > 0 {
        println!(
            "[reduce-bm] reduced fraction: {:.1}%",
            100.0 * reduced as f64 / total as f64
        );
    }
    println!("[reduce-bm] bail histogram:");
    for (tag, n) in &bail_hist {
        println!("[reduce-bm]   {tag}: {n}");
    }
    println!("[reduce-bm] top unmodelled constructs (what we'd need to inline/model):");
    let mut sorted: Vec<_> = construct_hist.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (payload, n) in sorted.iter().take(12) {
        println!("[reduce-bm]   {n:>5}  {payload}");
    }
    println!("[reduce-bm] reducer wall-time (incl. codebase scan): {dt:?}");
}
