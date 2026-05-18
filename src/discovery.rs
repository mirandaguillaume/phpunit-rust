use crate::types::TestCase;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor};
use walkdir::WalkDir;

/// A discovered test class with all of its methods, grouped for batched
/// dispatch (one request per class to the worker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestClass {
    pub file: PathBuf,
    pub class: String,
    pub methods: Vec<String>,
}

/// Group a flat list of TestCases by class. Preserves discovery order.
pub fn group_by_class(cases: Vec<TestCase>) -> Vec<TestClass> {
    let mut groups: Vec<TestClass> = Vec::new();
    for case in cases {
        if let Some(existing) = groups.iter_mut().find(|g| g.class == case.class) {
            existing.methods.push(case.method);
        } else {
            groups.push(TestClass {
                file: case.file,
                class: case.class,
                methods: vec![case.method],
            });
        }
    }
    groups
}

/// Parse one PHP file and return any test classes + methods it declares.
pub fn discover_in_file(path: &Path) -> Result<Vec<TestCase>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_php::language_php())
        .context("setting tree-sitter-php language")?;
    let tree = parser
        .parse(&src, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {}", path.display()))?;

    let root = tree.root_node();
    let bytes = src.as_bytes();

    let namespace = find_namespace(root, bytes);

    let mut cases = Vec::new();
    collect_test_classes(root, bytes, namespace.as_deref(), path, &mut cases)?;
    Ok(cases)
}

fn find_namespace(root: Node, bytes: &[u8]) -> Option<String> {
    // PHP: `namespace Foo\Bar;` produces a `namespace_definition` node.
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "namespace_definition" {
            if let Some(name) = child.child_by_field_name("name") {
                return name.utf8_text(bytes).ok().map(String::from);
            }
        }
    }
    None
}

fn collect_test_classes(
    root: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    path: &Path,
    out: &mut Vec<TestCase>,
) -> Result<()> {
    let query_src = r#"
        (class_declaration
          name: (name) @class_name
          (base_clause (name) @base)?
          body: (declaration_list) @body)
    "#;
    let lang = tree_sitter_php::language_php();
    let query = Query::new(&lang, query_src).context("compiling class query")?;
    let mut cursor = QueryCursor::new();
    let captures = query.capture_names();
    let class_name_idx = captures.iter().position(|n| *n == "class_name").unwrap();
    let base_idx = captures.iter().position(|n| *n == "base").unwrap();
    let body_idx = captures.iter().position(|n| *n == "body").unwrap();

    for m in cursor.matches(&query, root, bytes) {
        let mut class_name: Option<&str> = None;
        let mut base_name: Option<&str> = None;
        let mut body_node: Option<Node> = None;
        for cap in m.captures {
            let idx = cap.index as usize;
            if idx == class_name_idx {
                class_name = cap.node.utf8_text(bytes).ok();
            } else if idx == base_idx {
                base_name = cap.node.utf8_text(bytes).ok();
            } else if idx == body_idx {
                body_node = Some(cap.node);
            }
        }

        let (Some(name), Some(body)) = (class_name, body_node) else { continue };
        let base = base_name.unwrap_or("");
        if !is_testcase_subclass(base) {
            continue;
        }

        let fqcn = match namespace {
            Some(ns) => format!("{ns}\\{name}"),
            None => name.to_string(),
        };

        for method in collect_test_methods(body, bytes) {
            out.push(TestCase {
                file: path.to_path_buf(),
                class: fqcn.clone(),
                method,
            });
        }
    }
    Ok(())
}

fn is_testcase_subclass(base: &str) -> bool {
    // MVP heuristic: anything named TestCase or ending in TestCase.
    // Real PHPUnit-compat would resolve `use` aliases. Tracked in follow-up plan.
    base == "TestCase" || base.ends_with("\\TestCase")
}

fn collect_test_methods(body: Node, bytes: &[u8]) -> Vec<String> {
    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        // Skip non-public for MVP — PHPUnit only runs public methods.
        let is_public = method_is_public(child, bytes);
        if !is_public {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else { continue };
        let Ok(name) = name_node.utf8_text(bytes) else { continue };
        if name.starts_with("test") {
            methods.push(name.to_string());
        }
        // #[Test] attribute support is deferred to a follow-up plan.
    }
    methods
}

fn method_is_public(method: Node, bytes: &[u8]) -> bool {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child.utf8_text(bytes).map(|t| t == "public").unwrap_or(false);
        }
    }
    // PHP defaults to public when no visibility modifier is present.
    true
}

/// Walk a directory, returning all discovered test cases.
pub fn discover_in_dir(root: &Path) -> Result<Vec<TestCase>> {
    let mut all = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("php") {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains("Test") {
            continue;
        }
        all.extend(discover_in_file(p)?);
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_tmp(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SomeTest.php");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn discovers_a_namespaced_test_class() {
        let src = r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
final class FooTest extends TestCase {
    public function testOne(): void {}
    public function testTwo(): void {}
    public function helper(): void {}
    private function testIsPrivateSoSkipped(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let methods: Vec<_> = cases.iter().map(|c| c.method.as_str()).collect();
        assert_eq!(methods, vec!["testOne", "testTwo"]);
        assert_eq!(cases[0].class, "App\\Tests\\FooTest");
    }

    #[test]
    fn skips_classes_not_extending_testcase() {
        let src = r#"<?php
namespace App;
final class NotATest {
    public function testNothing(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert!(cases.is_empty());
    }

    #[test]
    fn handles_file_without_namespace() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class BareTest extends TestCase {
    public function testStuff(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].class, "BareTest");
    }

    #[test]
    fn discovers_fixture_project_tests() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/sample_project/tests");
        let cases = discover_in_dir(&fixture).unwrap();
        let methods: Vec<_> = cases.iter().map(|c| (c.class.as_str(), c.method.as_str())).collect();
        assert!(methods.contains(&("Sample\\Tests\\CalculatorTest", "testAddsTwoPositiveIntegers")));
        assert!(methods.contains(&("Sample\\Tests\\FailingTest", "testThisDeliberatelyFails")));
        assert_eq!(cases.len(), 12);
    }

    #[test]
    fn group_by_class_collapses_per_method_cases() {
        let cases = vec![
            TestCase { file: PathBuf::from("/p/A.php"), class: "A".into(), method: "testOne".into() },
            TestCase { file: PathBuf::from("/p/A.php"), class: "A".into(), method: "testTwo".into() },
            TestCase { file: PathBuf::from("/p/B.php"), class: "B".into(), method: "testThree".into() },
        ];
        let grouped = group_by_class(cases);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].class, "A");
        assert_eq!(grouped[0].methods, vec!["testOne".to_string(), "testTwo".to_string()]);
        assert_eq!(grouped[1].class, "B");
        assert_eq!(grouped[1].methods, vec!["testThree".to_string()]);
    }
}
