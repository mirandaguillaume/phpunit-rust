//! Performance benchmarks for `pcov-rs analyze`.
//!
//! Two scenarios on the value_objects fixture:
//!   * full (--no-cache): cold-path analysis cost
//!   * warm cache: incremental cost when nothing changed
//!
//! These are baselines, not regression gates. Run with `cargo bench`.

use criterion::{criterion_group, criterion_main, Criterion};
use std::process::Command;

const FIXTURE: &str = "tests/fixtures/value_objects";

fn bench_full(c: &mut Criterion) {
    c.bench_function("value_objects full (no cache)", |b| {
        b.iter(|| {
            let output = Command::new(env!("CARGO_BIN_EXE_pcov-rs"))
                .current_dir(FIXTURE)
                .args(["analyze", "--format", "pcov", "--no-cache"])
                .output()
                .expect("pcov-rs analyze failed");
            assert!(output.status.success(), "non-zero exit: {output:?}");
        });
    });
}

fn bench_warm(c: &mut Criterion) {
    // Warm the cache once before iterations.
    let _ = Command::new(env!("CARGO_BIN_EXE_pcov-rs"))
        .current_dir(FIXTURE)
        .args(["analyze", "--format", "pcov"])
        .output()
        .expect("warmup run failed");

    c.bench_function("value_objects warm cache", |b| {
        b.iter(|| {
            let output = Command::new(env!("CARGO_BIN_EXE_pcov-rs"))
                .current_dir(FIXTURE)
                .args(["analyze", "--format", "pcov"])
                .output()
                .expect("pcov-rs analyze failed");
            assert!(output.status.success(), "non-zero exit: {output:?}");
        });
    });
}

criterion_group!(benches, bench_full, bench_warm);
criterion_main!(benches);
