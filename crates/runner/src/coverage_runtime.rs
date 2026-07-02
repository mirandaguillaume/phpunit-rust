//! Delegated runtime coverage — Rust orchestration only.
//!
//! Coverage is collected INSIDE the PHP workers via php-code-coverage (the same
//! library PHPUnit uses — see `php/src/Coverage.php`). The Rust runner does not
//! touch a single line of coverage data; it only:
//!   1. probes that a driver (pcov/xdebug) is available,
//!   2. hands every worker a directory to drop its per-worker `.cov` file in (via
//!      the inherited `PROUST_COVERAGE_DIR` env var), and
//!   3. after the run, invokes `php/merge_coverage.php` to merge those files with
//!      the library's own `CodeCoverage::merge()` and write the reports with the
//!      library's own writers.
//!
//! Parity with `phpunit --coverage-*` is therefore by construction.

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One requested coverage report: a format and where it goes. A `None` target for
/// the `text` format means "print to stdout".
#[derive(Debug, Clone)]
pub struct ReportTarget {
    pub format: String,
    pub target: Option<PathBuf>,
}

/// Whether `php` has a line-coverage driver (pcov or xdebug) loaded.
#[must_use]
pub fn driver_available(php: &str) -> bool {
    Command::new(php)
        .arg("-r")
        .arg("echo (extension_loaded('pcov') || extension_loaded('xdebug')) ? '1' : '0';")
        .output()
        .ok()
        .is_some_and(|o| o.stdout.first() == Some(&b'1'))
}

/// The `.cov` files a run produced in `cov_dir`.
fn collect_cov_files(cov_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(cov_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cov"))
        .collect()
}

/// Merge the per-worker coverage in `cov_dir` and emit `reports` by invoking
/// `merge_coverage.php` (JSON request on stdin, JSON result on stdout — the same
/// contract as the DB-provisioning helper). Prints the text report to stdout when
/// its target is `None`.
pub fn merge_and_emit(
    merge_script: &Path,
    autoload: &Path,
    cov_dir: &Path,
    reports: &[ReportTarget],
) -> Result<()> {
    let files = collect_cov_files(cov_dir);
    if files.is_empty() {
        return Err(anyhow!(
            "no coverage was collected (no worker produced a .cov file)"
        ));
    }

    let request = serde_json::json!({
        "files": files.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
        "reports": reports.iter().map(|r| serde_json::json!({
            "format": r.format,
            "target": r.target.as_ref().map(|t| t.to_string_lossy()),
        })).collect::<Vec<_>>(),
    });

    let mut child = Command::new("php")
        .arg(merge_script)
        .arg("--autoload")
        .arg(autoload)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning merge_coverage.php")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(request.to_string().as_bytes())
            .context("writing request to merge_coverage.php")?;
    }
    let output = child
        .wait_with_output()
        .context("waiting for merge_coverage.php")?;

    let text = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value = serde_json::from_str(text.trim())
        .with_context(|| format!("parsing merge_coverage.php output: {text:?}"))?;
    if result.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(anyhow!(
            "coverage merge failed: {}",
            result
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
        ));
    }
    if let Some(rendered) = result.get("text").and_then(serde_json::Value::as_str) {
        print!("{rendered}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cov_dir_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = merge_and_emit(
            Path::new("/nonexistent/merge.php"),
            Path::new("/nonexistent/autoload.php"),
            dir.path(),
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("no coverage was collected"));
    }

    #[test]
    fn collect_cov_files_picks_only_dot_cov() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0-1.cov"), "x").unwrap();
        std::fs::write(dir.path().join("1-2.cov"), "y").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "z").unwrap();
        let mut got: Vec<_> = collect_cov_files(dir.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, vec!["0-1.cov".to_string(), "1-2.cov".to_string()]);
    }
}
