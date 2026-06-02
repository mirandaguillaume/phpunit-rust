pub mod lifecycle;
pub mod methods;
pub mod testcase;

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TestMethod {
    pub class: String,
    pub method: String,
    pub file: PathBuf,
    pub line: u32,
    pub has_data_provider: Option<String>,
    pub lifecycle: LifecycleBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct LifecycleBinding {
    pub set_up_before_class: bool,
    pub set_up: bool,
    pub tear_down: bool,
    pub tear_down_after_class: bool,
}

use crate::cache::store::CacheError;
use crate::cache::{CacheStore, ContentHash};
use crate::mago_bridge::{BridgeError, MagoProject};

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("cache: {0}")]
    Cache(#[from] CacheError),
    #[error("bridge: {0}")]
    Bridge(#[from] BridgeError),
}

const CACHE_KIND: &str = "test_discovery";

#[derive(serde::Serialize, serde::Deserialize)]
struct DiscoveryEntry {
    methods: Vec<TestMethod>,
}

/// Try to load test methods for the given files from cache only.
/// Returns `None` on any cache miss; the caller must fall back to a full load.
pub fn try_from_cache(
    cache: &CacheStore,
    files: &[std::path::PathBuf],
) -> Result<Option<Vec<TestMethod>>, DiscoveryError> {
    let mut all = Vec::new();
    for file in files {
        let hash = ContentHash::of_file(file).map_err(CacheError::Io)?;
        match cache.get::<DiscoveryEntry>(CACHE_KIND, &hash)? {
            Some(entry) => all.extend(entry.methods),
            None => return Ok(None),
        }
    }
    Ok(Some(all))
}

/// Discover test methods for the given files, using the cache where possible.
///
/// Each file is keyed by its BLAKE3 content hash. Files with cached results
/// return their cached TestMethods directly. If any files miss, a full project
/// scan runs once and per-file results are persisted.
pub fn discover(
    project: &MagoProject,
    cache: &CacheStore,
    files: &[std::path::PathBuf],
) -> Result<Vec<TestMethod>, DiscoveryError> {
    let mut all = Vec::new();
    let mut misses: Vec<(std::path::PathBuf, ContentHash)> = Vec::new();

    for file in files {
        let hash = ContentHash::of_file(file).map_err(CacheError::Io)?;
        if let Some(entry) = cache.get::<DiscoveryEntry>(CACHE_KIND, &hash)? {
            all.extend(entry.methods);
        } else {
            misses.push((file.clone(), hash));
        }
    }

    if misses.is_empty() {
        return Ok(all);
    }

    // At least one file changed — run the full project scan.
    let classes = testcase::find_testcase_subclasses(project);
    let mut fresh = methods::find_test_methods(project, &classes);
    lifecycle::bind_lifecycle_methods(project, &mut fresh);

    // Bucket fresh methods by their source file (TestMethod.file).
    let mut per_file: std::collections::HashMap<std::path::PathBuf, Vec<TestMethod>> =
        std::collections::HashMap::new();
    for m in fresh.iter() {
        per_file.entry(m.file.clone()).or_default().push(m.clone());
    }

    for (file, hash) in misses {
        let entry = DiscoveryEntry {
            methods: per_file.get(&file).cloned().unwrap_or_default(),
        };
        cache.put(CACHE_KIND, &hash, &entry)?;
        all.extend(entry.methods);
    }

    Ok(all)
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::cache::CacheStore;
    use std::fs;

    fn project_with(test_php: &str) -> (tempfile::TempDir, MagoProject, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        )
        .unwrap();
        let test_file = dir.path().join("MyTest.php");
        fs::write(&test_file, test_php).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        (dir, project, test_file)
    }

    #[test]
    fn second_call_reads_from_cache() {
        let (dir, project, file) = project_with(
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass MyTest extends TestCase {\n  public function testFoo(): void {}\n}",
        );
        let cache = CacheStore::open(dir.path(), MagoProject::version()).unwrap();

        let first = discover(&project, &cache, std::slice::from_ref(&file)).unwrap();
        let second = discover(&project, &cache, std::slice::from_ref(&file)).unwrap();

        assert_eq!(first.len(), 1, "expected 1 test method");
        assert_eq!(first, second, "cached result must match fresh result");
    }

    #[test]
    fn cache_invalidates_on_file_change() {
        let (dir, project, file) = project_with(
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass MyTest extends TestCase {\n  public function testFoo(): void {}\n}",
        );
        let cache = CacheStore::open(dir.path(), MagoProject::version()).unwrap();
        let _first = discover(&project, &cache, std::slice::from_ref(&file)).unwrap();

        // Rewrite the file with an additional test method.
        fs::write(&file,
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass MyTest extends TestCase {\n  public function testFoo(): void {}\n  public function testBar(): void {}\n}",
        ).unwrap();
        // Reload the project to see the new method.
        let project2 = MagoProject::load(dir.path()).unwrap();
        let second = discover(&project2, &cache, &[file]).unwrap();

        assert_eq!(
            second.len(),
            2,
            "cache should have invalidated on file change; got: {second:?}"
        );
    }
}
