//! P3 resource lease. Modeled on `provider_enum.rs`: a short-lived `php`
//! helper (`provision_db.php`) is spawned once with stdin-JSON and gated on
//! exit status. Builds a Postgres template DB once, clones it per worker
//! slot via `CREATE DATABASE ..._w{slot} TEMPLATE`, and returns each clone's
//! DSN. Every clone name is recorded in a `ResourceLease` registry built
//! BEFORE the master forks, so teardown never depends on a live child.
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
#[allow(dead_code)]
use std::sync::atomic::{AtomicBool, Ordering};
#[allow(dead_code)]
use std::sync::Mutex;

/// Deterministic clone name for a slot: `{base_db}_{run_uuid}_w{slot}`.
/// `base` is the TEMPLATE_DSN_BASE; we extract its database name (the path
/// component after the last `/`) to derive the clone identifier. The registry
/// only needs the name, not the DSN.
#[must_use]
pub fn clone_name(base: &str, run_uuid: &str, slot: usize) -> String {
    let db = base.rsplit('/').next().unwrap_or(base);
    format!("{db}_{run_uuid}_w{slot}")
}

/// Wire shape returned by provision_db.php for build/clone/drop actions.
/// `dsn` is the connection string the worker should use (null for `drop`);
/// `error` is set (and the process exits non-zero) on hard failure.
#[derive(Debug, Deserialize)]
struct ProvisionResult {
    dsn: Option<String>,
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
    /// `wait()`/`terminate()` (via the `LeaseGuard` held until after them) and
    /// to the SIGINT handler. Best-effort: logs but NEVER panics, so it is safe
    /// from a `Drop` and from a signal-handler context.
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
}

/// Deep-copy the registry so the SIGINT handler's global copy and the
/// RAII guard's copy are independent (both call `destroy_all`; the drop is
/// idempotent via `DROP DATABASE IF EXISTS`).
fn clone_registry(l: &ResourceLease) -> ResourceLease {
    ResourceLease {
        script: l.script.clone(),
        autoload: l.autoload.clone(),
        bootstrap: l.bootstrap.clone(),
        defines: l.defines.clone(),
        base: l.base.clone(),
        names: l.names.clone(),
    }
}

/// Process-global handle so a SIGINT handler (which takes no arguments) can
/// reach the registry. Set once, pre-fork, when provisioning is active.
static SIGINT_TRIPPED: AtomicBool = AtomicBool::new(false);
static GLOBAL_LEASE: Mutex<Option<ResourceLease>> = Mutex::new(None);

extern "C" fn handle_sigint(_sig: libc::c_int) {
    // Flip the flag, then opportunistically drop clones under a try_lock so a
    // plain Ctrl-C (where RAII Drop never runs) still cleans up. Re-raise the
    // default SIGINT so the process actually terminates.
    SIGINT_TRIPPED.store(true, Ordering::SeqCst);
    if let Ok(guard) = GLOBAL_LEASE.try_lock() {
        if let Some(lease) = guard.as_ref() {
            lease.destroy_all();
        }
    }
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::raise(libc::SIGINT);
    }
}

/// Install the SIGINT handler and stash a copy of the lease registry globally
/// so the handler can drop the clones on Ctrl-C (where RAII `Drop` never runs).
/// Returns a `LeaseGuard` whose `Drop` is the normal-path teardown.
pub fn install_sigint_handler(lease: ResourceLease) -> LeaseGuard {
    {
        let mut g = GLOBAL_LEASE.lock().expect("GLOBAL_LEASE poisoned");
        *g = Some(clone_registry(&lease));
    }
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as libc::sighandler_t);
    }
    LeaseGuard { lease: Some(lease) }
}

/// RAII teardown. Holding this until AFTER `pool.wait()`/`pool.terminate()`
/// guarantees `destroy_all()` runs on the Ok path AND on any `?` early-return
/// or panic-unwind. Idempotent with the SIGINT handler (`DROP IF EXISTS`).
pub struct LeaseGuard {
    lease: Option<ResourceLease>,
}

impl LeaseGuard {
    pub fn new(lease: ResourceLease) -> Self {
        install_sigint_handler(lease)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if SIGINT_TRIPPED.load(Ordering::SeqCst) {
            // The signal handler already dropped the clones.
            return;
        }
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
        assert_eq!(n0, clone_name("postgres://u@h/app", "pr123", 0), "stable across calls");
    }

    #[test]
    fn run_helper_errors_when_php_script_missing() {
        use std::path::Path;
        let req = serde_json::json!({"action": "drop", "base": "postgres://u@h/app", "clone_name": "x"});
        let res = run_helper(
            Path::new("/does/not/exist/provision_db.php"),
            Path::new("/does/not/exist/autoload.php"),
            None,
            &[],
            &req,
        );
        assert!(res.is_err(), "missing/failing helper must hard-fail, not degrade");
    }

    #[test]
    fn build_and_clone_hard_fail_without_helper() {
        use std::path::Path;
        let script = Path::new("/does/not/exist/provision_db.php");
        let autoload = Path::new("/does/not/exist/autoload.php");
        let bt = build_template(script, autoload, None, &[], "postgres://u@h/app");
        assert!(bt.is_err(), "build_template must hard-fail without a usable helper");
        let cl = clone_for_slot(script, autoload, None, &[], 0, "pr1", "app", "postgres://u@h/app");
        assert!(cl.is_err(), "clone_for_slot must hard-fail without a usable helper");
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
