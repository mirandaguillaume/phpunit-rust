//! Proxy coverage for interfaces and empty stubs.
//!
//! Interfaces and stub classes with no executable statements are never
//! reached by the main tracer. This post-processing pass marks an
//! interface/stub as "covered" when at least one concrete implementor
//! or subclass is present in the coverage map.

use std::collections::HashMap;
use std::path::PathBuf;

use mago_codex::symbol::SymbolKind;

use crate::analyzer::{Coverage, TestId};
use crate::boundary::{Boundary, BoundaryResolver};
use crate::mago_bridge::{word_to_string, MagoProject};

/// Add proxy coverage entries for interfaces/empty stubs with a covered implementor.
///
/// Must be called AFTER the main trace loop. For each project-boundary class-like
/// that has no executable lines (interface or empty class) and is NOT yet covered,
/// this function looks for a covered concrete class that implements or extends it.
/// When found, it records the declaring line as covered with those test IDs.
pub fn add_proxy_coverage(
    project: &MagoProject,
    boundary: &BoundaryResolver,
    coverage: &mut Coverage,
) {
    // 1. Collect proxy targets: uncovered interfaces and empty classes in project boundary.
    //    Key: fqcn (lowercased, no leading '\')
    //    Value: (file_path, 1-based declaration line)
    let mut proxy_targets: HashMap<String, (PathBuf, u32)> = HashMap::new();
    for refl in project.class_likes() {
        let is_candidate = refl.kind == SymbolKind::Interface
            || (refl.kind == SymbolKind::Class && refl.methods.is_empty());
        if !is_candidate {
            continue;
        }

        let Some(src) = project.file_of_span(&refl.span) else {
            continue;
        };
        let file = file_path_of(src);

        if boundary.classify(&file) != Boundary::Project {
            continue;
        }
        if coverage.contains_key(&file) {
            continue; // already covered by the main pass
        }

        let line = src.line_number(refl.span.start.offset) + 1;
        let fqcn = word_to_string(&refl.name)
            .trim_start_matches('\\')
            .to_lowercase();
        proxy_targets.insert(fqcn, (file, line));
    }

    if proxy_targets.is_empty() {
        return;
    }

    // 2. For each covered concrete class, check which proxy targets it covers
    //    via its inheritance chain.
    let mut proxy_additions: HashMap<PathBuf, (u32, Vec<TestId>)> = HashMap::new();

    for refl in project.class_likes() {
        if refl.kind == SymbolKind::Interface || refl.kind == SymbolKind::Trait {
            continue;
        }

        let Some(src) = project.file_of_span(&refl.span) else {
            continue;
        };
        let file = file_path_of(src);

        let Some(line_map) = coverage.get(&file) else {
            continue;
        };
        let test_ids: Vec<TestId> = line_map.values().flatten().cloned().collect();
        if test_ids.is_empty() {
            continue;
        }

        // Check all interfaces this class implements (direct + transitive).
        for iface_name in refl.all_parent_interfaces.iter() {
            let fqcn = word_to_string(iface_name)
                .trim_start_matches('\\')
                .to_lowercase();
            if let Some((proxy_file, proxy_line)) = proxy_targets.get(&fqcn) {
                let entry = proxy_additions
                    .entry(proxy_file.clone())
                    .or_insert_with(|| (*proxy_line, Vec::new()));
                entry.1.extend(test_ids.iter().cloned());
            }
        }

        // Check all parent classes (direct + transitive) — covers empty stub base classes.
        for parent_name in refl.all_parent_classes.iter() {
            let fqcn = word_to_string(parent_name)
                .trim_start_matches('\\')
                .to_lowercase();
            if let Some((proxy_file, proxy_line)) = proxy_targets.get(&fqcn) {
                let entry = proxy_additions
                    .entry(proxy_file.clone())
                    .or_insert_with(|| (*proxy_line, Vec::new()));
                entry.1.extend(test_ids.iter().cloned());
            }
        }
    }

    // 3. Insert proxy entries, deduplicating test IDs.
    for (file, (line, mut test_ids)) in proxy_additions {
        test_ids.sort_by(|a, b| {
            a.class
                .cmp(&b.class)
                .then(a.method.cmp(&b.method))
                .then(a.data_set.cmp(&b.data_set))
        });
        test_ids.dedup();
        coverage
            .entry(file)
            .or_default()
            .entry(line)
            .or_default()
            .extend(test_ids);
    }
}

