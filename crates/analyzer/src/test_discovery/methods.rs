//! Method identification for PHPUnit test classes.
//!
//! # API investigation findings (Task 9)
//!
//! ## What mago-reflection 0.26 exposes for FunctionLikeReflection
//! - `attribute_reflections: Vec<AttributeReflection>` — PHP 8 `#[...]` attributes ARE parsed.
//!   - `AttributeReflection { name: Name, arguments: Option<AttributeArgumentListReflection>, … }`
//!   - `AttributeArgumentReflection::Positional { value_type_reflection: TypeReflection, … }`
//!   - `TypeReflection { kind: TypeKind, … }` where `TypeKind::Value(ValueTypeKind::String { value, … })`
//!     carries the interned string literal — usable for `#[DataProvider("name")]` extraction.
//! - `name: FunctionLikeName::Method(ClassLikeName, Name)` — method name via `interner.lookup(&name.value)`.
//! - `span: Span` — `span.start.source` is the `SourceIdentifier`, `span.start.offset` is the byte offset.
//! - Line numbers: `Source::line_number(offset)` (0-based). We look up the `Source` via `project.source_by_id`.
//! - File path: `interner.lookup(&span.start.source.0)` gives the name string used when creating the source
//!   (which equals the file path string, since `MagoProject::load` uses `path.display().to_string()` as the name).
//!
//! ## What is NOT available
//! - **Doc comments**: `FunctionLikeReflection` has no `doc_comment` field.
//!   Therefore `@test` and `@dataProvider` PHPDoc annotation detection is NOT possible
//!   through the reflection layer alone. Only PHP 8 attribute syntax is supported.

use std::path::PathBuf;

use mago_reflection::attribute::AttributeArgumentReflection;
use mago_reflection::identifier::FunctionLikeName;
use mago_reflection::r#type::kind::{TypeKind, ValueTypeKind};

use super::TestMethod;
use crate::mago_bridge::MagoProject;

/// Attribute FQNs for the `#[Test]` and `#[DataProvider("name")]` attributes.
/// mago-reflection stores attribute names as they appear after name resolution,
/// i.e., the FQCN without the leading backslash.
const ATTR_TEST: &str = "PHPUnit\\Framework\\Attributes\\Test";
const ATTR_DATA_PROVIDER: &str = "PHPUnit\\Framework\\Attributes\\DataProvider";

/// Returns the list of test methods declared on the given test classes.
///
/// A method is considered a "test method" if any of:
/// 1. Its name starts with `test` (case-sensitive, PHPUnit convention).
/// 2. It carries the `#[PHPUnit\Framework\Attributes\Test]` attribute.
///
/// `has_data_provider` is set when the method carries a
/// `#[PHPUnit\Framework\Attributes\DataProvider("name")]` attribute.
///
/// Note: `@test` and `@dataProvider` doc annotations are NOT supported because
/// `FunctionLikeReflection` in mago-reflection 0.26 does not expose doc comments.
pub fn find_test_methods(project: &MagoProject, test_classes: &[String]) -> Vec<TestMethod> {
    let interner = project.interner();

    // Build a lowercased FQCN → reflection lookup.
    let class_index: std::collections::HashMap<
        String,
        &mago_reflection::class_like::ClassLikeReflection,
    > = project
        .class_likes()
        .map(|(name, refl)| (project.class_name_str(name).to_lowercase(), refl))
        .collect();

    let mut out = Vec::new();

    for class_name in test_classes {
        let key = class_name.to_lowercase();
        let Some(class_refl) = class_index.get(&key) else {
            continue;
        };

        for (_method_id, method_refl) in class_refl.methods.members.iter() {
            // Extract the plain method name from FunctionLikeName.
            let method_name: String = match &method_refl.name {
                FunctionLikeName::Method(_, name) => interner.lookup(&name.value).to_string(),
                _ => continue, // skip closures / bare functions (shouldn't occur here)
            };

            // Determine if this is a test method.
            let has_test_attr = has_attribute(method_refl, ATTR_TEST, interner);
            let is_test = method_name.starts_with("test") || has_test_attr || {
                // Fall back to raw source scan for `@test` PHPDoc annotations.
                // (mago-reflection 0.26 does not expose doc comments.)
                let src_id = method_refl.span.start.source;
                project.source_by_id(src_id).map_or(false, |src| {
                    let text = interner.lookup(&src.content);
                    has_doc_test_annotation(text, method_refl.span.start.offset)
                })
            };
            if !is_test {
                continue;
            }

            // Extract DataProvider attribute argument if present.
            let has_data_provider = extract_data_provider(method_refl, interner);

            // Resolve file path and line number from the span.
            let span = method_refl.span;
            let source_id = span.start.source;
            let file: PathBuf = PathBuf::from(interner.lookup(&source_id.0).to_string());
            let line: u32 = project
                .source_by_id(source_id)
                .map(|src| src.line_number(span.start.offset) as u32 + 1) // convert 0-based → 1-based
                .unwrap_or(0);

            out.push(TestMethod {
                class: class_name.clone(),
                method: method_name,
                file,
                line,
                has_data_provider,
                lifecycle: Default::default(), // filled by Task 10
            });
        }
    }

    out
}

/// Returns `true` if the method carries the given attribute (by FQCN, case-insensitive tail match).
fn has_attribute(
    method_refl: &mago_reflection::function_like::FunctionLikeReflection,
    attr_fqcn: &str,
    interner: &mago_interner::ThreadedInterner,
) -> bool {
    method_refl.attribute_reflections.iter().any(|attr| {
        let name = interner.lookup(&attr.name.value);
        names_match(name.as_ref(), attr_fqcn)
    })
}

/// Extract the DataProvider name from the first positional string argument of
/// a `#[DataProvider("name")]` attribute, if present.
fn extract_data_provider(
    method_refl: &mago_reflection::function_like::FunctionLikeReflection,
    interner: &mago_interner::ThreadedInterner,
) -> Option<String> {
    for attr in &method_refl.attribute_reflections {
        let attr_name = interner.lookup(&attr.name.value);
        if !names_match(attr_name.as_ref(), ATTR_DATA_PROVIDER) {
            continue;
        }
        // Found the DataProvider attribute — look for the first positional string arg.
        if let Some(arg_list) = &attr.arguments {
            for arg in &arg_list.arguments {
                if let AttributeArgumentReflection::Positional {
                    value_type_reflection,
                    ..
                } = arg
                {
                    if let TypeKind::Value(ValueTypeKind::String { value, .. }) =
                        &value_type_reflection.kind
                    {
                        return Some(interner.lookup(value).to_string());
                    }
                }
            }
        }
    }
    None
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
