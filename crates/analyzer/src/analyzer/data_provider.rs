//! Data provider expansion: turn a parameterized test method into one
//! ExpandedTest per data row.
//!
//! # Approach: direct AST walk (re-parse on demand via the bridge)
//!
//! mago 1.30 does not cache the parsed `Program`; the AST is discarded after
//! reflection is built.  We recover it on demand via
//! [`MagoProject::with_program`], which re-parses the file into a scoped scratch
//! arena.  Because the AST is arena-bound (`'arena` lifetime), the concrete
//! evaluation of the provider's return expression happens INSIDE the closure —
//! we extract only the owned `PhpValue`, never the AST node.
//!
//! Walk:
//!   class reflection → declaring file → with_program(file) → Program.statements
//!     → flatten through Namespace wrappers
//!       → Statement::Class whose name matches `class_name`
//!         → ClassLikeMember::Method whose name matches `provider_name`
//!           → MethodBody::Concrete(block)
//!             → first Statement::Return → Return.value → compute() → PhpValue

use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
use mago_syntax::ast::ast::expression::Expression;
use mago_syntax::ast::ast::statement::Statement;

use crate::concrete::{compute, ArrayKey, Context, PhpValue};
use crate::mago_bridge::{word_to_string, MagoProject};
use crate::test_discovery::TestMethod;

/// A test invocation with concrete arguments. For non-parameterized tests
/// the args are empty and data_set is None.
#[derive(Debug, Clone)]
pub struct ExpandedTest {
    pub test: TestMethod,
    pub data_set: Option<String>,
    pub args: Vec<PhpValue>,
}

/// Expand a test method into one or more invocations.
///
/// If the test has no data provider, returns a single `ExpandedTest` with no
/// data_set and empty args.  On any failure (provider not found, return
/// expression not concretely-computable, return value not an array) falls
/// back to the same no-data variant.
pub fn expand(project: &MagoProject, test: &TestMethod) -> Vec<ExpandedTest> {
    let no_data = || {
        vec![ExpandedTest {
            test: test.clone(),
            data_set: None,
            args: vec![],
        }]
    };

    let Some(provider_name) = &test.has_data_provider else {
        return no_data();
    };

    // The provider method lives where the test method's BODY lives: for an
    // inherited test (Inc-4 C), that is the declaring `*TestCase`, not the concrete
    // subclass. `body_class()` resolves to the right one.
    let Some(value) = compute_provider_return(project, test.body_class(), provider_name) else {
        return no_data();
    };

    let PhpValue::Array(rows) = value else {
        return no_data();
    };

    rows.into_iter()
        .enumerate()
        .map(|(idx, (key, row))| {
            let data_set = match key {
                ArrayKey::Int(_) => idx.to_string(),
                ArrayKey::String(s) => s,
            };
            let args = match row {
                PhpValue::Array(a) => a.into_values().collect(),
                other => vec![other],
            };
            ExpandedTest {
                test: test.clone(),
                data_set: Some(data_set),
                args,
            }
        })
        .collect()
}

/// Concretely evaluate the return value of `class_name::provider_name` by
/// re-parsing the file that declares the class and walking the AST.
///
/// The evaluation happens inside [`MagoProject::with_program`] because the AST
/// is arena-bound; only the owned `PhpValue` escapes. Returns `None` whenever
/// anything along the way fails (class not found, no concrete return, the
/// expression is not concretely-computable).
fn compute_provider_return(
    project: &MagoProject,
    class_name: &str,
    provider_name: &str,
) -> Option<PhpValue> {
    // Find the file that declares the class via the codebase reflection.
    let class_key = class_name.trim_start_matches('\\').to_lowercase();
    let refl = project.class_likes().find(|r| {
        word_to_string(&r.name)
            .trim_start_matches('\\')
            .eq_ignore_ascii_case(&class_key)
    })?;
    let file = project.file_of_span(&refl.span)?;
    let logical_name = String::from_utf8_lossy(&file.name).into_owned();

    project.with_program(&logical_name, |program, _file, _names| {
        let return_expr =
            find_return_in_statements(program.statements.iter(), class_name, provider_name)?;
        let mut ctx = Context::new();
        compute(return_expr, &mut ctx).ok()
    })?
}

