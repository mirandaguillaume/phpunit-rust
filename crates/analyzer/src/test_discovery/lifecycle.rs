//! Detect PHPUnit lifecycle methods (setUp/tearDown/setUpBeforeClass/tearDownAfterClass)
//! on test classes and record their presence on each TestMethod.
//!
//! Lifecycle methods are matched by exact name; case-sensitive per PHPUnit convention.

use super::TestMethod;
use crate::mago_bridge::{MagoProject, word_to_string};
use mago_codex::metadata::class_like::ClassLikeMetadata;
use std::collections::HashMap;

/// Walk each test method's owning class and flag which lifecycle methods are defined.
///
/// Method names matched (case-sensitive): `setUp`, `tearDown`, `setUpBeforeClass`,
/// `tearDownAfterClass`. Any other names are ignored.
pub fn bind_lifecycle_methods(project: &MagoProject, methods: &mut [TestMethod]) {
    // Build lowercased FQCN → reflection index (consistent with Task 8's testcase.rs).
    let class_index: HashMap<String, &ClassLikeMetadata> = project
        .class_likes()
        .map(|refl| (word_to_string(&refl.name).to_lowercase(), refl))
        .collect();

    for tm in methods.iter_mut() {
        let key = tm.class.to_lowercase();
        let Some(class_refl) = class_index.get(&key) else {
            continue;
        };
        for method_word in class_refl.methods.iter() {
            let method_name = word_to_string(method_word).to_lowercase();
            match method_name.as_str() {
                "setup" => tm.lifecycle.set_up = true,
                "teardown" => tm.lifecycle.tear_down = true,
                "setupbeforeclass" => tm.lifecycle.set_up_before_class = true,
                "teardownafterclass" => tm.lifecycle.tear_down_after_class = true,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_discovery::LifecycleBinding;
    use std::path::PathBuf;

    fn project_with(content: &str) -> (tempfile::TempDir, MagoProject) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        )
        .unwrap();
        std::fs::write(dir.path().join("MyTest.php"), content).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        (dir, project)
    }

    fn empty_test_method(class: &str, method: &str) -> TestMethod {
        TestMethod {
            class: class.to_string(),
            method: method.to_string(),
            file: PathBuf::new(),
            line: 0,
            has_data_provider: None,
            lifecycle: LifecycleBinding::default(),
        }
    }

    #[test]
    fn detects_setup_and_teardown() {
        let (_d, project) = project_with(
            r#"<?php
use PHPUnit\Framework\TestCase;
class MyTest extends TestCase {
    protected function setUp(): void {}
    public function testThing(): void {}
    protected function tearDown(): void {}
}"#,
        );
        let mut methods = vec![empty_test_method("MyTest", "testThing")];
        bind_lifecycle_methods(&project, &mut methods);
        assert!(methods[0].lifecycle.set_up, "setUp should be detected");
        assert!(
            methods[0].lifecycle.tear_down,
            "tearDown should be detected"
        );
        assert!(
            !methods[0].lifecycle.set_up_before_class,
            "setUpBeforeClass not present"
        );
        assert!(
            !methods[0].lifecycle.tear_down_after_class,
            "tearDownAfterClass not present"
        );
    }

    #[test]
    fn detects_class_level_lifecycles() {
        let (_d, project) = project_with(
            r#"<?php
use PHPUnit\Framework\TestCase;
class MyTest extends TestCase {
    public static function setUpBeforeClass(): void {}
    public static function tearDownAfterClass(): void {}
    public function testThing(): void {}
}"#,
        );
        let mut methods = vec![empty_test_method("MyTest", "testThing")];
        bind_lifecycle_methods(&project, &mut methods);
        assert!(methods[0].lifecycle.set_up_before_class);
        assert!(methods[0].lifecycle.tear_down_after_class);
    }

    #[test]
    fn no_lifecycle_methods_present() {
        let (_d, project) = project_with(
            r#"<?php
use PHPUnit\Framework\TestCase;
class MyTest extends TestCase {
    public function testThing(): void {}
}"#,
        );
        let mut methods = vec![empty_test_method("MyTest", "testThing")];
        bind_lifecycle_methods(&project, &mut methods);
        assert!(!methods[0].lifecycle.set_up);
        assert!(!methods[0].lifecycle.tear_down);
        assert!(!methods[0].lifecycle.set_up_before_class);
        assert!(!methods[0].lifecycle.tear_down_after_class);
    }

    #[test]
    fn unknown_class_leaves_methods_untouched() {
        let (_d, project) = project_with(
            r#"<?php
use PHPUnit\Framework\TestCase;
class OtherTest extends TestCase {
    protected function setUp(): void {}
    public function testFoo(): void {}
}"#,
        );
        let mut methods = vec![empty_test_method("MissingClass", "testThing")];
        bind_lifecycle_methods(&project, &mut methods);
        assert!(
            !methods[0].lifecycle.set_up,
            "unknown class should leave defaults"
        );
    }
}
