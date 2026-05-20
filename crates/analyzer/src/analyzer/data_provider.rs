//! Data provider expansion: turn a parameterized test method into one
//! ExpandedTest per data row.
//!
//! # Approach: direct AST walk (Fallback A — re-parse per module)
//!
//! mago-project's `Module` does not cache the parsed `Program`; the AST is
//! discarded after reflection is built.  We recover it on demand via
//! `Module::parse(&interner)`, which re-runs `mago_syntax::parser::parse_source`
//! on the in-memory source bytes.  This is slightly wasteful but avoids storing
//! the AST across threads, and is fast enough for the analysis-time use case.
//!
//! Walk:
//!   Project.modules → Module with matching source identifier
//!     → module.parse(&interner) → Program.statements
//!       → flatten through Namespace wrappers
//!         → Statement::Class whose name matches `class_name`
//!           → ClassLikeMember::Method whose name matches `provider_name`
//!             → MethodBody::Concrete(block)
//!               → first Statement::Return → Return.value

use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
use mago_syntax::ast::ast::statement::Statement;
use mago_syntax::ast::ast::expression::Expression;

use crate::concrete::{compute, ArrayKey, Context, PhpValue};
use crate::mago_bridge::MagoProject;
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
        vec![ExpandedTest { test: test.clone(), data_set: None, args: vec![] }]
    };

    let Some(provider_name) = &test.has_data_provider else {
        return no_data();
    };

    let Some(return_expr) =
        find_provider_return_expr(project, &test.class, provider_name)
    else {
        return no_data();
    };

    let mut ctx = Context::new();
    let value = match compute(&return_expr, &mut ctx) {
        Ok(v) => v,
        Err(_) => return no_data(),
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
            ExpandedTest { test: test.clone(), data_set: Some(data_set), args }
        })
        .collect()
}

/// Find the return Expression of `class_name::provider_name` by re-parsing
/// the module that contains the class and walking the AST.
///
/// Returns `None` whenever anything along the way fails.
fn find_provider_return_expr(
    project: &MagoProject,
    class_name: &str,
    provider_name: &str,
) -> Option<Expression> {
    let interner = project.interner();
    let inner = project.inner();

    // Find the Module that contains the class definition.
    // We use the class reflection to get the span → source identifier, then
    // find the Module whose source identifier matches.
    let class_source_id = {
        let class_key = class_name.to_lowercase();
        let mut found: Option<mago_source::SourceIdentifier> = None;
        for (name, refl) in project.class_likes() {
            if project.class_name_str(name).to_lowercase() == class_key {
                found = Some(refl.span.start.source);
                break;
            }
        }
        found?
    };

    // Locate the matching Module and re-parse it.
    let module = inner
        .modules
        .iter()
        .find(|m| m.source.identifier == class_source_id)?;

    let program = module.parse(interner);

    // Walk statements, transparently descending into Namespace wrappers.
    find_return_in_statements(
        program.statements.iter(),
        class_name,
        provider_name,
        interner,
    )
}

/// Recursively walk a statement list, descending into namespaces, looking for
/// the named class + method and returning its first return expression.
fn find_return_in_statements<'s, I>(
    stmts: I,
    class_name: &str,
    provider_name: &str,
    interner: &mago_interner::ThreadedInterner,
) -> Option<Expression>
where
    I: Iterator<Item = &'s Statement>,
{
    // Strip any leading namespace from class_name for matching purposes.
    // The AST `Class.name` is a LocalIdentifier, so it only contains the
    // unqualified name (e.g. "MyTest", not "My\\Ns\\MyTest").
    let simple_class = simple_name(class_name);
    let simple_provider = simple_name(provider_name);

    for stmt in stmts {
        match stmt {
            Statement::Class(class) => {
                let ast_name = interner.lookup(&class.name.value);
                if ast_name.eq_ignore_ascii_case(simple_class) {
                    return find_method_return(&class.members, simple_provider, interner);
                }
            }
            Statement::Namespace(ns) => {
                // NamespaceBody has both Implicit and BraceDelimited variants;
                // both expose `.statements` via a method.
                use mago_syntax::ast::ast::namespace::NamespaceBody;
                let inner_stmts = match &ns.body {
                    NamespaceBody::Implicit(b) => b.statements.iter(),
                    NamespaceBody::BraceDelimited(b) => b.statements.iter(),
                };
                if let Some(expr) = find_return_in_statements(
                    inner_stmts,
                    class_name,
                    provider_name,
                    interner,
                ) {
                    return Some(expr);
                }
            }
            _ => {}
        }
    }
    None
}

/// Search `members` for a concrete method named `provider_name` and return
/// the expression from its first `return` statement.
fn find_method_return(
    members: &mago_syntax::ast::sequence::Sequence<ClassLikeMember>,
    provider_name: &str,
    interner: &mago_interner::ThreadedInterner,
) -> Option<Expression> {
    use mago_syntax::ast::ast::class_like::method::MethodBody;

    for member in members.iter() {
        let ClassLikeMember::Method(method) = member else {
            continue;
        };
        let ast_name = interner.lookup(&method.name.value);
        if !ast_name.eq_ignore_ascii_case(provider_name) {
            continue;
        }
        let MethodBody::Concrete(block) = &method.body else {
            continue;
        };
        // Find first `return <expr>;` with a value.
        for stmt in block.statements.iter() {
            if let Statement::Return(ret) = stmt {
                if let Some(expr) = &ret.value {
                    return Some(expr.clone());
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
            dir.path().join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
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
            dir.path().join("vendor/phpunit/phpunit/src/Framework/Attributes"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
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
        assert_eq!(expanded.len(), 2, "expected 2 expansions; got: {expanded:?}");

        let data_sets: Vec<&str> =
            expanded.iter().filter_map(|e| e.data_set.as_deref()).collect();
        assert!(data_sets.contains(&"first"), "expected 'first' data set");
        assert!(data_sets.contains(&"second"), "expected 'second' data set");
    }
}
