//! P3 resource lease. Modeled on `provider_enum.rs`: a short-lived `php`
//! helper (`provision_db.php`) is spawned once with stdin-JSON and gated on
//! exit status. Builds a Postgres template DB once, clones it per worker
//! slot via `CREATE DATABASE ..._w{slot} TEMPLATE`, and returns each clone's
//! DSN. Every clone name is recorded in a `ResourceLease` registry built
//! BEFORE the master forks, so teardown never depends on a live child.
//!
//! Cleanup model: graceful exit, `?` early-return, and panic-unwind are all
//! covered by [`LeaseGuard`]'s `Drop`, which runs [`ResourceLease::destroy_all`].
//! Ctrl-C / SIGKILL cleanup is handled by the P4 startup GC sweep (drops stale
//! clones by `run_uuid` prefix). We deliberately do NOT install a SIGINT
//! handler — doing real cleanup (spawning `php`, allocating, I/O) from a signal
//! handler is async-signal-unsafe and can deadlock or invoke UB.
//!
//! NOTE: this module is not yet wired into `main.rs` or the run path (Task 8).

#[allow(dead_code)]
use anyhow::{anyhow, Context, Result};
#[allow(dead_code)]
use serde::Deserialize;
#[allow(dead_code)]
use std::io::Write;
#[allow(dead_code)]
use std::path::{Path, PathBuf};
#[allow(dead_code)]
use std::process::{Command, Stdio};

/// Max length of a Postgres identifier (NAMEDATALEN - 1). Clone names MUST fit
/// or Postgres silently truncates them — which could collapse two distinct
/// slots onto the same database (cross-slot data bleed).
const PG_MAX_IDENT: usize = 63;

