//! Method identification for PHPUnit test classes.
//!
//! # API model (mago 1.30)
//!
//! The codex `ClassLikeMetadata.methods` is a `WordSet` of method NAMES only,
//! and `FunctionLikeMetadata` (looked up via `codebase.get_method`) carries the
//! span but NOT attribute argument values — `AttributeMetadata` in 1.30 exposes
//! only `{ name, span }`. So `#[DataProvider("name")]` argument extraction and
//! `@test` PHPDoc detection both require walking the AST, which we do on demand
//! via [`MagoProject::with_program`] (the AST is arena-bound, so all reads happen
//! inside the closure and only owned `TestMethod`s escape).

use std::path::PathBuf;

use mago_span::HasSpan;
use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
use mago_syntax::ast::ast::statement::Statement;
use mago_syntax::ast::Method;

use super::TestMethod;
use crate::mago_bridge::{word_to_string, MagoProject};

/// Attribute FQNs for the `#[Test]` and `#[DataProvider("name")]` attributes.
/// mago stores attribute names as they appear after name resolution, i.e., the
/// FQCN without the leading backslash.
const ATTR_TEST: &str = "PHPUnit\\Framework\\Attributes\\Test";
const ATTR_DATA_PROVIDER: &str = "PHPUnit\\Framework\\Attributes\\DataProvider";

/// Returns the list of test methods declared on the given test classes.
///
/// A method is considered a "test method" if any of:
/// 1. Its name starts with `test` (case-sensitive, PHPUnit convention).
/// 2. It carries the `#[PHPUnit\Framework\Attributes\Test]` attribute.
/// 3. It has a `/** @test */` PHPDoc annotation.
///
/// `has_data_provider` is set when the method carries a
/// `#[PHPUnit\Framework\Attributes\DataProvider("name")]` attribute.
pub fn find_test_methods(project: &MagoProject, test_classes: &[String]) -> Vec<TestMethod> {
    let mut out = Vec::new();

    // Build a lowercased FQCN → reflection lookup.
    let class_index: std::collections::HashMap<
        String,
        &mago_codex::metadata::class_like::ClassLikeMetadata,
    > = project
        .class_likes()
        .map(|refl| (word_to_string(&refl.name).to_lowercase(), refl))
        .collect();

    for class_name in test_classes {
        let key = class_name.trim_start_matches('\\').to_lowercase();
        let Some(class_refl) = class_index.get(&key) else {
            continue;
        };

        // PHPUnit never INSTANTIATES an abstract test case — it runs its tests via
        // the concrete subclass(es). So an abstract `*TestCase` contributes no rows
        // of its own; its `test*` bodies are surfaced (below) to each CONCRETE
        // descendant, bound to THAT subclass's `$this` (Inc-4 Task C). Skipping the
        // abstract class here also avoids emitting a duplicate, always-bailing row
        // (abstract `$this` cannot run an abstract method like `buildCollection`).
        if class_refl.flags.is_abstract() {
            continue;
        }

        // Walk the class itself + its ancestor chain (nearest-first). A method
        // declared directly on `class_name` carries `declaring_class = None`; one
        // declared on an ancestor carries `declaring_class = Some(ancestor FQCN)`.
        // The FIRST class up the chain to declare a given method name wins (an
        // override shadows the parent), matching PHP method resolution.
        let mut seen_methods: std::collections::HashSet<String> = std::collections::HashSet::new();

        // The CONCRETE class's own file is where discovery BUCKETS every surfaced
        // method (own + inherited): the user queries by the concrete test file, so
        // an inherited method must report THAT file (its body lives elsewhere, found
        // by the driver via `body_class()`). Resolve it once.
        let Some(concrete_file) = project.file_of_span(&class_refl.span) else {
            continue;
        };
        let concrete_logical = String::from_utf8_lossy(&concrete_file.name).into_owned();
        let concrete_file_path: PathBuf = match &concrete_file.path {
            Some(p) => p.clone(),
            None => PathBuf::from(concrete_logical.clone()),
        };

        // The chain: the concrete class, then each parent (closest first). mago's
        // `all_parent_classes` is unordered, so we re-derive nearest-first by
        // following `direct_parent_class` links through the index.
        let chain = ancestor_chain(class_name, class_refl, &class_index);

        for (depth, ancestor_fqcn) in chain.iter().enumerate() {
            let akey = ancestor_fqcn.trim_start_matches('\\').to_lowercase();
            let Some(ancestor_refl) = class_index.get(&akey) else {
                continue;
            };
            let Some(file) = project.file_of_span(&ancestor_refl.span) else {
                continue;
            };
            let logical_name = String::from_utf8_lossy(&file.name).into_owned();
            let file_path: PathBuf = match &file.path {
                Some(p) => p.clone(),
                None => PathBuf::from(logical_name.clone()),
            };
            // depth 0 = the concrete class itself → declaring_class None.
            let declaring_class = if depth == 0 {
                None
            } else {
                Some(ancestor_fqcn.clone())
            };

            let mut methods = project
                .with_program(&logical_name, |program, file, names| {
                    let mut found = Vec::new();
                    collect_methods_in_statements(
                        program.statements.iter(),
                        ancestor_fqcn,
                        class_name,
                        declaring_class.clone(),
                        file,
                        &file_path,
                        names,
                        &mut found,
                    );
                    found
                })
                .unwrap_or_default();

            for mut m in methods.drain(..) {
                // First declaration up the chain wins (override shadows parent).
                if seen_methods.insert(m.method.to_lowercase()) {
                    // Inherited methods are bucketed under the CONCRETE file so a
                    // query for the subclass's test file surfaces them.
                    if depth > 0 {
                        m.file = concrete_file_path.clone();
                    }
                    out.push(m);
                }
            }
        }
    }

    out
}

