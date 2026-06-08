//! Per-test execution trace (Phase 2).
//!
//! Walks each test method body via the type tracker, emits CallSiteEvents,
//! resolves them through dispatch + opacity, and recurses into traced
//! callees, marking lines along the way.

use super::{Coverage, TestId};
use crate::boundary::BoundaryResolver;
use crate::mago_bridge::{word_to_string, MagoProject};
use crate::opacity::{self, Opacity, ReceiverType};
use crate::test_discovery::TestMethod;
use crate::types::env::TypeEnv;
use crate::types::walker::{walk_block, CallSiteEvent, WalkerCtx};
use mago_codex::metadata::function_like::FunctionLikeMetadata;
use mago_codex::symbol::SymbolKind;
use mago_syntax::ast::class_like::member::ClassLikeMember;
use mago_syntax::ast::class_like::method::MethodBody;
use mago_syntax::ast::Statement;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const MAX_RECURSION_DEPTH: usize = 50;

/// Trace a single test method invocation (possibly parameterized by a data set
/// row) and return a Coverage map of the lines it visited.
pub fn trace_test(
    project: &MagoProject,
    boundary: &BoundaryResolver,
    test: &TestMethod,
    data_set: Option<String>,
) -> Coverage {
    let test_id = TestId {
        class: test.class.clone(),
        method: test.method.clone(),
        data_set,
    };
    let mut coverage: Coverage = HashMap::new();
    let mut visiting: HashSet<(String, String)> = HashSet::new();

    trace_method(
        project,
        boundary,
        &test.class,
        &test.method,
        &test_id,
        &mut coverage,
        &mut visiting,
        0,
    );

    coverage
}

// justification: recursive call-graph walker; each argument is distinct state threaded
// through the recursion, and a context struct would only relocate the arity.
#[allow(clippy::too_many_arguments)]
fn trace_method(
    project: &MagoProject,
    boundary: &BoundaryResolver,
    class: &str,
    method: &str,
    test_id: &TestId,
    coverage: &mut Coverage,
    visiting: &mut HashSet<(String, String)>,
    depth: usize,
) {
    if depth > MAX_RECURSION_DEPTH {
        return;
    }
    let key = (class.to_lowercase(), method.to_lowercase());
    if !visiting.insert(key.clone()) {
        return;
    }

    // 1. Find the class + method reflections.
    let Some((file_path, start_line, end_line, events)) =
        collect_method_info(project, class, method)
    else {
        visiting.remove(&key);
        return;
    };

    // 2. Mark every line in the method body span.
    for line in start_line..=end_line {
        coverage
            .entry(file_path.clone())
            .or_default()
            .entry(line)
            .or_default()
            .push(test_id.clone());
    }

    // 3. For each call-site event, decide opacity and recurse if traced.
    for event in events {
        let mut call_site = crate::analyzer::dispatch::resolve(&event, Some(class));
        // The walker sets callee_file to the receiver-type's class definition file.
        // For inherited methods the body lives in an ancestor file — override here
        // so the boundary check uses the file where the method is actually defined.
        if let Some(cc) = call_site.callee_class.clone() {
            let method_lc = call_site.callee_method.to_lowercase();
            if let Some((m_refl, _)) = find_method_in_hierarchy(project, &cc, &method_lc) {
                if let Some(src) = project.file_of_span(&m_refl.span) {
                    call_site.callee_file = Some(file_path_of(src));
                }
            }
        }
        // Partial-mock transparency: a mock of a known concrete/abstract class
        // (not an interface or trait) should trace into real method implementations,
        // since only selected methods are stubbed while others call real code.
        if call_site.receiver_type == ReceiverType::Mock {
            if let Some(cc) = call_site.callee_class.clone() {
                let is_traceable_class = project
                    .find_class(&cc)
                    .map(|r| r.kind != SymbolKind::Interface && r.kind != SymbolKind::Trait)
                    .unwrap_or(false);
                if is_traceable_class {
                    call_site.receiver_type = ReceiverType::Concrete(cc);
                }
            }
        }
        match opacity::decide(&call_site, boundary) {
            Opacity::Opaque => {}
            Opacity::Trace => {
                if let Some(callee_class) = &call_site.callee_class {
                    trace_method(
                        project,
                        boundary,
                        callee_class,
                        &call_site.callee_method,
                        test_id,
                        coverage,
                        visiting,
                        depth + 1,
                    );
                }
            }
        }
    }

    visiting.remove(&key);
}