/// Map every char not in `[A-Za-z0-9_]` to `_`. Keeps clone names safe to
/// interpolate into DDL and free of surprising characters.
fn sanitize_ident(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Deterministic clone name for a slot: `{base_db}_{run_uuid}_w{slot}`, with
/// every component sanitized to `[A-Za-z0-9_]` and the whole bounded to 63
/// bytes (Postgres NAMEDATALEN). `base` is the TEMPLATE_DSN_BASE; we extract
/// its database name (the path component after the last `/`) to derive the
/// clone identifier. The registry only needs the name, not the DSN.
///
/// The `_w{slot}` suffix ALWAYS survives. When the natural name exceeds 63
/// bytes we truncate the variable `{db}_{run_uuid}` prefix and splice in the
/// first 8 hex of a hash of the FULL unsanitized name, so distinct inputs that
/// would otherwise truncate to the same string still get distinct names.
#[must_use]
pub fn clone_name(base: &str, run_uuid: &str, slot: usize) -> String {
    let raw_db = base.rsplit('/').next().unwrap_or(base);
    let db = sanitize_ident(raw_db);
    let uuid = sanitize_ident(run_uuid);

    let suffix = format!("_w{slot}");
    let natural = format!("{db}_{uuid}{suffix}");
    if natural.len() <= PG_MAX_IDENT {
        return natural;
    }

    // Too long: keep `_w{slot}` and a deterministic 8-hex hash of the full
    // unsanitized input (so collisions across distinct inputs are impossible),
    // then fill the remaining budget with as much sanitized prefix as fits.
    let mut hasher = blake3::Hasher::new();
    hasher.update(raw_db.as_bytes());
    hasher.update(b"/");
    hasher.update(run_uuid.as_bytes());
    hasher.update(b"/");
    hasher.update(slot.to_string().as_bytes());
    let hash = hasher.finalize().to_hex();
    let hash8: String = hash.chars().take(8).collect();
    let tail = format!("_{hash8}{suffix}");

    let prefix_budget = PG_MAX_IDENT.saturating_sub(tail.len());
    let prefix: String = db.chars().take(prefix_budget).collect();
    format!("{prefix}{tail}")
}

/// Wire shape returned by provision_db.php for build/clone/drop/gc actions.
/// `dsn` is the connection string the worker should use (null for `drop`/`gc`);
/// `dropped` is the list of clone names reclaimed by the `gc` action;
/// `error` is set (and the process exits non-zero) on hard failure.
#[derive(Debug, Deserialize)]
struct ProvisionResult {
    dsn: Option<String>,
    #[serde(default)]
    dropped: Vec<String>,
    #[allow(dead_code)]
    error: Option<String>,
}

/// Spawn `php provision_db.php` with one JSON request on stdin, gate on exit
/// status, parse one JSON `ProvisionResult` from stdout. Mirrors
/// `provider_enum::enumerate`'s spawn-once / stdin-JSON / gate-on-exit shape.
fn run_helper(
    script: &Path,
    autoload: &Path,
    bootstrap: Option<&Path>,
    defines: &[[String; 2]],
    request: &serde_json::Value,
) -> Result<ProvisionResult> {
    let mut cmd = Command::new("php");
    cmd.arg(script)
        .arg("--autoload")
        .arg(autoload)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(bs) = bootstrap {
        cmd.arg("--bootstrap").arg(bs);
    }
    if !defines.is_empty() {
        cmd.arg("--defines")
            .arg(serde_json::to_string(defines).context("serializing defines")?);
    }
    let mut child = cmd.spawn().context("spawning provision_db.php")?;
    let json = serde_json::to_string(request).context("serializing provision request")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(json.as_bytes())
            .context("writing request to provision_db.php stdin")?;
    }
    let output = child
        .wait_with_output()
        .context("waiting for provision_db.php")?;
    if !output.status.success() {
        // Required-resource build/clone failure HARD-FAILS: never degrade to
        // an empty/unprovisioned run.
        return Err(anyhow!(
            "provision_db.php exited with status {} for request {}",
            output.status,
            request
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim())
        .with_context(|| format!("parsing provision_db.php output: {text:?}"))
}

/// Build the resource template ONCE (migrate/seed amortized). Returns the
/// template DB name the per-slot clones will `CREATE DATABASE ... TEMPLATE`
/// from. Hard-fails on error.
pub fn build_template(
    script: &Path,
    autoload: &Path,
    bootstrap: Option<&Path>,
    defines: &[[String; 2]],
    base: &str,
) -> Result<String> {
    let req = serde_json::json!({"action": "build_template", "base": base});
    let res = run_helper(script, autoload, bootstrap, defines, &req)?;
    res.dsn
        .ok_or_else(|| anyhow!("provision_db.php build_template returned no template name"))
}

/// Clone the template for one worker slot via `CREATE DATABASE ..._w{slot}
/// TEMPLATE template`. Returns the clone's DSN to inject as PHPUNIT_RUST_DB_DSN
/// for that slot. Hard-fails on error.
#[allow(clippy::too_many_arguments)]
pub fn clone_for_slot(
    script: &Path,
    autoload: &Path,
    bootstrap: Option<&Path>,
    defines: &[[String; 2]],
    slot: usize,
    run_uuid: &str,
    template: &str,
    base: &str,
) -> Result<String> {
    let name = clone_name(base, run_uuid, slot);
    let req = serde_json::json!({
        "action": "clone",
        "base": base,
        "template": template,
        "clone_name": name,
    });
    let res = run_helper(script, autoload, bootstrap, defines, &req)?;
    res.dsn
        .ok_or_else(|| anyhow!("provision_db.php clone returned no DSN for slot {slot}"))
}

/// Best-effort startup GC sweep: ask `provision_db.php` to drop any clone
/// databases that match `<base_db>_pr<N>_w<N>` and have zero active Postgres
/// backends (meaning the run that created them is gone). Called BEFORE
/// `build_template` so the current run starts with a clean slate.
///
/// Returns `Ok(n)` where `n` is the number of stale clones reclaimed.
/// Returns `Err` if the helper process cannot be spawned or exits non-zero;
/// the caller is expected to swallow the error and continue (best-effort).
pub fn gc_stale_clones(
    script: &Path,
    autoload: &Path,
    bootstrap: Option<&Path>,
    defines: &[[String; 2]],
    base: &str,
) -> Result<usize> {
    let req = serde_json::json!({"action": "gc", "base": base});
    let res = run_helper(script, autoload, bootstrap, defines, &req)?;
    Ok(res.dropped.len())
}

/// Pre-fork registry of every clone name created for this run. Built BEFORE
/// the master forks so teardown is authoritative even if every child is
/// SIGKILLed. Holds enough to re-invoke `provision_db.php` with a `drop`
/// request per name.
pub struct ResourceLease {
    script: PathBuf,
    autoload: PathBuf,
    bootstrap: Option<PathBuf>,
    defines: Vec<[String; 2]>,
    base: String,
    /// Every registered clone name. Public for the registry tests.
    pub names: Vec<String>,
}

impl ResourceLease {
    pub fn new(
        script: PathBuf,
        autoload: PathBuf,
        bootstrap: Option<PathBuf>,
        defines: Vec<[String; 2]>,
        base: String,
    ) -> Self {
        Self {
            script,
            autoload,
            bootstrap,
            defines,
            base,
            names: Vec::new(),
        }
    }

    /// Record a clone name. Called once per slot, BEFORE the fork.
    pub fn register(&mut self, name: String) {
        self.names.push(name);
    }

    /// `DROP DATABASE IF EXISTS` every registered clone (idempotent). Wired to
    /// `wait()`/`terminate()` (via the `LeaseGuard` held until after them).
    /// Best-effort: logs but NEVER panics, so it is safe to call from a `Drop`.
    pub fn destroy_all(&self) {
        for name in &self.names {
            let req = serde_json::json!({
                "action": "drop",
                "base": &self.base,
                "clone_name": name,
            });
            if let Err(e) = run_helper(
                &self.script,
                &self.autoload,
                self.bootstrap.as_deref(),
                &self.defines,
                &req,
            ) {
                eprintln!("warning: failed to drop clone database {name}: {e:#}");
            }
        }
    }

    /// Consume the lease into a [`LeaseGuard`] for RAII teardown. Convenience
    /// for the Task 8 main wiring; equivalent to `LeaseGuard::new(self)`.
    #[must_use]
    pub fn into_guard(self) -> LeaseGuard {
        LeaseGuard::new(self)
    }
}

/// RAII teardown. Holding this until AFTER `pool.wait()`/`pool.terminate()`
/// guarantees `destroy_all()` runs on the Ok path AND on any `?` early-return
/// or panic-unwind. Idempotent (`DROP DATABASE IF EXISTS`), so a P4 startup GC
/// sweep that already reclaimed a clone causes no harm.
///
/// There is deliberately NO SIGINT handler: doing real cleanup (spawning `php`,
/// allocating, I/O) from a signal handler is async-signal-unsafe. Ctrl-C /
/// SIGKILL leftovers are reclaimed by the P4 startup GC sweep instead.
pub struct LeaseGuard {
    lease: Option<ResourceLease>,
}

impl LeaseGuard {
    /// Wrap a lease for RAII teardown. Task 8 (main wiring) calls this after
    /// the registry is populated and holds the guard until after the pool exits.
    pub fn new(lease: ResourceLease) -> Self {
        LeaseGuard { lease: Some(lease) }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            lease.destroy_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_name_is_deterministic_and_slotted() {
        let n0 = clone_name("postgres://u@h/app", "pr123", 0);
        let n1 = clone_name("postgres://u@h/app", "pr123", 1);
        assert_eq!(n0, "app_pr123_w0");
        assert_eq!(n1, "app_pr123_w1");
        assert_ne!(n0, n1, "each slot must get a distinct clone");
        assert_eq!(
            n0,
            clone_name("postgres://u@h/app", "pr123", 0),
            "stable across calls"
        );
    }

    #[test]
    fn clone_name_sanitizes_special_chars() {
        let is_ident = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        for base in [
            "postgres://u@h/my\"db",
            "postgres://u@h/my-db",
            "postgres://u@h/a.b c",
        ] {
            let n = clone_name(base, "pr-1.x", 0);
            assert!(
                is_ident(&n),
                "clone name must be ^[A-Za-z0-9_]+$, got: {n:?}"
            );
            assert!(
                n.ends_with("_w0"),
                "the _w{{slot}} suffix must survive: {n:?}"
            );
        }
    }

    #[test]
    fn clone_name_is_bounded_and_distinct_for_long_inputs() {
        let long_db = "x".repeat(200);
        let base = format!("postgres://u@h/{long_db}");
        let uuid = "run".to_string() + &"y".repeat(200);
        let n0 = clone_name(&base, &uuid, 0);
        let n1 = clone_name(&base, &uuid, 1);
        // Postgres NAMEDATALEN bound: never exceed 63 bytes (else silent
        // truncation could collapse two slots onto the same database).
        assert!(
            n0.len() <= 63,
            "slot 0 over 63 bytes: {} ({n0:?})",
            n0.len()
        );
        assert!(
            n1.len() <= 63,
            "slot 1 over 63 bytes: {} ({n1:?})",
            n1.len()
        );
        // Distinct per slot even when truncated.
        assert_ne!(
            n0, n1,
            "long inputs must NOT collapse two slots onto one name"
        );
        assert!(
            n0.ends_with("_w0") && n1.ends_with("_w1"),
            "suffix survives: {n0:?} {n1:?}"
        );
        // Sanitized.
        assert!(
            n0.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "bounded name must still be ^[A-Za-z0-9_]+$: {n0:?}"
        );
    }

    #[test]
    fn run_helper_errors_when_php_script_missing() {
        use std::path::Path;
        let req =
            serde_json::json!({"action": "drop", "base": "postgres://u@h/app", "clone_name": "x"});
        let res = run_helper(
            Path::new("/does/not/exist/provision_db.php"),
            Path::new("/does/not/exist/autoload.php"),
            None,
            &[],
            &req,
        );
        assert!(
            res.is_err(),
            "missing/failing helper must hard-fail, not degrade"
        );
    }

    #[test]
    fn build_and_clone_hard_fail_without_helper() {
        use std::path::Path;
        let script = Path::new("/does/not/exist/provision_db.php");
        let autoload = Path::new("/does/not/exist/autoload.php");
        let bt = build_template(script, autoload, None, &[], "postgres://u@h/app");
        assert!(
            bt.is_err(),
            "build_template must hard-fail without a usable helper"
        );
        let cl = clone_for_slot(
            script,
            autoload,
            None,
            &[],
            0,
            "pr1",
            "app",
            "postgres://u@h/app",
        );
        assert!(
            cl.is_err(),
            "clone_for_slot must hard-fail without a usable helper"
        );
    }

    #[test]
    fn gc_stale_clones_errs_when_helper_missing() {
        // Without a real Postgres connection gc_stale_clones must Err (the
        // helper binary is absent) — the caller swallows this and continues,
        // so the error path must be reachable and not panic.
        use std::path::Path;
        let res = gc_stale_clones(
            Path::new("/does/not/exist/provision_db.php"),
            Path::new("/does/not/exist/autoload.php"),
            None,
            &[],
            "postgres://u@h/app",
        );
        assert!(
            res.is_err(),
            "gc_stale_clones must Err when the helper binary is missing"
        );
    }

    #[test]
    fn register_records_one_name_per_slot() {
        use std::path::PathBuf;
        let mut lease = ResourceLease::new(
            PathBuf::from("/x/provision_db.php"),
            PathBuf::from("/x/autoload.php"),
            None,
            vec![],
            "postgres://u@h/app".to_string(),
        );
        for slot in 0..4 {
            lease.register(clone_name("postgres://u@h/app", "pr1", slot));
        }
        assert_eq!(lease.names.len(), 4);
    }

    #[test]
    fn destroy_all_is_panic_free_when_empty() {
        use std::path::PathBuf;
        let empty = ResourceLease::new(
            PathBuf::from("/does/not/exist/provision_db.php"),
            PathBuf::from("/does/not/exist/autoload.php"),
            None,
            vec![],
            "postgres://u@h/app".to_string(),
        );
        // No registered names => no helper spawned => returns cleanly, no panic.
        empty.destroy_all();
    }

    #[test]
    fn lease_guard_drop_is_panic_free() {
        use std::path::PathBuf;
        let mut lease = ResourceLease::new(
            PathBuf::from("/does/not/exist/provision_db.php"),
            PathBuf::from("/does/not/exist/autoload.php"),
            None,
            vec![],
            "postgres://u@h/app".to_string(),
        );
        lease.register(clone_name("postgres://u@h/app", "pr1", 0));
        // Constructing then dropping the guard must invoke destroy_all (which
        // logs a warning for the missing helper) without panicking.
        let guard = LeaseGuard::new(lease);
        drop(guard);
    }
}
