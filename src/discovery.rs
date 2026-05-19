use crate::types::TestCase;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor};
use walkdir::WalkDir;

/// One class discovered during the pass-1 scan: enough information to build
/// the inheritance graph (pass 2) and emit test cases (pass 3) without
/// re-parsing the source.
#[derive(Debug, Clone)]
struct ParsedClass {
    file: PathBuf,
    /// Fully-qualified class name (namespace + "\\" + short name).
    fqcn: String,
    /// Resolved FQCN of the parent (post namespace + use-alias resolution),
    /// or `None` if the class has no `extends` clause.
    parent_fqcn: Option<String>,
    /// All public methods whose name starts with "test". Already filtered;
    /// pass 3 just emits them if the class is determined to be a test.
    test_methods: Vec<String>,
    /// `true` if the class is `abstract`. Abstract classes can never be
    /// instantiated by PHPUnit; we skip emitting test cases for them even
    /// when their chain reaches TestCase.
    is_abstract: bool,
}

/// Maps every discovered class FQCN to its resolved parent FQCN (or None).
/// Used by the BFS to decide whether a class reaches TestCase.
type ClassGraph = HashMap<String, Option<String>>;

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

/// Pass 1 (per file): parse a PHP source file and return every class
/// declaration in it, with its resolved parent FQCN and test methods.
///
/// "Resolved parent FQCN" applies the file's namespace + use-alias context so
/// the BFS in pass 3 can compare on FQCN strings alone.
fn parse_file_classes(path: &Path) -> Result<Vec<ParsedClass>> {
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
    let aliases = parse_use_aliases(root, bytes);

    let mut out = Vec::new();
    collect_parsed_classes(root, bytes, namespace.as_deref(), &aliases, path, &mut out)?;
    Ok(out)
}

