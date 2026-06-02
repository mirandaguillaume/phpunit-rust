//! PHP process utilities: version probe and fork-script locator.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// Find `worker_fork.php` relative to the binary's directory layout.
pub fn find_fork_script() -> Result<PathBuf> {
    find_script_named("worker_fork.php")
}

/// Find `enumerate_providers.php` — same search path as the fork script.
pub fn find_enumerate_script() -> Result<PathBuf> {
    find_script_named("enumerate_providers.php")
}

fn find_script_named(name: &str) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from(format!("php/{name}"))];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(format!("../../php/{name}")));
            candidates.push(dir.join(format!("../../../php/{name}")));
            candidates.push(dir.join(format!("php/{name}")));
        }
    }
    for c in &candidates {
        if c.is_file() {
            return Ok(c.canonicalize()?);
        }
    }
    Err(anyhow!("{name} not found in any of: {:?}", candidates))
}

/// Verify `php` is on $PATH and at least `min_version_id`. Errors clearly otherwise.
pub fn check_php_version(min_version_id: u32) -> Result<u32> {
    let output = Command::new("php")
        .args(["-r", "echo PHP_VERSION_ID;"])
        .output()
        .context("running `php -r 'echo PHP_VERSION_ID;'`; is PHP on $PATH?")?;
    if !output.status.success() {
        return Err(anyhow!(
            "php exited with status {} when probing version",
            output.status
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let id: u32 = text
        .trim()
        .parse()
        .with_context(|| format!("PHP_VERSION_ID was not a number: {:?}", text))?;
    if id < min_version_id {
        return Err(anyhow!(
            "PHP version is too old: PHP_VERSION_ID={} (need >= {})",
            id,
            min_version_id
        ));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_php_version_ok_for_modern_php() {
        // Whatever system PHP is on the test host, 70000 should be safe.
        let id = check_php_version(70000).expect("php must be on PATH and >= 7.0");
        assert!(id >= 70000, "PHP_VERSION_ID was {id}");
    }

    #[test]
    fn check_php_version_errs_when_too_old() {
        // 99999999 is "PHP 99.999.999" — no real php will be that new.
        let err = check_php_version(99_000_000).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("too old") || s.contains("99"), "got: {s}");
    }
}
