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
    let class_index: std::collections::HashMap<String, &mago_codex::metadata::class_like::ClassLikeMetadata> =
        project
            .class_likes()
            .map(|refl| (word_to_string(&refl.name).to_lowercase(), refl))
            .collect();

    for class_name in test_classes {
        let key = class_name.trim_start_matches('\\').to_lowercase();
        let Some(class_refl) = class_index.get(&key) else {
            continue;
        };
        let Some(file) = project.file_of_span(&class_refl.span) else {
            continue;
        };
        let logical_name = String::from_utf8_lossy(&file.name).into_owned();
        let file_path: PathBuf = match &file.path {
            Some(p) => p.clone(),
            None => PathBuf::from(logical_name.clone()),
        };

        let methods = project
            .with_program(&logical_name, |program, file, _names| {
                let mut found = Vec::new();
                collect_methods_in_statements(
                    program.statements.iter(),
                    class_name,
                    file,
                    &file_path,
                    &mut found,
                );
                found
            })
            .unwrap_or_default();

        out.extend(methods);
    }

    out
}

/// Recursively walk a statement list (descending into namespaces) for the named
/// class, and collect its test methods into `out`.
fn collect_methods_in_statements<'s, 'arena, I>(
    stmts: I,
    class_name: &str,
    file: &mago_database::file::File,
    file_path: &std::path::Path,
    out: &mut Vec<TestMethod>,
) where
    'arena: 's,
    I: Iterator<Item = &'s Statement<'arena>>,
{
    let simple_class = simple_name(class_name);
    for stmt in stmts {
        match stmt {
            Statement::Class(class) if name_eq_ignore_case(class.name.value, simple_class) => {
                let source_text = String::from_utf8_lossy(&file.contents);
                for member in class.members.iter() {
                    let ClassLikeMember::Method(method) = member else {
                        continue;
                    };
                    if let Some(tm) =
                        method_to_test(method, class_name, file, file_path, &source_text)
                    {
                        out.push(tm);
                    }
                }
            }
            Statement::Namespace(ns) => {
                use mago_syntax::ast::ast::namespace::NamespaceBody;
                match &ns.body {
                    NamespaceBody::Implicit(b) => collect_methods_in_statements(
                        b.statements.iter(),
                        class_name,
                        file,
                        file_path,
                        out,
                    ),
                    NamespaceBody::BraceDelimited(b) => collect_methods_in_statements(
                        b.statements.iter(),
                        class_name,
                        file,
                        file_path,
                        out,
                    ),
                };
            }
            _ => {}
        }
    }
}

/// Classify an AST method node into a `TestMethod` if it qualifies.
fn method_to_test(
    method: &Method,
    class_name: &str,
    file: &mago_database::file::File,
    file_path: &std::path::Path,
    source_text: &str,
) -> Option<TestMethod> {
    let method_name = String::from_utf8_lossy(method.name.value).into_owned();

    let has_test_attr = has_attribute(method, ATTR_TEST);
    let method_offset = method.span().start.offset;
    let is_test = method_name.starts_with("test")
        || has_test_attr
        || has_doc_test_annotation(source_text, method_offset as usize);
    if !is_test {
        return None;
    }

    let has_data_provider = extract_data_provider(method);
    let line = file.line_number(method_offset) + 1; // 0-based → 1-based

    Some(TestMethod {
        class: class_name.to_string(),
        method: method_name,
        file: file_path.to_path_buf(),
        line,
        has_data_provider,
        lifecycle: Default::default(),
    })
}

/// Returns `true` if the method carries the given attribute (by FQCN,
/// case-insensitive, leading-backslash-insensitive).
fn has_attribute(method: &Method, attr_fqcn: &str) -> bool {
    for attr_list in method.attribute_lists.iter() {
        for attr in attr_list.attributes.iter() {
            let name = String::from_utf8_lossy(attr.name.value());
            if names_match(&name, attr_fqcn) {
                return true;
            }
        }
    }
    false
}

/// Extract the DataProvider name from the first positional string argument of a
/// `#[DataProvider("name")]` attribute, if present.
fn extract_data_provider(method: &Method) -> Option<String> {
    use mago_syntax::ast::ast::argument::Argument;
    use mago_syntax::ast::ast::expression::Expression;
    use mago_syntax::ast::ast::literal::Literal;

    for attr_list in method.attribute_lists.iter() {
        for attr in attr_list.attributes.iter() {
            let name = String::from_utf8_lossy(attr.name.value());
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
    let end = method_offset.min(source_text.len());
    let window_start = end.saturating_sub(300);
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
