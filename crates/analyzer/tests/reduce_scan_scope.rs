//! Confirm the scan cost is OVER-SCANNING vendor, not mago being slow.
//! Compares full `load` (whole root incl. vendor) vs `load_excluding_vendor`.

use std::path::Path;
use std::time::Instant;

use analyzer::mago_bridge::MagoProject;

#[test]
#[ignore]
fn scan_scope_breakdown() {
    let root = Path::new("/tmp/doctrine-collections");
    if !root.exists() {
        eprintln!("[scope] absent; skipping");
        return;
    }

    let t = Instant::now();
    let full = MagoProject::load(root).expect("load");
    let full_dt = t.elapsed();
    println!(
        "[scope] FULL load (incl. vendor): {} classes in {:?}",
        full.class_like_count(),
        full_dt
    );

    let t = Instant::now();
    let novendor = MagoProject::load_excluding_vendor(root).expect("load_excluding_vendor");
    let nv_dt = t.elapsed();
    println!(
        "[scope] load_excluding_vendor: {} classes in {:?}",
        novendor.class_like_count(),
        nv_dt
    );

    println!(
        "[scope] vendor accounts for {} classes; scoped scan is {:.1}x faster",
        full.class_like_count() as i64 - novendor.class_like_count() as i64,
        full_dt.as_secs_f64() / nv_dt.as_secs_f64().max(1e-9)
    );
}