/// Resolve a loaded `File` to the `PathBuf` used as a coverage-map key.
/// Prefers the on-disk path; falls back to the logical name.
fn file_path_of(file: &mago_database::file::File) -> PathBuf {
    match &file.path {
        Some(p) => p.clone(),
        None => PathBuf::from(String::from_utf8_lossy(&file.name).into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;
    use crate::mago_bridge::MagoProject;

    fn make_boundary(root: &std::path::Path) -> BoundaryResolver {
        let cfg = ProjectConfig {
            root: root.to_path_buf(),
            test_suites: vec![root.join("tests")],
            source_includes: vec![root.join("src")],
            source_excludes: vec![],
        };
        BoundaryResolver::from_config(&cfg)
    }

    fn project_with(files: &[(&str, &str)]) -> (tempfile::TempDir, MagoProject) {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let target = dir.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).ok();
            std::fs::write(target, content).unwrap();
        }
        let project = MagoProject::load(dir.path()).unwrap();
        (dir, project)
    }

    fn covered(
        dir: &std::path::Path,
        rel_path: &str,
        line: u32,
        class: &str,
        method: &str,
    ) -> (PathBuf, HashMap<u32, Vec<TestId>>) {
        let file = dir.join(rel_path);
        let mut line_map = HashMap::new();
        line_map.insert(
            line,
            vec![TestId {
                class: class.into(),
                method: method.into(),
                data_set: None,
            }],
        );
        (file, line_map)
    }

    #[test]
    fn proxies_interface_when_implementor_is_covered() {
        let (dir, project) = project_with(&[
            (
                "src/MyInterface.php",
                "<?php\ninterface MyInterface {}",
            ),
            (
                "src/Concrete.php",
                "<?php\nclass Concrete implements MyInterface {\n  public function doIt(): void {}\n}",
            ),
        ]);
        let boundary = make_boundary(dir.path());
        let mut coverage: Coverage = HashMap::new();
        let (f, m) = covered(dir.path(), "src/Concrete.php", 3, "T", "testA");
        coverage.insert(f, m);

        add_proxy_coverage(&project, &boundary, &mut coverage);

        let iface_file = dir.path().join("src/MyInterface.php");
        assert!(
            coverage.contains_key(&iface_file),
            "interface should be proxied; keys: {:?}",
            coverage.keys().collect::<Vec<_>>()
        );
        let line_map = coverage.get(&iface_file).unwrap();
        let all_ids: Vec<_> = line_map.values().flatten().collect();
        assert!(!all_ids.is_empty(), "proxy entry should have test IDs");
    }

    #[test]
    fn skips_already_covered_interface() {
        let (dir, project) = project_with(&[
            ("src/MyInterface.php", "<?php\ninterface MyInterface {}"),
            ("src/Concrete.php", "<?php\nclass Concrete implements MyInterface {\n  public function doIt(): void {}\n}"),
        ]);
        let boundary = make_boundary(dir.path());
        let mut coverage: Coverage = HashMap::new();
        // Pre-cover the interface.
        let (fi, mi) = covered(dir.path(), "src/MyInterface.php", 2, "T", "testX");
        coverage.insert(fi, mi);
        let (fc, mc) = covered(dir.path(), "src/Concrete.php", 3, "T", "testA");
        coverage.insert(fc, mc);

        let keys_before = coverage.len();
        add_proxy_coverage(&project, &boundary, &mut coverage);
        assert_eq!(
            coverage.len(),
            keys_before,
            "already-covered interface must not be modified"
        );
    }

    #[test]
    fn no_proxy_when_no_implementor_covered() {
        let (dir, project) = project_with(&[
            ("src/MyInterface.php", "<?php\ninterface MyInterface {}"),
            ("src/Concrete.php", "<?php\nclass Concrete implements MyInterface {\n  public function doIt(): void {}\n}"),
        ]);
        let boundary = make_boundary(dir.path());
        let mut coverage: Coverage = HashMap::new();
        add_proxy_coverage(&project, &boundary, &mut coverage);
        assert!(coverage.is_empty(), "nothing covered → no proxy entries");
    }

    #[test]
    fn proxies_empty_stub_when_subclass_is_covered() {
        let (dir, project) = project_with(&[
            ("src/Stub.php", "<?php\nclass Stub {}"),
            (
                "src/Child.php",
                "<?php\nclass Child extends Stub {\n  public function doIt(): void {}\n}",
            ),
        ]);
        let boundary = make_boundary(dir.path());
        let mut coverage: Coverage = HashMap::new();
        let (f, m) = covered(dir.path(), "src/Child.php", 3, "T", "testX");
        coverage.insert(f, m);

        add_proxy_coverage(&project, &boundary, &mut coverage);

        let stub_file = dir.path().join("src/Stub.php");
        assert!(
            coverage.contains_key(&stub_file),
            "empty Stub should be proxied when Child (extends Stub) is covered"
        );
    }

    #[test]
    fn vendor_interface_not_proxied() {
        let (dir, project) = project_with(&[
            (
                "vendor/lib/VendorIface.php",
                "<?php\ninterface VendorIface {}",
            ),
            (
                "src/Impl.php",
                "<?php\nclass Impl implements VendorIface {\n  public function doIt(): void {}\n}",
            ),
        ]);
        let boundary = make_boundary(dir.path());
        let mut coverage: Coverage = HashMap::new();
        let (f, m) = covered(dir.path(), "src/Impl.php", 3, "T", "testA");
        coverage.insert(f, m);

        let keys_before = coverage.len();
        add_proxy_coverage(&project, &boundary, &mut coverage);
        assert_eq!(
            coverage.len(),
            keys_before,
            "vendor interface must not be proxied"
        );
    }
}