/// Recursively walk a statement list, descending into namespaces, looking for
/// the named class + method and returning its first return expression.
fn find_return_in_statements<'s, 'arena, I>(
    stmts: I,
    class_name: &str,
    provider_name: &str,
) -> Option<&'s Expression<'arena>>
where
    'arena: 's,
    I: Iterator<Item = &'s Statement<'arena>>,
{
    // Strip any leading namespace from class_name for matching purposes.
    // The AST `Class.name` is a LocalIdentifier, so it only contains the
    // unqualified name (e.g. "MyTest", not "My\\Ns\\MyTest").
    let simple_class = simple_name(class_name);
    let simple_provider = simple_name(provider_name);

    for stmt in stmts {
        match stmt {
            Statement::Class(class) => {
                if name_eq_ignore_case(class.name.value, simple_class) {
                    return find_method_return(&class.members, simple_provider);
                }
            }
            Statement::Namespace(ns) => {
                // NamespaceBody has both Implicit and BraceDelimited variants;
                // both expose `.statements`.
                use mago_syntax::ast::ast::namespace::NamespaceBody;
                let found = match &ns.body {
                    NamespaceBody::Implicit(b) => {
                        find_return_in_statements(b.statements.iter(), class_name, provider_name)
                    }
                    NamespaceBody::BraceDelimited(b) => {
                        find_return_in_statements(b.statements.iter(), class_name, provider_name)
                    }
                };
                if found.is_some() {
                    return found;
                }
            }
            _ => {}
        }
    }
    None
}

/// Search `members` for a concrete method named `provider_name` and return
/// the expression from its first `return` statement.
fn find_method_return<'s, 'arena>(
    members: &'s mago_syntax::ast::sequence::Sequence<'arena, ClassLikeMember<'arena>>,
    provider_name: &str,
) -> Option<&'s Expression<'arena>> {
    use mago_syntax::ast::ast::class_like::method::MethodBody;

    for member in members.iter() {
        let ClassLikeMember::Method(method) = member else {
            continue;
        };
        if !name_eq_ignore_case(method.name.value, provider_name) {
            continue;
        }
        let MethodBody::Concrete(block) = &method.body else {
            continue;
        };
        // Find first `return <expr>;` with a value.
        for stmt in block.statements.iter() {
            if let Statement::Return(ret) = stmt {
                if let Some(expr) = ret.value {
                    return Some(expr);
                }
            }
        }
    }
    None
}

/// Case-insensitive compare an AST identifier's raw bytes against a `&str`.
fn name_eq_ignore_case(bytes: &[u8], s: &str) -> bool {
    String::from_utf8_lossy(bytes).eq_ignore_ascii_case(s)
}

/// Strip a PHP namespace prefix: `My\\Ns\\ClassName` → `ClassName`.
fn simple_name(fqcn: &str) -> &str {
    fqcn.rsplit('\\').next().unwrap_or(fqcn)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheStore;
    use crate::test_discovery::discover;

    #[test]
    fn no_provider_yields_single_invocation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        )
        .unwrap();
        let test_file = dir.path().join("MyTest.php");
        std::fs::write(
            &test_file,
            r#"<?php
use PHPUnit\Framework\TestCase;
class MyTest extends TestCase {
    public function testFoo(): void {}
}
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let cache = CacheStore::open(dir.path(), MagoProject::version()).unwrap();
        let tests = discover(&project, &cache, &[test_file]).unwrap();
        assert_eq!(tests.len(), 1);

        let expanded = expand(&project, &tests[0]);
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].data_set, None);
        assert!(expanded[0].args.is_empty());
    }

    #[test]
    fn pure_array_provider_expands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        std::fs::create_dir_all(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/Attributes"),
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/Attributes/DataProvider.php"),
            "<?php namespace PHPUnit\\Framework\\Attributes; class DataProvider { public function __construct(string $name) {} }",
        )
        .unwrap();
        let test_file = dir.path().join("MyTest.php");
        std::fs::write(
            &test_file,
            r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
class MyTest extends TestCase {
    public static function provider(): array {
        return [
            'first' => [1, 2],
            'second' => [3, 4],
        ];
    }

    #[DataProvider('provider')]
    public function testAdd(int $a, int $b): void {}
}
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let cache = CacheStore::open(dir.path(), MagoProject::version()).unwrap();
        let tests = discover(&project, &cache, &[test_file]).unwrap();

        let test = tests
            .iter()
            .find(|t| t.method.to_lowercase() == "testadd")
            .unwrap();
        assert_eq!(test.has_data_provider.as_deref(), Some("provider"));

        let expanded = expand(&project, test);
        assert_eq!(
            expanded.len(),
            2,
            "expected 2 expansions; got: {expanded:?}"
        );

        let data_sets: Vec<&str> = expanded
            .iter()
            .filter_map(|e| e.data_set.as_deref())
            .collect();
        assert!(data_sets.contains(&"first"), "expected 'first' data set");
        assert!(data_sets.contains(&"second"), "expected 'second' data set");
    }
}
