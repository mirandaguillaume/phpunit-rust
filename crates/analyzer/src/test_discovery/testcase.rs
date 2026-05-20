use crate::mago_bridge::MagoProject;
use mago_reflection::class_like::ClassLikeReflection;
use std::collections::HashMap;

const TESTCASE_FQCN_LOWER: &str = "phpunit\\framework\\testcase";
const MAX_INHERITANCE_DEPTH: usize = 50;

/// Returns the FQCNs of classes that extend PHPUnit\Framework\TestCase
/// (directly or transitively). FQCNs are returned in whatever casing
/// mago-project produced — typically lowercased.
pub fn find_testcase_subclasses(project: &MagoProject) -> Vec<String> {
    // Build lowercased-FQCN → reflection index.
    let mut index: HashMap<String, &ClassLikeReflection> = HashMap::new();
    for (name, refl) in project.class_likes() {
        let key = project.class_name_str(name).to_lowercase();
        index.insert(key, refl);
    }

    let mut out = Vec::new();
    for (name, _refl) in project.class_likes() {
        let fqcn = project.class_name_str(name);
        if ascends_to_testcase(&fqcn, &index, project) {
            out.push(fqcn);
        }
    }
    out
}

fn ascends_to_testcase(
    start_fqcn: &str,
    index: &HashMap<String, &ClassLikeReflection>,
    project: &MagoProject,
) -> bool {
    let mut current = start_fqcn.to_lowercase();
    for _ in 0..MAX_INHERITANCE_DEPTH {
        if current == TESTCASE_FQCN_LOWER {
            return true;
        }
        let Some(refl) = index.get(&current) else { return false; };
        let Some(parent_name) = &refl.inheritance.direct_extended_class else { return false; };
        let parent_fqcn = project.interner().lookup(&parent_name.value).to_string()
            .trim_start_matches('\\').to_lowercase();
        if parent_fqcn == current {
            // Defensive: self-loop, bail.
            return false;
        }
        current = parent_fqcn;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with(files: &[(&str, &str)]) -> (tempfile::TempDir, MagoProject) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        std::fs::write(
            dir.path().join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        ).unwrap();
        for (path, content) in files {
            let target = dir.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).ok();
            std::fs::write(target, content).unwrap();
        }
        let project = MagoProject::load(dir.path()).unwrap();
        (dir, project)
    }

    #[test]
    fn finds_direct_subclass() {
        let (_d, project) = project_with(&[
            ("UserTest.php", "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass UserTest extends TestCase {}"),
        ]);
        let classes = find_testcase_subclasses(&project);
        assert!(
            classes.iter().any(|c| c.to_lowercase() == "usertest"),
            "expected UserTest in results; got: {classes:?}"
        );
    }

    #[test]
    fn finds_transitive_subclass() {
        let (_d, project) = project_with(&[
            ("BaseTest.php", "<?php\nuse PHPUnit\\Framework\\TestCase;\nabstract class BaseTest extends TestCase {}"),
            ("ConcreteTest.php", "<?php\nclass ConcreteTest extends BaseTest {}"),
        ]);
        let classes = find_testcase_subclasses(&project);
        let lc: Vec<String> = classes.iter().map(|c| c.to_lowercase()).collect();
        assert!(lc.iter().any(|c| c == "concretetest"), "expected ConcreteTest; got: {classes:?}");
        assert!(lc.iter().any(|c| c == "basetest"), "expected BaseTest; got: {classes:?}");
    }

    #[test]
    fn excludes_non_test_classes() {
        let (_d, project) = project_with(&[
            ("Plain.php", "<?php\nclass Plain {}"),
            ("User.php", "<?php\nclass User { public function name(): string { return 'x'; } }"),
        ]);
        let classes = find_testcase_subclasses(&project);
        // TestCase itself may or may not appear in results — that's fine.
        // What MUST NOT appear: Plain or User.
        let lc: Vec<String> = classes.iter().map(|c| c.to_lowercase()).collect();
        assert!(!lc.iter().any(|c| c == "plain"), "Plain should not be in results: {classes:?}");
        assert!(!lc.iter().any(|c| c == "user"), "User should not be in results: {classes:?}");
    }
}
