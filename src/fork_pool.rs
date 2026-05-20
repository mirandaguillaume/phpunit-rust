//! Fork-based PHP worker pool. One PHP master loads autoloader + bootstrap,
//! then forks N children via pcntl_fork(). Each child gets ONE BatchPlan JSON
//! on its stdin pipe and streams TestOutcome lines back on its stdout pipe.
//! No per-class round-trips. No describe phase.

use crate::types::BatchPlan;
use anyhow::{anyhow, Context, Result};
use std::io::{BufReader, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub struct PhpForkPool {
    master: Child,
    write_ends: Vec<Option<std::fs::File>>,
    read_ends:  Vec<Option<std::fs::File>>,
}

impl PhpForkPool {
    /// Spawn a PHP fork-master with `n` worker slots.
    ///
    /// FD inheritance: `libc::pipe()` creates FDs without CLOEXEC; PHP-facing
    /// FDs survive execv automatically. We set CLOEXEC on Rust-facing FDs in
    /// the parent *before* spawn so the OS clears them on execv.
    pub fn spawn(
        script: &Path,
        autoload: &Path,
        bootstrap: Option<&Path>,
        defines: &[[String; 2]],
        n: usize,
    ) -> Result<Self> {
        if n == 0 {
            return Err(anyhow!("fork pool requires at least 1 slot"));
        }

        let mut to_php_read:    Vec<RawFd> = Vec::with_capacity(n);
        let mut to_php_write:   Vec<RawFd> = Vec::with_capacity(n);
        let mut from_php_read:  Vec<RawFd> = Vec::with_capacity(n);
        let mut from_php_write: Vec<RawFd> = Vec::with_capacity(n);

        for _ in 0..n {
            let [r, w] = raw_pipe()?;
            to_php_read.push(r);
            to_php_write.push(w);
            let [r, w] = raw_pipe()?;
            from_php_read.push(r);
            from_php_write.push(w);
        }

        // Mark Rust-facing FDs close-on-exec in the *parent* before spawn.
        // The OS will close them automatically during execv in the child.
        // PHP-facing FDs have no CLOEXEC flag and survive execv unchanged.
        unsafe {
            for &fd in to_php_write.iter().chain(from_php_read.iter()) {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }

        let stdin_fds_str = to_php_read.iter()
            .map(|fd| fd.to_string()).collect::<Vec<_>>().join(",");
        let stdout_fds_str = from_php_write.iter()
            .map(|fd| fd.to_string()).collect::<Vec<_>>().join(",");

        let mut cmd = Command::new("php");
        cmd.arg(script)
           .arg("--autoload").arg(autoload)
           .arg("--child-stdin-fds").arg(&stdin_fds_str)
           .arg("--child-stdout-fds").arg(&stdout_fds_str)
           .stdin(Stdio::null())
           .stdout(Stdio::null())
           .stderr(Stdio::inherit());

        if let Some(bs) = bootstrap {
            cmd.arg("--bootstrap").arg(bs);
        }
        if !defines.is_empty() {
            cmd.arg("--defines").arg(
                serde_json::to_string(defines).context("serializing defines")?
            );
        }

        let master = cmd.spawn().context("failed to spawn PHP master")?;

        // Close PHP-facing ends in Rust — PHP owns them from here on.
        for &fd in to_php_read.iter().chain(from_php_write.iter()) {
            unsafe { libc::close(fd) };
        }

        let write_ends = to_php_write.iter()
            .map(|&fd| Some(unsafe { std::fs::File::from_raw_fd(fd) }))
            .collect();
        let read_ends = from_php_read.iter()
            .map(|&fd| Some(unsafe { std::fs::File::from_raw_fd(fd) }))
            .collect();

        Ok(PhpForkPool { master, write_ends, read_ends })
    }

    /// Write a `BatchPlan` to slot `i`. Call before `close_write_ends()`.
    pub fn write_batch(&mut self, slot: usize, plan: &BatchPlan) -> Result<()> {
        let f = self.write_ends[slot].as_mut()
            .ok_or_else(|| anyhow!("write end for slot {slot} already closed"))?;
        let json = serde_json::to_string(plan).context("serializing BatchPlan")?;
        f.write_all(json.as_bytes()).context("writing BatchPlan")?;
        f.write_all(b"\n").context("writing BatchPlan newline")?;
        f.flush().context("flushing BatchPlan")?;
        Ok(())
    }

    /// Close all write ends, signalling EOF to each PHP child.
    /// Must be called before `into_readers()`.
    pub fn close_write_ends(&mut self) {
        for slot in self.write_ends.iter_mut() {
            *slot = None;
        }
    }

    /// Consume the read ends as `BufReader<File>` for rayon draining.
    /// Call only after `close_write_ends()`.
    pub fn into_readers(&mut self) -> Vec<BufReader<std::fs::File>> {
        // Ensure write ends are closed before we hand out readers.
        self.close_write_ends();
        std::mem::take(&mut self.read_ends)
            .into_iter()
            .filter_map(|opt| opt)
            .map(BufReader::new)
            .collect()
    }

    /// Wait for the PHP master process to exit cleanly.
    pub fn wait(&mut self) {
        let _ = self.master.wait();
    }
}

impl Drop for PhpForkPool {
    fn drop(&mut self) {
        self.close_write_ends();
        let _ = self.master.kill();
        let _ = self.master.wait();
    }
}

fn raw_pipe() -> Result<[RawFd; 2]> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(anyhow!(
            "pipe() failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(fds)
}
