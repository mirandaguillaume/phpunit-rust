//! Fork-based PHP worker pool. One PHP master loads autoloader + bootstrap,
//! then forks N children via pcntl_fork(). The master keeps each child's
//! stdin pipe open and streams newline-delimited BatchPlan JSONs to it
//! (work-stealing dispatch). Children stream TestOutcome lines back and
//! emit `{"batch_done": true}` between batches as a ready signal, exiting
//! when their stdin is closed (EOF).

use crate::types::BatchPlan;
use anyhow::{anyhow, Context, Result};
use std::io::{BufReader, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct PhpForkPool {
    master: Child,
    write_ends: Vec<Option<std::fs::File>>,
    read_ends: Vec<Option<std::fs::File>>,
    // Keep class-map temp file alive until the pool is dropped.
    _class_map_tmp: Option<tempfile::NamedTempFile>,
}

impl PhpForkPool {
    /// Spawn a PHP fork-master with `n` worker slots.
    ///
    /// Pipe pairs are wrapped in `File` immediately after creation so that
    /// any early return from this function automatically closes all allocated
    /// FDs. CLOEXEC errors are propagated rather than silently ignored.
    pub fn spawn(
        script: &Path,
        autoload: &Path,
        bootstrap: Option<&Path>,
        defines: &[[String; 2]],
        env: &[(String, String, bool)],
        server: &[[String; 2]],
        ini: &[[String; 2]],
        vars: &[[String; 2]],
        n: usize,
        class_map: &std::collections::HashMap<String, std::path::PathBuf>,
        worker_memory_limit: &str,
        max_batches_per_child: u32,
    ) -> Result<Self> {
        if n == 0 {
            return Err(anyhow!("fork pool requires at least 1 slot"));
        }

        // Create N stdin-pipes (Rust writes, PHP reads) and
        //        N stdout-pipes (PHP writes, Rust reads).
        // Wrap raw FDs into File immediately so early returns auto-close them.
        let mut to_php_read: Vec<std::fs::File> = Vec::with_capacity(n);
        let mut to_php_write: Vec<std::fs::File> = Vec::with_capacity(n);
        let mut from_php_read: Vec<std::fs::File> = Vec::with_capacity(n);
        let mut from_php_write: Vec<std::fs::File> = Vec::with_capacity(n);

        for _ in 0..n {
            let [r, w] = raw_pipe()?;
            to_php_read.push(unsafe { std::fs::File::from_raw_fd(r) });
            to_php_write.push(unsafe { std::fs::File::from_raw_fd(w) });
            let [r, w] = raw_pipe()?;
            from_php_read.push(unsafe { std::fs::File::from_raw_fd(r) });
            from_php_write.push(unsafe { std::fs::File::from_raw_fd(w) });
        }

        // Mark Rust-facing FDs close-on-exec so the OS closes them in the
        // PHP master process during execv. PHP-facing FDs have no CLOEXEC
        // and survive execv naturally (libc::pipe() default).
        unsafe {
            for f in to_php_write.iter().chain(from_php_read.iter()) {
                let fd = f.as_raw_fd();
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(anyhow!(
                        "fcntl(F_GETFD) failed for fd {fd}: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
                    return Err(anyhow!(
                        "fcntl(F_SETFD, CLOEXEC) failed for fd {fd}: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
        }

        let stdin_fds_str = to_php_read
            .iter()
            .map(|f| f.as_raw_fd().to_string())
            .collect::<Vec<_>>()
            .join(",");
        let stdout_fds_str = from_php_write
            .iter()
            .map(|f| f.as_raw_fd().to_string())
            .collect::<Vec<_>>()
            .join(",");

        let mut cmd = Command::new("php");
        cmd.arg("-d")
            .arg("opcache.enable_cli=1")
            // The master pre-warms opcache for every test file before forking,
            // so each compile bumps the master's resident set. On large test
            // suites (rector: ~1500 fixture files in class_file_index, phpstan
            // similar) the default 128M memory_limit fills up and PHP fatals
            // the master half-way through. -1 (unlimited) is safe here because
            // the OS still enforces real limits and the master is short-lived
            // — by design we strip the per-child cap via --worker-memory-limit.
            .arg("-d")
            .arg("memory_limit=-1")
            .arg(script)
            .arg("--autoload")
            .arg(autoload)
            .arg("--child-stdin-fds")
            .arg(&stdin_fds_str)
            .arg("--child-stdout-fds")
            .arg(&stdout_fds_str)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());

        // Linux: ask the kernel to deliver SIGTERM to the PHP master when
        // *we* die for any reason — including SIGKILL on Rust where Drop
        // never runs. The master's SIGTERM handler (in worker_fork.php)
        // then propagates the death to its forked children. Without this,
        // a worker stuck in setUp can outlive its parent for hours.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGTERM as libc::c_ulong,
                    0,
                    0,
                    0,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        if let Some(bs) = bootstrap {
            cmd.arg("--bootstrap").arg(bs);
        }
        if !defines.is_empty() {
            cmd.arg("--defines")
                .arg(serde_json::to_string(defines).context("serializing defines")?);
        }
        if !env.is_empty() {
            // Wire each <env> as [name, value, force?] triples so the
            // master can honour the `force` semantics (don't clobber a
            // shell-provided value unless the XML says so).
            let payload: Vec<(&str, &str, bool)> = env
                .iter()
                .map(|(n, v, f)| (n.as_str(), v.as_str(), *f))
                .collect();
            cmd.arg("--env")
                .arg(serde_json::to_string(&payload).context("serializing env")?);
        }
        if !server.is_empty() {
            cmd.arg("--server")
                .arg(serde_json::to_string(server).context("serializing server")?);
        }
        if !ini.is_empty() {
            cmd.arg("--ini")
                .arg(serde_json::to_string(ini).context("serializing ini")?);
        }
        if !vars.is_empty() {
            cmd.arg("--vars")
                .arg(serde_json::to_string(vars).context("serializing vars")?);
        }
        // Write class map to a temp file to avoid ARG_MAX limits.
        // The file is deleted when the pool is dropped (or when the process exits).
        let _class_map_tmp: Option<tempfile::NamedTempFile>;
        if !class_map.is_empty() {
            let map_str: std::collections::HashMap<&str, &str> = class_map
                .iter()
                .filter_map(|(k, v)| v.to_str().map(|s| (k.as_str(), s)))
                .collect();
            let json = serde_json::to_string(&map_str).context("serializing class-map")?;
            let mut tmp = tempfile::NamedTempFile::new().context("creating class-map temp file")?;
            use std::io::Write as _;
            tmp.write_all(json.as_bytes())
                .context("writing class-map temp file")?;
            cmd.arg("--class-map-file").arg(tmp.path());
            _class_map_tmp = Some(tmp);
        } else {
            _class_map_tmp = None;
        }
        cmd.arg("--worker-memory-limit").arg(worker_memory_limit);
        cmd.arg("--max-batches-per-child")
            .arg(max_batches_per_child.to_string());

        let master = cmd.spawn().context("failed to spawn PHP master")?;

        // Drop PHP-facing ends in Rust — PHP owns them from here on.
        // Dropping File closes the underlying FD.
        drop(to_php_read);
        drop(from_php_write);

        let write_ends = to_php_write.into_iter().map(Some).collect();
        let read_ends = from_php_read.into_iter().map(Some).collect();

        Ok(PhpForkPool {
            master,
            write_ends,
            read_ends,
            _class_map_tmp,
        })
    }

    /// Write a `BatchPlan` to slot `i`. Can be called multiple times per slot
    /// (the worker reads newline-delimited plans in a loop). Call `close_slot`
    /// when you have no more work for this slot.
    pub fn write_batch(&mut self, slot: usize, plan: &BatchPlan) -> Result<()> {
        let f = self.write_ends[slot]
            .as_mut()
            .ok_or_else(|| anyhow!("write end for slot {slot} already closed"))?;
        let json = serde_json::to_string(plan).context("serializing BatchPlan")?;
        f.write_all(json.as_bytes()).context("writing BatchPlan")?;
        f.write_all(b"\n").context("writing BatchPlan newline")?;
        f.flush().context("flushing BatchPlan")?;
        Ok(())
    }

    /// Close one slot's write end, signalling EOF to that PHP child so it
    /// exits its read loop cleanly. Idempotent.
    pub fn close_slot(&mut self, slot: usize) {
        if let Some(s) = self.write_ends.get_mut(slot) {
            *s = None;
        }
    }

    /// Close all write ends, signalling EOF to each PHP child.
    pub fn close_write_ends(&mut self) {
        for slot in self.write_ends.iter_mut() {
            *slot = None;
        }
    }

    /// Take ownership of a single slot's read end as a `BufReader`. Once
    /// taken, this slot's read end is no longer available from the pool.
    pub fn take_reader(&mut self, slot: usize) -> Option<BufReader<std::fs::File>> {
        self.read_ends.get_mut(slot)?.take().map(BufReader::new)
    }

    /// Consume the read ends as `BufReader<File>` for rayon draining.
    /// Call only after `close_write_ends()`.
    pub fn into_readers(&mut self) -> Vec<BufReader<std::fs::File>> {
        self.close_write_ends();
        std::mem::take(&mut self.read_ends)
            .into_iter()
            .filter_map(|opt| opt)
            .map(BufReader::new)
            .collect()
    }

    /// Returns the number of worker slots in this pool.
    pub fn len(&self) -> usize {
        self.write_ends.len()
    }

    /// Wait for the PHP master process to exit cleanly.
    pub fn wait(&mut self) {
        let _ = self.master.wait();
    }

    /// Forcibly tear the pool down *now*: close all write ends and SIGTERM the
    /// master so its handler kills every forked child's process group. Used by
    /// the runner's inactivity watchdog to unblock the per-slot reader threads
    /// — a stuck child never emits output, but once it is killed its pipe
    /// write-end closes and the reader sees EOF. Best-effort; `Drop` still runs
    /// the SIGTERM→SIGKILL escalation as a backstop. Safe to call before
    /// `wait()`.
    pub fn terminate(&mut self) {
        self.close_write_ends();
        let pid = self.master.id() as i32;
        // Try graceful first: in fork-server mode (`--worker-max-batches > 0`)
        // the master loops on `usleep`, so `pcntl_async_signals` can run its
        // SIGTERM handler and reap its children. Give it a brief window.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            match self.master.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => break,
            }
        }
        // Backstop for the *stuck-worker* case: in long-lived mode the master
        // blocks in a non-dispatching `pcntl_waitpid` and never runs its
        // SIGTERM handler, and even a SIGKILL of the master would leave a stuck
        // child orphaned and alive — keeping its stdout pipe open so the
        // runner's reader threads never see EOF. Kill the whole descendant
        // tree directly so every child's pipe closes.
        kill_process_tree(pid);
        let _ = self.master.kill();
        let _ = self.master.wait();
    }
}

impl Drop for PhpForkPool {
    fn drop(&mut self) {
        self.close_write_ends();
        // SIGTERM first so the master can run its handler (which posix_kills
        // every forked child). SIGKILL would bypass the handler and leave
        // grandchildren orphaned, blocked in setUp/test. Give it 500ms,
        // then SIGKILL as a fallback.
        let pid = self.master.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.master.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => break,
            }
        }
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

/// SIGKILL every transitive descendant of `root` (children, grandchildren, …)
/// and their process groups by walking `/proc`. Used to guarantee teardown of
/// a stuck worker that the PHP master won't reap on its own (it may be parked
/// in a non-dispatching `pcntl_waitpid`, so `pcntl_async_signals` never runs
/// its SIGTERM handler). Linux-only; a no-op elsewhere (the caller still
/// SIGKILLs the master directly).
#[cfg(target_os = "linux")]
fn kill_process_tree(root: i32) {
    fn ppid_of(stat: &str) -> Option<i32> {
        // /proc/<pid>/stat is "<pid> (comm) <state> <ppid> …". `comm` may
        // contain spaces and parens, so scan past the last ')'.
        let rparen = stat.rfind(')')?;
        stat[rparen + 1..].split_whitespace().nth(1)?.parse().ok()
    }
    let procs: Vec<(i32, i32)> = match std::fs::read_dir("/proc") {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()))
            .filter_map(|pid| {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                ppid_of(&stat).map(|pp| (pid, pp))
            })
            .collect(),
        Err(_) => return,
    };
    // BFS from `root` to collect every descendant.
    let mut descendants: Vec<i32> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for &(pid, ppid) in &procs {
            if ppid == parent && pid != root && !descendants.contains(&pid) {
                descendants.push(pid);
                frontier.push(pid);
            }
        }
    }
    for pid in descendants {
        // Negative pid targets the child's process group (catches grandchildren
        // a test spawned via proc_open); the bare pid covers any that did not
        // setpgid. Both are best-effort.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn kill_process_tree(_root: i32) {}