/// Build the inheritance chain for `class_name` nearest-first: the class itself,
/// then its direct parent, grandparent, … up to (but not including) the point
/// where the chain leaves the loaded codebase. Stops at `PHPUnit\Framework\
/// TestCase` and below (those declare no user `test*` methods worth surfacing,
/// and walking into vendor is wasted work). Bounded to avoid cycles.
fn ancestor_chain(
    class_name: &str,
    class_refl: &mago_codex::metadata::class_like::ClassLikeMetadata,
    index: &std::collections::HashMap<String, &mago_codex::metadata::class_like::ClassLikeMetadata>,
) -> Vec<String> {
    const TESTCASE_FQCN_LOWER: &str = "phpunit\\framework\\testcase";
    const MAX_DEPTH: usize = 50;

    let mut chain = vec![class_name.to_string()];
    let mut current = class_refl;
    for _ in 0..MAX_DEPTH {
        let Some(parent) = &current.direct_parent_class else {
            break;
        };
        let parent_display = word_to_string(parent);
        let parent_key = parent_display.trim_start_matches('\\').to_lowercase();
        if parent_key == TESTCASE_FQCN_LOWER {
            break;
        }
        let Some(parent_refl) = index.get(&parent_key) else {
            break;
        };
        // Use the original-cased FQCN for display/lookup.
        chain.push(word_to_string(&parent_refl.original_name));
        current = parent_refl;
    }
    chain
}

/// Recursively walk a statement list (descending into namespaces) for the class
/// whose FQCN is `declaring_fqcn`, and collect its test methods into `out`.
///
/// `attributed_class` is the CONCRETE class the discovered methods belong to
/// (equal to `declaring_fqcn` for own methods; the subclass for inherited ones).
/// `declaring_class` is `None` for own methods, `Some(parent)` for inherited ones.
#[allow(clippy::too_many_arguments)]
fn collect_methods_in_statements<'s, 'arena, I>(
    stmts: I,
    declaring_fqcn: &str,
    attributed_class: &str,
    declaring_class: Option<String>,
    file: &mago_database::file::File,
    file_path: &std::path::Path,
    names: &mago_names::ResolvedNames,
    out: &mut Vec<TestMethod>,
) where
    'arena: 's,
    I: Iterator<Item = &'s Statement<'arena>>,
{
    let simple_class = simple_name(declaring_fqcn);
    for stmt in stmts {
        match stmt {
            Statement::Class(class) if name_eq_ignore_case(class.name.value, simple_class) => {
                let source_text = String::from_utf8_lossy(&file.contents);
                // The methods are attributed to the CONCRETE class (`attributed_class`
                // already carries source casing from `original_name`); for an
                // inherited method, the AST/body lives in this `declaring_fqcn` file.
                let display_class = attributed_class;
                for member in class.members.iter() {
                    let ClassLikeMember::Method(method) = member else {
                        continue;
                    };
                    if let Some(tm) = method_to_test(
                        method,
                        display_class,
                        declaring_class.clone(),
                        file,
                        file_path,
                        names,
                        &source_text,
                    ) {
                        out.push(tm);
                    }
                }
            }
            Statement::Namespace(ns) => {
                use mago_syntax::ast::ast::namespace::NamespaceBody;
                match &ns.body {
                    NamespaceBody::Implicit(b) => collect_methods_in_statements(
                        b.statements.iter(),
                        declaring_fqcn,
                        attributed_class,
                        declaring_class.clone(),
                        file,
                        file_path,
                        names,
                        out,
                    ),
                    NamespaceBody::BraceDelimited(b) => collect_methods_in_statements(
                        b.statements.iter(),
                        declaring_fqcn,
                        attributed_class,
                        declaring_class.clone(),
                        file,
                        file_path,
                        names,
                        out,
                    ),
                };
            }
            _ => {}
        }
    }
}