/// Find the method in the project, mark its lines, walk its body, and return
/// `(file_path, start_line, end_line, events)`.
///
/// Returns `None` if the class or method cannot be found anywhere in the
/// inheritance chain.
fn collect_method_info(
    project: &MagoProject,
    class: &str,
    method: &str,
) -> Option<(PathBuf, u32, u32, Vec<CallSiteEvent>)> {
    let target_method_lc = method.to_lowercase();

    // 1. Walk the inheritance chain to find which class defines the method.
    //    `defining_class` is where the method body lives (may differ from `class`
    //    when the method is inherited). `TypeEnv` still uses the original `class`
    //    so `$this` resolves to the subclass — giving virtual dispatch for free.
    let (method_refl, defining_class) =
        find_method_in_hierarchy(project, class, &target_method_lc)?;

    // 2. Resolve span → file path + line range.
    let span = method_refl.span;
    let source = project.file_of_span(&span)?;
    let file_path = file_path_of(source);
    let start_line = source.line_number(span.start.offset) + 1;
    let end_line = source.line_number(span.end.offset) + 1;
    let logical_name = String::from_utf8_lossy(&source.name).into_owned();

    // 3. Seed parameter types from the method reflection (independent of the AST).
    let mut env = TypeEnv::for_class(class);
    seed_param_types(&mut env, project, method_refl);

    // 4. Re-parse the declaring file on demand and walk the method body inside the
    //    closure (the AST + resolved names are arena-bound). Use `defining_class`
    //    for AST navigation (the source contains `class JsonFormatter`, not the
    //    subclass name).
    let events = project.with_program(&logical_name, |program, _file, names| {
        let mut ctx = WalkerCtx::new(env, project, names);
        walk_method_body(&mut ctx, program, &defining_class, method);
        ctx.events
    })?;

    Some((file_path, start_line, end_line, events))
}

/// Walk up the inheritance chain to find the closest ancestor (including `start_class`
/// itself) that defines `method_lc`. Returns the reflection and the FQCN of the
/// class that defines it.
fn find_method_in_hierarchy<'a>(
    project: &'a MagoProject,
    start_class: &str,
    method_lc: &str,
) -> Option<(&'a FunctionLikeMetadata, String)> {
    let codebase = project.codebase();
    let mut current = start_class.trim_start_matches('\\').to_lowercase();

    for _ in 0..MAX_RECURSION_DEPTH {
        // Find the metadata for `current`.
        let class_refl = project.find_class(&current)?;
        let class_fqcn = word_to_string(&class_refl.name);

        // Look for the method directly on this class (names are in `methods`).
        if class_refl.methods.iter().any(|m| word_to_string(m).to_lowercase() == method_lc) {
            if let Some(m_refl) = codebase.get_method(current.as_bytes(), method_lc.as_bytes()) {
                return Some((m_refl, class_fqcn));
            }
        }

        // Not in own methods — search traits used by this class. Trait methods
        // are not inlined into `methods`, so look up each trait explicitly.
        for trait_name in class_refl.used_traits.iter() {
            let trait_fqcn_lc = word_to_string(trait_name)
                .trim_start_matches('\\')
                .to_lowercase();
            if let Some(t_refl) = project.find_class(&trait_fqcn_lc) {
                if t_refl
                    .methods
                    .iter()
                    .any(|m| word_to_string(m).to_lowercase() == method_lc)
                {
                    if let Some(m_refl) =
                        codebase.get_method(trait_fqcn_lc.as_bytes(), method_lc.as_bytes())
                    {
                        return Some((m_refl, word_to_string(&t_refl.name)));
                    }
                }
            }
        }

        // Not found here — try the direct parent.
        let Some(parent_name) = &class_refl.direct_parent_class else {
            return None;
        };
        let parent_fqcn = word_to_string(parent_name)
            .trim_start_matches('\\')
            .to_lowercase();
        if parent_fqcn == current {
            return None; // self-loop guard
        }
        current = parent_fqcn;
    }

    None
}

