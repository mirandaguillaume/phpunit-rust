use anyhow::{anyhow, Context, Result};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct FrankenPhp {
    child: Child,
    pub base_url: String,
}

impl FrankenPhp {
    /// Spawn FrankenPHP in worker mode bound to a free localhost port.
    /// `worker_script` must be an absolute path to `worker.php`.
    pub fn spawn(worker_script: &Path) -> Result<Self> {
        if !worker_script.is_file() {
            return Err(anyhow!("worker script not found: {}", worker_script.display()));
        }

        let port = find_free_port()?;
        let root = worker_script
            .parent()
            .ok_or_else(|| anyhow!("worker script has no parent dir"))?;

        let child = Command::new("frankenphp")
            .arg("php-server")
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--root")
            .arg(root)
            .arg("--worker")
            .arg(worker_script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn frankenphp; is it on $PATH?")?;

        let base_url = format!("http://127.0.0.1:{port}");
        let inst = FrankenPhp { child, base_url };
        inst.wait_until_ready(port, Duration::from_secs(10))?;
        Ok(inst)
    }

    fn wait_until_ready(&self, port: u16, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(anyhow!("frankenphp did not become ready within {timeout:?}"))
    }

    pub fn worker_url(&self) -> String {
        // FrankenPHP routes by file path in worker mode; the URL must reference
        // the worker script itself, not just `/`. Discovered during Task 4 smoke test.
        format!("{}/worker.php", self.base_url)
    }
}

impl Drop for FrankenPhp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

pub fn find_worker_script() -> Result<PathBuf> {
    // Try common locations relative to the binary's working dir.
    let candidates = [
        PathBuf::from("php/worker.php"),
        PathBuf::from("/home/gumiranda/PHPUnit_rust/php/worker.php"),
    ];
    for c in &candidates {
        if c.is_file() {
            return Ok(c.canonicalize()?);
        }
    }
    Err(anyhow!(
        "worker.php not found in any of: {:?}",
        candidates
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_free_port_returns_usable_port() {
        let port = find_free_port().unwrap();
        // Bind it again to confirm it's actually free.
        let _ = TcpListener::bind(("127.0.0.1", port)).unwrap();
    }
}