/// Classify an AST method node into a `TestMethod` if it qualifies.
///
/// `class_name` is the CONCRETE class the method is attributed to; an inherited
/// method also carries `declaring_class = Some(parent FQCN)` (its body's class).
fn method_to_test(
    method: &Method,
    class_name: &str,
    declaring_class: Option<String>,
    file: &mago_database::file::File,
    file_path: &std::path::Path,
    names: &mago_names::ResolvedNames,
    source_text: &str,
) -> Option<TestMethod> {
    let method_name = String::from_utf8_lossy(method.name.value).into_owned();

    let has_test_attr = has_attribute(method, names, ATTR_TEST);
    let method_offset = method.span().start.offset;
    let is_test = method_name.starts_with("test")
        || has_test_attr
        || has_doc_test_annotation(source_text, method_offset as usize);
    if !is_test {
        return None;
    }

    let has_data_provider = extract_data_provider(method, names);
    let line = file.line_number(method_offset) + 1; // 0-based → 1-based

    Some(TestMethod {
        class: class_name.to_string(),
        method: method_name,
        declaring_class,
        file: file_path.to_path_buf(),
        line,
        has_data_provider,
        lifecycle: Default::default(),
    })
}

/// Resolve an attribute's identifier to its fully-qualified name using the
/// name-resolution table; falls back to the raw written name.
fn resolved_attr_name(
    names: &mago_names::ResolvedNames,
    attr_name: &mago_syntax::ast::Identifier,
) -> String {
    match names.resolve(attr_name) {
        Some(fqcn) => String::from_utf8_lossy(fqcn).into_owned(),
        None => String::from_utf8_lossy(attr_name.value()).into_owned(),
    }
}

/// Returns `true` if the method carries the given attribute (by FQCN,
/// case-insensitive, leading-backslash-insensitive).
fn has_attribute(method: &Method, names: &mago_names::ResolvedNames, attr_fqcn: &str) -> bool {
    for attr_list in method.attribute_lists.iter() {
        for attr in attr_list.attributes.iter() {
            let name = resolved_attr_name(names, &attr.name);
            if names_match(&name, attr_fqcn) {
                return true;
            }
        }
    }
    false
}

/// Extract the DataProvider name from the first positional string argument of a
/// `#[DataProvider("name")]` attribute, if present.
fn extract_data_provider(method: &Method, names: &mago_names::ResolvedNames) -> Option<String> {
    use mago_syntax::ast::ast::argument::Argument;
    use mago_syntax::ast::ast::expression::Expression;
    use mago_syntax::ast::ast::literal::Literal;

    for attr_list in method.attribute_lists.iter() {
        for attr in attr_list.attributes.iter() {
            let name = resolved_attr_name(names, &attr.name);
            if !names_match(&name, ATTR_DATA_PROVIDER) {
                continue;
            }
            let Some(arg_list) = &attr.argument_list else {
                continue;
            };
            for arg in arg_list.arguments.iter() {
                let expr = match arg {
                    Argument::Positional(p) => p.value,
                    Argument::Named(n) => n.value,
                };
                if let Expression::Literal(Literal::String(s)) = expr {
                    if let Some(v) = s.value {
                        return Some(String::from_utf8_lossy(v).into_owned());
                    }
                }
            }
        }
    }
    None
}

/// Strip a PHP namespace prefix: `My\\Ns\\ClassName` → `ClassName`.
fn simple_name(fqcn: &str) -> &str {
    fqcn.rsplit('\\').next().unwrap_or(fqcn)
}

