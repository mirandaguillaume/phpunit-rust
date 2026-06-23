//! Shared helpers for analyzer integration tests.
//!
//! `pcov-rs analyze` writes a `.pcov-rs/cache/` store into its project root (the
//! working directory). Pointing several tests at one tracked fixture dir
//! therefore (a) litters `tests/fixtures/**` with cache dirs and (b) races on
//! that shared cache when cargo runs the tests in parallel — the documented
//! flake. Each test instead copies its fixture into a private temp dir via
//! `isolated_fixture` and runs the analyzer there: a fresh cache per test, and
//! the directory is removed when the returned `TempDir` drops.

use std::path::Path;
use tempfile::TempDir;

/// Copy `tests/fixtures/<name>` into a fresh temp dir and return its handle.
/// Keep the handle alive for the whole test (drop = cleanup); pass `.path()`
/// as the analyzer's working directory.
pub fn isolated_fixture(name: &str) -> TempDir {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let tmp = tempfile::Builder::new()
        .prefix("pcov-fixture-")
        .tempdir()
        .expect("create temp dir for fixture");
    copy_dir_all(&src, tmp.path());
    tmp
}

/// Recursively copy `src` into `dst`, skipping any stale `.pcov-rs` cache dir so
/// the isolated copy always starts cold.
fn copy_dir_all(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dst dir");
    for entry in std::fs::read_dir(src).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        if entry.file_name() == ".pcov-rs" {
            continue;
        }
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir_all(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).expect("copy fixture file");
        }
    }
}
