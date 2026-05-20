use std::fs;
use std::path::Path;
use std::time::SystemTime;

pub const DEFAULT_MAX_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Debug)]
pub struct GcStats {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub entries_removed: u64,
}

pub fn prune(cache_root: &Path, max_bytes: u64) -> std::io::Result<GcStats> {
    let mut entries: Vec<(SystemTime, u64, std::path::PathBuf)> = Vec::new();
    let mut total = 0;

    for entry in walkdir(cache_root)? {
        let meta = fs::metadata(&entry)?;
        if !meta.is_file() { continue; }
        let size = meta.len();
        // Falls back to UNIX_EPOCH if atime is unavailable (e.g. noatime mounts).
        // On such filesystems, LRU eviction effectively degrades to FIFO order.
        let accessed = meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
        total += size;
        entries.push((accessed, size, entry));
    }

    let bytes_before = total;
    if total <= max_bytes {
        return Ok(GcStats { bytes_before, bytes_after: total, entries_removed: 0 });
    }

    // Sort newest-first so pop() yields the oldest in O(1).
    entries.sort_by_key(|(t, _, _)| std::cmp::Reverse(*t));

    let mut removed = 0;
    while total > max_bytes {
        let Some((_, size, path)) = entries.pop() else { break };
        fs::remove_file(&path)?;
        total -= size;
        removed += 1;
    }

    Ok(GcStats { bytes_before, bytes_after: total, entries_removed: removed })
}

fn walkdir(root: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if !dir.exists() { continue; }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() { stack.push(path); } else { out.push(path); }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_op_under_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), [0u8; 100]).unwrap();
        let stats = prune(dir.path(), 1000).unwrap();
        assert_eq!(stats.entries_removed, 0);
        assert!(dir.path().join("a.bin").exists());
    }

    #[test]
    fn evicts_when_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.bin"), [0u8; 100]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join("new.bin"), [0u8; 100]).unwrap();
        let stats = prune(dir.path(), 100).unwrap();
        assert!(stats.entries_removed >= 1);
    }
}