/// Case-insensitive compare an AST identifier's raw bytes against a `&str`.
fn name_eq_ignore_case(bytes: &[u8], s: &str) -> bool {
    String::from_utf8_lossy(bytes).eq_ignore_ascii_case(s)
}

/// Returns `true` if the raw source text has a `/** @test */` docblock immediately
/// before `method_offset` (the byte offset of the method declaration).
///
/// Scans up to 300 bytes backwards, finds the last `/**` … `*/` pair, and checks
/// for `@test` not immediately followed by an alphanumeric character or `_`
/// (so `@testWith` and `@testdox` are not matched).
fn has_doc_test_annotation(source_text: &str, method_offset: usize) -> bool {
    // The offset is a BYTE offset that may land mid-UTF-8-codepoint (real suites
    // have accented chars in source); snap both ends to valid char boundaries
    // before slicing, or `source_text[..]` panics.
    let end = floor_char_boundary(source_text, method_offset.min(source_text.len()));
    let window_start = floor_char_boundary(source_text, end.saturating_sub(300));
    let window = source_text[window_start..end].trim_end();
    if !window.ends_with("*/") {
        return false;
    }
    let Some(open) = window.rfind("/**") else {
        return false;
    };
    let docblock = &window[open..];
    let mut pos = 0;
    while let Some(i) = docblock[pos..].find("@test") {
        let abs = pos + i;
        let after = &docblock[abs + 5..];
        match after.chars().next() {
            None | Some(' ') | Some('\n') | Some('\r') | Some('*') | Some('\t') => return true,
            Some(c) if !c.is_alphanumeric() && c != '_' => return true,
            _ => {}
        }
        pos = abs + 1;
    }
    false
}

/// The largest char boundary `<= index` (a stable-Rust stand-in for the unstable
/// `str::floor_char_boundary`). A byte is a boundary unless it is a UTF-8
/// continuation byte (`0b10xx_xxxx`).
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    let bytes = s.as_bytes();
    while i > 0 && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
        i -= 1;
    }
    i
}

