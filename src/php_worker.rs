//! Plain PHP CLI worker pool. Each PhpWorker is a long-lived `php worker.php`
//! child process talking JSON-per-line over stdin/stdout. No FrankenPHP,
//! no Caddy, no HTTP — Rust spawns the process, owns its stdio, kills it on
//! Drop. Thread-safe access from rayon via a `Mutex` around the (stdin,
//! stdout) pair.

use anyhow::{anyhow, Context, Result};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

/// One long-lived PHP worker process. Holds the Child (for lifecycle) and a
/// Mutex around its stdio so callers can safely write a request line and
/// read a response line atomically. The Mutex is defensive — rayon's thread
/// pool is sized to match worker count so each thread normally hits its own
/// worker, but the Mutex prevents byte-level interleaving if a future change
/// ever fans more work onto fewer workers.
pub struct PhpWorker {
    child: Child,
    stdio: Mutex<Stdio2>,
}

struct Stdio2 {
    writer: BufWriter<ChildStdin>,
    reader: BufReader<ChildStdout>,
}

impl PhpWorker {
    /// Spawn one `php worker.php` child. Returns when the process is up;
    /// does NOT wait for autoload to complete (the first request pays that
    /// cost). The worker is ready to receive its first JSON line as soon
    /// as spawn() returns.
    pub fn spawn(worker_script: &Path) -> Result<Self> {
        if !worker_script.is_file() {
            return Err(anyhow!("worker script not found: {}", worker_script.display()));
        }
        let mut child = Command::new("php")
            .arg(worker_script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())  // discard worker stderr (warnings, deprecations)
            .spawn()
            .context("failed to spawn `php`; is PHP on $PATH?")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout pipe"))?;
        Ok(PhpWorker {
            child,
            stdio: Mutex::new(Stdio2 {
                writer: BufWriter::new(stdin),
                reader: BufReader::new(stdout),
            }),
        })
    }

    /// Send one request line + read one response line, atomically with
    /// respect to other callers of this same worker.
    pub fn round_trip(&self, json: &str) -> Result<String> {
        let mut stdio = self.stdio.lock().expect("worker stdio mutex poisoned");
        stdio.writer.write_all(json.as_bytes()).context("writing request")?;
        stdio.writer.write_all(b"\n").context("writing newline")?;
        stdio.writer.flush().context("flushing request")?;
        let mut line = String::new();
        let bytes = stdio.reader.read_line(&mut line).context("reading response")?;
        if bytes == 0 {
            return Err(anyhow!("worker closed stdout (process exited?)"));
        }
        Ok(line)
    }
}

impl Drop for PhpWorker {
    fn drop(&mut self) {
        // Closing stdin causes the PHP while-loop to terminate naturally.
        // Take ownership of stdio so the writer drops (closing stdin).
        // Then wait for the child to exit so we don't leave zombies.
        // If wait() blocks longer than reasonable, fall back to kill.
        if let Ok(mut stdio) = self.stdio.lock() {
            // Drop the writer by replacing it with a no-op closed writer
            // — actually simpler: just close by dropping the Mutex contents
            // on guard drop. The writer's Drop won't close stdin until
            // we drop the BufWriter; but it will when the Mutex drops.
            let _ = stdio.writer.flush();
        }
        // Try a clean wait first; if the child hangs (shouldn't, since
        // stdin closure ends the loop), force-kill.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A pool of N PhpWorker processes. Drop kills all children.
pub struct PhpWorkerPool {
    workers: Vec<PhpWorker>,
}

impl PhpWorkerPool {
    pub fn spawn(worker_script: &Path, n: usize) -> Result<Self> {
        if n == 0 {
            return Err(anyhow!("worker pool needs at least 1 worker (got 0)"));
        }
        let mut workers = Vec::with_capacity(n);
        for _ in 0..n {
            workers.push(PhpWorker::spawn(worker_script)?);
        }
        Ok(PhpWorkerPool { workers })
    }

    pub fn worker(&self, idx: usize) -> &PhpWorker {
        &self.workers[idx % self.workers.len()]
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }
}

/// Find a usable `worker.php` relative to the binary's directory layout.
/// Same heuristic as the old find_worker_script.
pub fn find_worker_script() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.push(PathBuf::from("php/worker.php"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../../php/worker.php"));
            candidates.push(dir.join("php/worker.php"));
        }
    }
    for c in &candidates {
        if c.is_file() {
            return Ok(c.canonicalize()?);
        }
    }
    Err(anyhow!("worker.php not found in any of: {:?}", candidates))
}

/// Find `worker_fork.php` relative to the binary's directory layout.
pub fn find_fork_script() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("php/worker_fork.php")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../../php/worker_fork.php"));
            candidates.push(dir.join("php/worker_fork.php"));
        }
    }
    for c in &candidates {
        if c.is_file() {
            return Ok(c.canonicalize()?);
        }
    }
    Err(anyhow!("worker_fork.php not found in any of: {:?}", candidates))
}

/// Verify `php` is on $PATH and at least 8.1. Errors clearly otherwise.
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
            id, min_version_id
        ));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_php_version_ok_for_modern_php() {
        // Whatever system PHP is on the test host, 80000 should be safe.
        let id = check_php_version(70000).expect("php must be on PATH and >= 7.0");
        assert!(id >= 70000, "PHP_VERSION_ID was {id}");
    }

    #[test]
    fn check_php_version_errs_when_too_old() {
        // 99999999 is "PHP 99.999.999" — no real php will be that new.
        let err = check_php_version(990_000_00).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("too old") || s.contains("99"), "got: {s}");
    }
}
