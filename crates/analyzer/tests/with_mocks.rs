//! Integration test: mocks fixture.
//!
//! Verifies two Phase 2 invariants:
//!  1. Interface methods routed through a mock are NOT marked covered (correct
//!     semantic for both runtime tools and our static analyzer).
//!  2. Directly-instantiated production classes DO appear in coverage —
//!     `new UserService($repo)` causes the analyzer to recurse into
//!     UserService::__construct and attribute its lines to testRename.

use assert_cmd::Command;

mod common;

const FIXTURE_NAME: &str = "with_mocks";

/// Run `pcov-rs analyze` in `work_dir`. Each caller passes a private
/// `isolated_fixture` copy so the analyzer's `.pcov-rs/` cache is per-test.
/// This used to retry 3× to paper over a shared-cache flake (two tests running
/// in the same tracked fixture dir raced on that cache); isolating the working
/// directory removes the root cause, so the retry is gone.
fn run_analyze(work_dir: &std::path::Path) -> std::process::Output {
    Command::cargo_bin("pcov-rs")
        .unwrap()
        .current_dir(work_dir)
        .args(["analyze", "--format", "pcov-extended"])
        .output()
        .unwrap()
}

#[test]
fn mocks_fixture_runs_clean() {
    let wd = common::isolated_fixture(FIXTURE_NAME);
    let out = run_analyze(wd.path());
    assert!(
        out.status.success(),
        "pcov-rs analyze failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn mocks_do_not_cover_interface_methods() {
    let wd = common::isolated_fixture(FIXTURE_NAME);
    let output = run_analyze(wd.path());
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let map = parsed
        .as_object()
        .expect("expected JSON object at top level");

    // Test method body should be covered.
    let has_test_file = map.keys().any(|k| k.ends_with("UserServiceTest.php"));
    assert!(
        has_test_file,
        "expected UserServiceTest.php in output; keys: {:?}",
        map.keys().collect::<Vec<_>>()
    );

    // Interface methods of UserRepository must NOT be covered — mock dispatch
    // is opaque, which is the correct semantic (same as PCov/Xdebug at runtime).
    let has_repo_coverage = map
        .keys()
        .any(|k| k.ends_with("UserRepository.php") && !k.contains("Test"));
    assert!(
        !has_repo_coverage,
        "interface methods must never be marked covered via mock dispatch; keys: {:?}",
        map.keys().collect::<Vec<_>>()
    );

    // Phase 2: UserService is instantiated directly with `new UserService($repo)`,
    // so the analyzer recurses into UserService::__construct and attributes its
    // constructor line to testRename.
    let has_service_coverage = map
        .keys()
        .any(|k| k.ends_with("UserService.php") && !k.contains("Test"));
    assert!(has_service_coverage,
        "Phase 2: UserService.php should appear in coverage (direct instantiation recurses into __construct); keys: {:?}",
        map.keys().collect::<Vec<_>>());

    // Verify the UserService lines are attributed to testRename.
    let service_key = map
        .keys()
        .find(|k| k.ends_with("UserService.php") && !k.contains("Test"))
        .expect("UserService.php must be present");
    let service_lines = map[service_key]
        .as_object()
        .expect("UserService.php entry must be a line-map");
    assert!(
        service_lines.values().any(|tests| {
            tests
                .as_array()
                .map(|a| {
                    a.iter().any(|v| {
                        v.as_str()
                            .map(|s| s.contains("testRename"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        }),
        "UserService.php lines should be attributed to testRename; got: {service_lines:?}"
    );
}