/// Compare a resolved attribute name against an expected FQCN.
///
/// mago-reflection may store the name with or without a leading backslash depending
/// on how the PHP source was written. We compare case-insensitively after stripping
/// any leading `\`.
fn names_match(resolved: &str, expected: &str) -> bool {
    let r = resolved.trim_start_matches('\\');
    let e = expected.trim_start_matches('\\');
    r.eq_ignore_ascii_case(e)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn finds_test_prefix_method() {
        let (_d, project) = project_with(
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass MyTest extends TestCase {\n  public function testFoo(): void {}\n  public function helper(): void {}\n}",
        );
        let methods = find_test_methods(&project, &["MyTest".to_string()]);
        assert_eq!(methods.len(), 1, "expected only testFoo; got: {methods:?}");
        assert_eq!(methods[0].method.to_lowercase(), "testfoo");
    }

    #[test]
    fn excludes_non_test_methods() {
        let (_d, project) = project_with(
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass MyTest extends TestCase {\n  public function helper(): void {}\n  public function setUp(): void {}\n}",
        );
        let methods = find_test_methods(&project, &["MyTest".to_string()]);
        assert_eq!(
            methods.len(),
            0,
            "expected no test methods; got: {methods:?}"
        );
    }

    #[test]
    fn finds_test_attribute_method() {
        let (_d, project) = project_with(concat!(
            "<?php\n",
            "use PHPUnit\\Framework\\TestCase;\n",
            "use PHPUnit\\Framework\\Attributes\\Test;\n",
            "class MyTest extends TestCase {\n",
            "  #[Test]\n",
            "  public function itDoesSomething(): void {}\n",
            "  public function helper(): void {}\n",
            "}"
        ));
        let methods = find_test_methods(&project, &["MyTest".to_string()]);
        assert!(
            methods
                .iter()
                .any(|m| m.method.to_lowercase() == "itdoessomething"),
            "expected itDoesSomething via #[Test]; got: {methods:?}"
        );
        assert!(
            !methods.iter().any(|m| m.method.to_lowercase() == "helper"),
            "helper should not be a test method; got: {methods:?}"
        );
    }

    #[test]
    fn extracts_data_provider_attribute() {
        let (_d, project) = project_with(concat!(
            "<?php\n",
            "use PHPUnit\\Framework\\TestCase;\n",
            "use PHPUnit\\Framework\\Attributes\\DataProvider;\n",
            "class MyTest extends TestCase {\n",
            "  #[DataProvider('provideData')]\n",
            "  public function testFoo(int $x): void {}\n",
            "  public static function provideData(): array { return []; }\n",
            "}"
        ));
        let methods = find_test_methods(&project, &["MyTest".to_string()]);
        let test_foo = methods
            .iter()
            .find(|m| m.method.to_lowercase() == "testfoo");
        assert!(
            test_foo.is_some(),
            "expected testFoo in results; got: {methods:?}"
        );
        assert_eq!(
            test_foo.unwrap().has_data_provider.as_deref(),
            Some("provideData"),
            "expected DataProvider name 'provideData'; got: {:?}",
            test_foo.unwrap().has_data_provider
        );
    }

    #[test]
    fn surfaces_inherited_test_methods_to_concrete_subclass() {
        // Inc-4 C: a test declared in an abstract `*TestCase` is surfaced to the
        // CONCRETE subclass, attributed to the subclass with `declaring_class` set
        // to the parent. The abstract class itself contributes NO rows.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("BaseTestCase.php"),
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nabstract class BaseTestCase extends TestCase {\n  public function testInherited(): void {}\n}",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ConcreteTest.php"),
            "<?php\nclass ConcreteTest extends BaseTestCase {\n  public function testOwn(): void {}\n}",
        )
        .unwrap();
        let project = MagoProject::load(dir.path()).unwrap();

        let methods = find_test_methods(
            &project,
            &["BaseTestCase".to_string(), "ConcreteTest".to_string()],
        );

        // The abstract class emits nothing of its own.
        assert!(
            !methods
                .iter()
                .any(|m| m.class.eq_ignore_ascii_case("BaseTestCase")),
            "abstract class must not emit rows; got {methods:?}"
        );
        // The concrete subclass gets BOTH its own and the inherited method.
        let own = methods
            .iter()
            .find(|m| m.method.eq_ignore_ascii_case("testOwn"))
            .expect("testOwn surfaced");
        assert!(own.class.eq_ignore_ascii_case("ConcreteTest"));
        assert_eq!(
            own.declaring_class, None,
            "own method has no declaring_class"
        );

        let inherited = methods
            .iter()
            .find(|m| m.method.eq_ignore_ascii_case("testInherited"))
            .expect("testInherited surfaced to ConcreteTest");
        assert!(
            inherited.class.eq_ignore_ascii_case("ConcreteTest"),
            "inherited method must be attributed to the concrete class; got {inherited:?}"
        );
        assert!(
            inherited
                .declaring_class
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case("BaseTestCase")),
            "inherited method must carry its declaring class; got {inherited:?}"
        );
    }

    #[test]
    fn doc_annotation_scan_is_utf8_safe() {
        // A method preceded by source containing multi-byte UTF-8 (accented chars)
        // must not panic the docblock scanner (regression: byte-offset slice landed
        // mid-codepoint on real suites like symfony/string).
        let (_d, project) = project_with(concat!(
            "<?php\n",
            "use PHPUnit\\Framework\\TestCase;\n",
            "class MyTest extends TestCase {\n",
            "  // un café très élégant à côté — accents accents accents accents\n",
            "  /** @test */\n",
            "  public function itWorks(): void {}\n",
            "}"
        ));
        let methods = find_test_methods(&project, &["MyTest".to_string()]);
        assert!(
            methods.iter().any(|m| m.method == "itWorks"),
            "the @test method must be found without a UTF-8 panic; got {methods:?}"
        );
    }

    #[test]
    fn returns_empty_for_unknown_class() {
        let (_d, project) = project_with(
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass MyTest extends TestCase {\n  public function testFoo(): void {}\n}",
        );
        let methods = find_test_methods(&project, &["NonExistent".to_string()]);
        assert_eq!(methods.len(), 0, "unknown class should yield no methods");
    }

    #[test]
    fn reports_nonzero_line_number() {
        let (_d, project) = project_with(
            "<?php\nuse PHPUnit\\Framework\\TestCase;\nclass MyTest extends TestCase {\n  public function testFoo(): void {}\n}",
        );
        let methods = find_test_methods(&project, &["MyTest".to_string()]);
        assert_eq!(methods.len(), 1);
        assert!(
            methods[0].line > 0,
            "expected a non-zero line number; got {}",
            methods[0].line
        );
    }
}