/// Seed `env` with parameter types from the method's reflection.
///
/// For each parameter that has a declared type, we resolve it and bind
/// `$paramName → Type` in the env so the walker can track it.
fn seed_param_types(
    env: &mut TypeEnv,
    project: &MagoProject,
    method_refl: &FunctionLikeMetadata,
) {
    for param in &method_refl.parameters {
        let name_str = word_to_string(&param.name.0);
        // param.name already includes the leading `$` (same as properties).
        let var = if name_str.starts_with('$') {
            name_str
        } else {
            format!("${name_str}")
        };
        let ty = if let Some(type_meta) = &param.type_metadata {
            crate::types::walker::type_union_to_type_pub(project, &type_meta.type_union, env)
        } else {
            crate::types::Type::Mixed
        };
        env.set(var, ty);
    }
}

/// Walk the body of the named method within the given program's AST.
fn walk_method_body(ctx: &mut WalkerCtx, program: &mago_syntax::ast::Program, class: &str, method: &str) {
    walk_method_body_impl(ctx, program.statements.iter(), class, method);
}

/// Recursively walk statements looking for the target class + method,
/// descending into namespace wrappers as needed.
fn walk_method_body_impl<'s, 'arena, I>(ctx: &mut WalkerCtx, stmts: I, class: &str, method: &str)
where
    'arena: 's,
    I: Iterator<Item = &'s Statement<'arena>>,
{
    // Strip any leading namespace from class name for matching purposes.
    // The AST `Class.name` is a LocalIdentifier, so it only contains the
    // unqualified name (e.g. "ServiceTest", not "App\\Tests\\ServiceTest").
    let simple_class = simple_name(class);
    let target_method_lc = method.to_lowercase();

    for stmt in stmts {
        match stmt {
            Statement::Class(c) => {
                if name_eq_ignore_case(c.name.value, simple_class) {
                    for member in c.members.iter() {
                        if let ClassLikeMember::Method(m) = member {
                            if name_to_lower(m.name.value) == target_method_lc {
                                if let MethodBody::Concrete(block) = &m.body {
                                    walk_block(ctx, block);
                                    return;
                                }
                            }
                        }
                    }
                    return;
                }
            }
            Statement::Trait(t) => {
                if name_eq_ignore_case(t.name.value, simple_class) {
                    for member in t.members.iter() {
                        if let ClassLikeMember::Method(m) = member {
                            if name_to_lower(m.name.value) == target_method_lc {
                                if let MethodBody::Concrete(block) = &m.body {
                                    walk_block(ctx, block);
                                    return;
                                }
                            }
                        }
                    }
                    return;
                }
            }
            Statement::Namespace(ns) => {
                use mago_syntax::ast::namespace::NamespaceBody;
                match &ns.body {
                    NamespaceBody::Implicit(b) => {
                        walk_method_body_impl(ctx, b.statements.iter(), class, method)
                    }
                    NamespaceBody::BraceDelimited(b) => {
                        walk_method_body_impl(ctx, b.statements.iter(), class, method)
                    }
                };
                if !ctx.events.is_empty() {
                    // Found and walked the method, return early.
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Strip a PHP namespace prefix: `My\\Ns\\ClassName` → `ClassName`.
fn simple_name(fqcn: &str) -> &str {
    fqcn.rsplit('\\').next().unwrap_or(fqcn)
}

/// Case-insensitive compare an AST identifier's raw bytes against a `&str`.
fn name_eq_ignore_case(bytes: &[u8], s: &str) -> bool {
    String::from_utf8_lossy(bytes).eq_ignore_ascii_case(s)
}

/// Lowercase an AST identifier's raw bytes into an owned `String`.
fn name_to_lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_lowercase()
}

/// Resolve a loaded `File` to its on-disk path (falls back to logical name).
fn file_path_of(file: &mago_database::file::File) -> PathBuf {
    match &file.path {
        Some(p) => p.clone(),
        None => PathBuf::from(String::from_utf8_lossy(&file.name).into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::BoundaryResolver;
    use crate::cache::CacheStore;
    use crate::config::ProjectConfig;
    use crate::test_discovery::discover;

    fn make_boundary_for(dir: &std::path::Path) -> BoundaryResolver {
        let cfg = ProjectConfig {
            root: dir.to_path_buf(),
            test_suites: vec![dir.to_path_buf()],
            source_includes: vec![dir.to_path_buf()],
            source_excludes: vec![dir.join("vendor")],
        };
        BoundaryResolver::from_config(&cfg)
    }

    fn make_boundary_src(dir: &std::path::Path) -> BoundaryResolver {
        let cfg = ProjectConfig {
            root: dir.to_path_buf(),
            test_suites: vec![dir.to_path_buf()],
            source_includes: vec![dir.join("src")],
            source_excludes: vec![dir.join("vendor")],
        };
        BoundaryResolver::from_config(&cfg)
    }

    #[test]
    fn traces_a_simple_test_method() {
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
    public function testFoo(): void {
        $x = 1;
        $y = 2;
    }
}
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let cache = CacheStore::open(dir.path(), MagoProject::version()).unwrap();
        let boundary = make_boundary_for(dir.path());
        let tests = discover(&project, &cache, &[test_file]).unwrap();
        assert_eq!(tests.len(), 1, "expected 1 test method");

        let coverage = trace_test(&project, &boundary, &tests[0], None);
        // At least some line in the method body should be marked.
        assert!(
            !coverage.is_empty(),
            "coverage should not be empty; got: {coverage:?}"
        );

        // Find the entry for our test file. Verify at least one line has the test ID.
        let lines = coverage.values().next().unwrap();
        let has_testfoo = lines
            .values()
            .any(|ids| ids.iter().any(|i| i.method == "testFoo"));
        assert!(has_testfoo, "expected testFoo to appear in coverage");
    }

    #[test]
    fn traces_into_production_callee() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        )
        .unwrap();
        // Production class in src/ (not vendor).
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let prod_file = src_dir.join("Money.php");
        std::fs::write(
            &prod_file,
            r#"<?php
class Money {
    public function amount(): int {
        return 42;
    }
}
"#,
        )
        .unwrap();
        let test_file = dir.path().join("MoneyTest.php");
        std::fs::write(
            &test_file,
            r#"<?php
use PHPUnit\Framework\TestCase;
class MoneyTest extends TestCase {
    public function testAmount(): void {
        $m = new Money();
        $m->amount();
    }
}
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let cache = CacheStore::open(dir.path(), MagoProject::version()).unwrap();
        // src/ is traced (not vendor).
        let boundary = make_boundary_src(dir.path());
        let tests = discover(&project, &cache, &[test_file]).unwrap();
        assert_eq!(tests.len(), 1);

        let coverage = trace_test(&project, &boundary, &tests[0], None);
        // The production file should appear in coverage because we recurse into Money::amount.
        let prod_covered = coverage.contains_key(&prod_file);
        assert!(
            prod_covered,
            "expected production file to be covered; keys: {:?}",
            coverage.keys().collect::<Vec<_>>()
        );
    }
}
