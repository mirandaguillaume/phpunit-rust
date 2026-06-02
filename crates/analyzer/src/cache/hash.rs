use std::path::Path;

/// 32-byte BLAKE3 content hash, hex-encoded for filesystem use.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn of_file(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(Self::of_bytes(&bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Stable fingerprint of a project's source-boundary config.
    ///
    /// Folds the project `root` plus its `source_includes`, `source_excludes`,
    /// and `test_suites` paths into a single BLAKE3 hash. The three path lists
    /// are sorted before hashing so that ordering differences in `phpunit.xml`
    /// do not change the fingerprint, while any change to the *set* of boundary
    /// paths (which is what `opacity::decide` routes on) produces a new hash.
    ///
    /// This lets callers namespace derived caches (trace_v2, result) by config:
    /// editing `<source><include>/<exclude>` in `phpunit.xml` — or pointing
    /// `--config` at a file with different boundaries — invalidates any cache
    /// keyed off this fingerprint even when no PHP file mtime changed.
    pub fn of_config(
        root: &Path,
        source_includes: &[std::path::PathBuf],
        source_excludes: &[std::path::PathBuf],
        test_suites: &[std::path::PathBuf],
    ) -> Self {
        fn push_sorted_paths(buf: &mut Vec<u8>, paths: &[std::path::PathBuf]) {
            let mut sorted: Vec<&std::path::PathBuf> = paths.iter().collect();
            sorted.sort();
            for p in sorted {
                buf.extend_from_slice(p.as_os_str().to_string_lossy().as_bytes());
                buf.push(0);
            }
            // Group separator: keeps the boundary between sections unambiguous
            // so distinct lists can't collide via concatenation.
            buf.push(0x1e);
        }

        let mut buf = Vec::new();
        buf.extend_from_slice(b"config-v1\x00");
        buf.extend_from_slice(root.as_os_str().to_string_lossy().as_bytes());
        buf.push(0x1e);
        push_sorted_paths(&mut buf, source_includes);
        push_sorted_paths(&mut buf, source_excludes);
        push_sorted_paths(&mut buf, test_suites);
        Self::of_bytes(&buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_bytes() {
        assert_eq!(ContentHash::of_bytes(b"hello"), ContentHash::of_bytes(b"hello"));
    }

    #[test]
    fn differs_for_different_bytes() {
        assert_ne!(ContentHash::of_bytes(b"hello"), ContentHash::of_bytes(b"world"));
    }

    #[test]
    fn hashes_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();
        assert_eq!(ContentHash::of_file(&file).unwrap(), ContentHash::of_bytes(b"hello"));
    }

    #[test]
    fn config_fingerprint_is_deterministic() {
        use std::path::PathBuf;
        let root = PathBuf::from("/proj");
        let inc = vec![PathBuf::from("/proj/src")];
        let exc = vec![PathBuf::from("/proj/src/Migrations")];
        let suites = vec![PathBuf::from("/proj/tests")];
        assert_eq!(
            ContentHash::of_config(&root, &inc, &exc, &suites),
            ContentHash::of_config(&root, &inc, &exc, &suites),
        );
    }

    #[test]
    fn config_fingerprint_ignores_path_order() {
        use std::path::PathBuf;
        let root = PathBuf::from("/proj");
        let a = vec![PathBuf::from("/proj/src"), PathBuf::from("/proj/lib")];
        let b = vec![PathBuf::from("/proj/lib"), PathBuf::from("/proj/src")];
        let exc: Vec<PathBuf> = vec![];
        let suites = vec![PathBuf::from("/proj/tests")];
        assert_eq!(
            ContentHash::of_config(&root, &a, &exc, &suites),
            ContentHash::of_config(&root, &b, &exc, &suites),
            "reordering the same set of include paths must not change the fingerprint",
        );
    }

    #[test]
    fn config_fingerprint_differs_on_includes() {
        use std::path::PathBuf;
        let root = PathBuf::from("/proj");
        let exc: Vec<PathBuf> = vec![];
        let suites = vec![PathBuf::from("/proj/tests")];
        let inc_a = vec![PathBuf::from("/proj/src")];
        let inc_b = vec![PathBuf::from("/proj/src"), PathBuf::from("/proj/app")];
        assert_ne!(
            ContentHash::of_config(&root, &inc_a, &exc, &suites),
            ContentHash::of_config(&root, &inc_b, &exc, &suites),
            "changing source_includes must change the fingerprint",
        );
    }

    #[test]
    fn config_fingerprint_differs_on_excludes() {
        use std::path::PathBuf;
        let root = PathBuf::from("/proj");
        let inc = vec![PathBuf::from("/proj/src")];
        let suites = vec![PathBuf::from("/proj/tests")];
        let exc_a: Vec<PathBuf> = vec![];
        let exc_b = vec![PathBuf::from("/proj/src/Migrations")];
        assert_ne!(
            ContentHash::of_config(&root, &inc, &exc_a, &suites),
            ContentHash::of_config(&root, &inc, &exc_b, &suites),
            "changing source_excludes must change the fingerprint",
        );
    }

    #[test]
    fn config_fingerprint_does_not_alias_between_sections() {
        use std::path::PathBuf;
        // Moving a path from includes to excludes must change the fingerprint:
        // the section grouping must not let lists collide via concatenation.
        let root = PathBuf::from("/proj");
        let suites: Vec<PathBuf> = vec![];
        let p = PathBuf::from("/proj/x");
        let as_include =
            ContentHash::of_config(&root, std::slice::from_ref(&p), &[], &suites);
        let as_exclude =
            ContentHash::of_config(&root, &[], std::slice::from_ref(&p), &suites);
        assert_ne!(as_include, as_exclude);
    }
}
