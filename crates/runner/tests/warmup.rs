//! `--warmup` runs an app-provided PHP file ONCE in the fork master, before any
//! child is forked, so workers inherit its warm state via copy-on-write. This
//! test asserts the master actually executes the warmup file (the perf win it
//! enables — collapsing each worker's cold framework-kernel boot — is measured
//! separately; here we only guard that the hook fires in the master).

use proust::fork_pool::PhpForkPool;
use std::collections::HashMap;
use std::time::Duration;

#[test]
fn warmup_script_runs_in_the_master() {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sample_project");
    let autoload = project.join("vendor/autoload.php");
    let script = proust::php_worker::find_fork_script().expect("worker_fork.php not found");

    let dir = std::env::temp_dir().join(format!("proust_warmup_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sentinel = dir.join("warmup_fired");
    let warmup = dir.join("warmup.php");
    // The warmup file simply records that it ran. A real warmup would boot the
    // app kernel here so forked workers inherit the warm class table.
    std::fs::write(
        &warmup,
        format!(
            "<?php file_put_contents({:?}, '1');\n",
            sentinel.to_str().unwrap()
        ),
    )
    .unwrap();

    let _pool = PhpForkPool::spawn(
        &script,
        &autoload,
        None,
        &[],
        &[],
        &[],
        &[],
        &[],
        1,
        &HashMap::new(),
        "512M",
        0,
        None,
        Some(warmup.as_path()),
    )
    .expect("spawn with --warmup");

    // The master runs the warmup before forking; poll briefly for the sentinel.
    let mut fired = false;
    for _ in 0..150 {
        if sentinel.exists() {
            fired = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        fired,
        "warmup file must be executed in the fork master before workers start"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