/// Parse `use Foo\Bar;` and `use Foo\Bar as Baz;` into a local-name → FQCN map.
/// Grouped uses (`use Foo\{Bar, Baz};`) are best-effort: tree-sitter-php
/// represents them with multiple `qualified_name` children we walk in order.
fn parse_use_aliases(root: Node, bytes: &[u8]) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        // Top-level `namespace_use_declaration` per PHP grammar.
        if child.kind() != "namespace_use_declaration" {
            continue;
        }
        let mut inner = child.walk();
        for clause in child.children(&mut inner) {
            if clause.kind() != "namespace_use_clause" {
                continue;
            }
            // The clause's first qualified_name (or name) is the imported FQCN;
            // an optional namespace_aliasing_clause supplies the alias.
            let mut imported: Option<String> = None;
            let mut alias: Option<String> = None;
            let mut clause_cur = clause.walk();
            for cc in clause.children(&mut clause_cur) {
                match cc.kind() {
                    "qualified_name" | "name" if imported.is_none() => {
                        imported = cc.utf8_text(bytes).ok().map(|s| s.trim_start_matches('\\').to_string());
                    }
                    "namespace_aliasing_clause" => {
                        // Child is the alias name.
                        let mut acur = cc.walk();
                        for ac in cc.children(&mut acur) {
                            if ac.kind() == "name" {
                                alias = ac.utf8_text(bytes).ok().map(String::from);
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let Some(fqcn) = imported {
                // Default local name is the last segment of the FQCN.
                let local = alias.unwrap_or_else(|| {
                    fqcn.rsplit('\\').next().unwrap_or(&fqcn).to_string()
                });
                aliases.insert(local, fqcn);
            }
        }
    }
    aliases
}

/// Resolve a parent class name (as written after `extends`) into a FQCN,
/// using PHP's name-resolution rules: absolute names start with `\`;
/// relative names check use-aliases on their first segment, else prefix
/// with the current namespace.
fn resolve_class_reference(
    raw: &str,
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
) -> String {
    // Absolute: `\Foo\Bar` → `Foo\Bar`.
    if let Some(stripped) = raw.strip_prefix('\\') {
        return stripped.to_string();
    }
    let mut segments = raw.splitn(2, '\\');
    let first = segments.next().unwrap_or("");
    let rest = segments.next();
    if let Some(aliased_fqcn) = aliases.get(first) {
        match rest {
            Some(tail) => format!("{aliased_fqcn}\\{tail}"),
            None => aliased_fqcn.clone(),
        }
    } else if let Some(ns) = namespace {
        format!("{ns}\\{raw}")
    } else {
        raw.to_string()
    }
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

fn collect_parsed_classes(
    root: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
    path: &Path,
    out: &mut Vec<ParsedClass>,
) -> Result<()> {
    let query_src = r#"
        (class_declaration
          name: (name) @class_name
          (base_clause (name) @base)?
          body: (declaration_list) @body) @class
    "#;
    let lang = tree_sitter_php::language_php();
    let query = Query::new(&lang, query_src).context("compiling class query")?;
    let mut cursor = QueryCursor::new();
    let captures = query.capture_names();
    let class_idx = captures.iter().position(|n| *n == "class").unwrap();
    let class_name_idx = captures.iter().position(|n| *n == "class_name").unwrap();
    let base_idx = captures.iter().position(|n| *n == "base").unwrap();
    let body_idx = captures.iter().position(|n| *n == "body").unwrap();

    for m in cursor.matches(&query, root, bytes) {
        let mut class_node: Option<Node> = None;
        let mut class_name: Option<&str> = None;
        let mut base_name: Option<&str> = None;
        let mut body_node: Option<Node> = None;
        for cap in m.captures {
            let idx = cap.index as usize;
            if idx == class_idx {
                class_node = Some(cap.node);
            } else if idx == class_name_idx {
                class_name = cap.node.utf8_text(bytes).ok();
            } else if idx == base_idx {
                base_name = cap.node.utf8_text(bytes).ok();
            } else if idx == body_idx {
                body_node = Some(cap.node);
            }
        }

        let (Some(name), Some(body), Some(decl)) = (class_name, body_node, class_node) else { continue };

        let fqcn = match namespace {
            Some(ns) => format!("{ns}\\{name}"),
            None => name.to_string(),
        };
        let parent_fqcn = base_name.map(|b| resolve_class_reference(b, namespace, aliases));
        let test_methods = collect_test_methods(body, bytes);
        let is_abstract = class_has_modifier(decl, bytes, "abstract");

        out.push(ParsedClass {
            file: path.to_path_buf(),
            fqcn,
            parent_fqcn,
            test_methods,
            is_abstract,
        });
    }
    Ok(())
}

/// Returns true if the class declaration has the named modifier (e.g. "abstract", "final").
/// Tree-sitter-php represents modifiers as plain `name`-like children of the class_declaration.
fn class_has_modifier(class_decl: Node, bytes: &[u8], wanted: &str) -> bool {
    let mut cursor = class_decl.walk();
    for child in class_decl.children(&mut cursor) {
        let kind = child.kind();
        if kind == "abstract_modifier" || kind == "final_modifier" || kind == "readonly_modifier" {
            if let Ok(text) = child.utf8_text(bytes) {
                if text.eq_ignore_ascii_case(wanted) {
                    return true;
                }
            }
        }
    }
    false
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

/// Convenience wrapper: discover tests in a single PHP file. Equivalent to
/// `discover_in_dir` on a directory containing only this file — useful for
/// unit tests and for cases where inheritance chains live entirely in one
/// file. Cross-file inheritance won't resolve (use `discover_in_dir`).
pub fn discover_in_file(path: &Path) -> Result<Vec<TestCase>> {
    let parsed = parse_file_classes(path)?;
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();
    emit_test_cases(&parsed, &graph)
}

/// Pass 3: walk the parsed classes, filter by the BFS, emit TestCases.
/// Shared between `discover_in_file` and `discover_in_dir`.
///
/// For each non-abstract class reaching TestCase, we union test methods
/// from the class itself plus every parsed ancestor in the chain. This
/// catches the doctrine/collections-style pattern where a `final` concrete
/// test class extends an abstract base that defines most/all of its tests.
/// Without this, the concrete class would emit zero TestCase entries (the
/// inherited methods were on the abstract parent), and the runner would
/// silently skip them.
fn emit_test_cases(parsed: &[ParsedClass], graph: &ClassGraph) -> Result<Vec<TestCase>> {
    // Index by FQCN for chain-walking.
    let by_fqcn: HashMap<&str, &ParsedClass> =
        parsed.iter().map(|c| (c.fqcn.as_str(), c)).collect();

    let mut cases = Vec::new();
    for class in parsed {
        if class.is_abstract {
            continue;
        }
        if !is_test_class_via_chain(&class.fqcn, graph) {
            continue;
        }
        // Collect methods from this class + all parsed ancestors, dedup by name.
        // The PHP runtime will run inherited methods on the concrete subclass,
        // so we emit them under `class.fqcn` (not the ancestor's FQCN).
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut visit = class.fqcn.as_str();
        let mut depth = 0;
        while depth < 32 {
            if let Some(c) = by_fqcn.get(visit) {
                for m in &c.test_methods {
                    if seen.insert(m.as_str()) {
                        cases.push(TestCase {
                            file: class.file.clone(),
                            class: class.fqcn.clone(),
                            method: m.clone(),
                        });
                    }
                }
                match c.parent_fqcn.as_deref() {
                    Some(p) => visit = p,
                    None => break,
                }
            } else {
                // Parent is outside our parsed set (e.g., PHPUnit's TestCase).
                break;
            }
            depth += 1;
        }
    }
    Ok(cases)
}

/// Walk a directory, returning all discovered test cases.
///
/// Three-pass algorithm:
///   1. Parse every `*Test*.php` file under `root` into a flat `Vec<ParsedClass>`.
///      Each class carries its resolved-FQCN parent + its test methods + abstract bit.
///   2. Build a `ClassGraph`: FQCN → resolved parent FQCN (or None).
///   3. For each non-abstract class: if `is_test_class_via_chain(fqcn, &graph)`
///      returns true, emit one `TestCase` per test method.
///
/// This catches projects with intermediate base classes (`AbstractTestCase`,
/// `KernelTestCase`, etc.) that the old single-pass discovery missed.
pub fn discover_in_dir(root: &Path) -> Result<Vec<TestCase>> {
    // Pass 1: parse every relevant file.
    let mut parsed: Vec<ParsedClass> = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("php") {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains("Test") {
            continue;
        }
        parsed.extend(parse_file_classes(p)?);
    }

    // Pass 2: build the inheritance graph (FQCN -> parent FQCN or None).
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();

    // Pass 3: emit test methods for every non-abstract class reaching TestCase.
    emit_test_cases(&parsed, &graph)
}

/// Returns true if `start_fqcn` is (transitively) a subclass of PHPUnit's
/// `TestCase`.
///
/// **Inputs:**
/// - `start_fqcn`: the class to check (already a key in `graph`).
/// - `graph`: maps every discovered FQCN → its resolved parent FQCN (or `None`
///   when the class has no `extends` clause).
///
/// **Terminal rules** (return `true`):
/// - Any parent FQCN we visit equals `"PHPUnit\\Framework\\TestCase"`.
/// - Any parent FQCN we visit ends with `"\\TestCase"` (catches re-exports
///   or projects using a custom namespace for what is actually PHPUnit's class
///   — false positives are tolerable; the worker rejects non-TestCase classes
///   at runtime).
///
/// **Termination (return `false`):**
/// - The chain ends at a class with no parent (`None`).
///
/// **Safety:**
/// - Guard against cycles in the graph (malformed but defensible input).
///   Either bound iteration depth (e.g. 32) or maintain a visited set.
///
/// **Out of graph:**
/// - If a parent FQCN is *not* a key in `graph` (e.g. PHPUnit's own `TestCase`
///   defined in vendor/, outside our scanned tests/ directory), apply the
///   terminal name check to that string and stop. Returning `false` when the
///   parent is unknown AND doesn't match terminal patterns is correct: we
///   simply don't have enough information to claim it's a TestCase.
fn is_test_class_via_chain(start_fqcn: &str, graph: &ClassGraph) -> bool {
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut current = start_fqcn;
    while visited.insert(current) {
        let parent = match graph.get(current) {
            Some(Some(p)) => p.as_str(),
            // No parent (no `extends` clause) OR class not in graph — stop walking.
            _ => return false,
        };
        if parent == "PHPUnit\\Framework\\TestCase" || parent.ends_with("\\TestCase") {
            return true;
        }
        current = parent;
    }
    // visited.insert returned false → we've already seen `current` → cycle.
    false
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

    /// The brick/math-style case: a project-local abstract base class extends
    /// PHPUnit\Framework\TestCase, and concrete tests extend that base. The
    /// old single-pass discovery missed these. After fix #4, `discover_in_dir`
    /// must walk the chain and discover the concrete subclass.
    #[test]
    fn discovers_class_extending_intermediate_base_class() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AbstractBaseTest.php"),
            r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
abstract class AbstractBaseTest extends TestCase {
    protected function helper(): void {}
}
"#,
        ).unwrap();
        std::fs::write(
            dir.path().join("ConcreteTest.php"),
            r#"<?php
namespace App\Tests;
final class ConcreteTest extends AbstractBaseTest {
    public function testActual(): void {}
    public function testAnother(): void {}
}
"#,
        ).unwrap();

        let cases = discover_in_dir(dir.path()).unwrap();
        let methods: Vec<_> = cases
            .iter()
            .map(|c| (c.class.as_str(), c.method.as_str()))
            .collect();
        // The abstract base class itself must NOT contribute tests.
        assert!(!methods.iter().any(|(c, _)| *c == "App\\Tests\\AbstractBaseTest"),
            "abstract base class leaked into discovery: {methods:?}");
        // The concrete subclass's two tests must appear.
        assert!(methods.contains(&("App\\Tests\\ConcreteTest", "testActual")));
        assert!(methods.contains(&("App\\Tests\\ConcreteTest", "testAnother")));
        assert_eq!(cases.len(), 2, "unexpected outcomes: {methods:?}");
    }

    /// Defense against malformed input: a class graph with a cycle must not
    /// cause `is_test_class_via_chain` to infinite-loop. Hand-build a graph
    /// where A → B → A and assert the call returns (false) within reasonable
    /// time. (If your BFS is unguarded, this test hangs.)
    #[test]
    fn bfs_handles_cyclic_graph_without_hanging() {
        let mut graph: ClassGraph = HashMap::new();
        graph.insert("App\\A".into(), Some("App\\B".into()));
        graph.insert("App\\B".into(), Some("App\\A".into()));
        // Neither class is a TestCase; the chain cycles back without reaching one.
        assert!(!is_test_class_via_chain("App\\A", &graph));
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
