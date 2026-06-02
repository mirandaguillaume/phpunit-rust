//! Per-test execution trace (Phase 2).
//!
//! Walks each test method body via the type tracker, emits CallSiteEvents,
//! resolves them through dispatch + opacity, and recurses into traced
//! callees, marking lines along the way.

use super::{Coverage, TestId};
use crate::boundary::BoundaryResolver;
use crate::mago_bridge::MagoProject;
use crate::opacity::{self, Opacity, ReceiverType};
use crate::test_discovery::TestMethod;
use crate::types::env::TypeEnv;
use crate::types::walker::{walk_block, CallSiteEvent, WalkerCtx};
use mago_reflection::function_like::FunctionLikeReflection;
use mago_reflection::identifier::FunctionLikeName;
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
                if let Some(src) = project.source_by_id(m_refl.span.start.source) {
                    call_site.callee_file = Some(PathBuf::from(
                        project.interner().lookup(&src.identifier.0).to_string(),
                    ));
                }
            }
        }
        // Partial-mock transparency: a mock of a known concrete/abstract class
        // (not an interface or trait) should trace into real method implementations,
        // since only selected methods are stubbed while others call real code.
        if call_site.receiver_type == ReceiverType::Mock {
            if let Some(cc) = call_site.callee_class.clone() {
                let is_traceable_class = project
                    .find_class_reflection(&cc)
                    .map(|r| !r.is_interface() && !r.is_trait())
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
    let interner = project.interner();
    let target_method_lc = method.to_lowercase();

    // 1. Walk the inheritance chain to find which class defines the method.
    //    `defining_class` is where the method body lives (may differ from `class`
    //    when the method is inherited). `TypeEnv` still uses the original `class`
    //    so `$this` resolves to the subclass — giving virtual dispatch for free.
    let (method_refl, defining_class) =
        find_method_in_hierarchy(project, class, &target_method_lc)?;

    // 2. Resolve span → file path + line range.
    let span = method_refl.span;
    let source_id = span.start.source;
    let source = project.source_by_id(source_id)?;
    let file_path = PathBuf::from(interner.lookup(&source.identifier.0).to_string());
    let start_line = source.line_number(span.start.offset) as u32 + 1;
    let end_line = source.line_number(span.end.offset) as u32 + 1;

    // 3. Parse the source — result is cached by MagoProject so each unique file is
    //    parsed at most once regardless of how many tests trace into it.
    let program = project.get_or_parse(source);

    // Phase 2.5: compute resolved names for the program so the walker can
    // look up FQCNs at class-name sites.
    let names = mago_names::resolver::NameResolver::new(interner).resolve(&program);

    // 4. Seed TypeEnv with the ORIGINAL (call-site) class so that `$this` inside
    //    an inherited method body correctly dispatches back to the subclass.
    let mut env = TypeEnv::for_class(class);
    seed_param_types(&mut env, project, method_refl);

    // 5. Walk the method body. Use `defining_class` for AST navigation (the
    //    source file contains `class JsonFormatter`, not `class Subclass`).
    let mut ctx = WalkerCtx::new(env, interner, project, names);
    walk_method_body(&mut ctx, &program, &defining_class, method);
    let events = ctx.events;

    Some((file_path, start_line, end_line, events))
}

/// Walk up the inheritance chain to find the closest ancestor (including `start_class`
/// itself) that defines `method_lc`. Returns the reflection and the FQCN of the
/// class that defines it.
fn find_method_in_hierarchy<'a>(
    project: &'a MagoProject,
    start_class: &str,
    method_lc: &str,
) -> Option<(&'a FunctionLikeReflection, String)> {
    let interner = project.interner();
    let mut current = start_class.to_lowercase();

    for _ in 0..MAX_RECURSION_DEPTH {
        // Find the reflection for `current`.
        let (class_fqcn, class_refl) = project.find_class(&current)?;

        // Look for the method directly on this class.
        for (_id, m_refl) in class_refl.methods.members.iter() {
            let mname = match &m_refl.name {
                FunctionLikeName::Method(_, n) => interner.lookup(&n.value).to_string(),
                _ => continue,
            };
            if mname.to_lowercase() == method_lc {
                return Some((m_refl, class_fqcn));
            }
        }

        // Not in own methods — search traits used by this class.
        // mago-reflection does NOT inline trait methods into methods.members,
        // so we must look up each trait's reflection explicitly.
        for trait_name in &class_refl.used_traits {
            let trait_fqcn_lc = interner
                .lookup(&trait_name.value)
                .trim_start_matches('\\')
                .to_lowercase();
            if let Some((t_fqcn, t_refl)) = project.find_class(&trait_fqcn_lc) {
                for (_id, m_refl) in t_refl.methods.members.iter() {
                    let mname = match &m_refl.name {
                        FunctionLikeName::Method(_, n) => interner.lookup(&n.value).to_string(),
                        _ => continue,
                    };
                    if mname.to_lowercase() == method_lc {
                        return Some((m_refl, t_fqcn));
                    }
                }
            }
        }

        // Not found here — try the direct parent.
        let Some(parent_name) = &class_refl.inheritance.direct_extended_class else {
            return None;
        };
        let parent_fqcn = interner
            .lookup(&parent_name.value)
            .to_string()
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
    method_refl: &FunctionLikeReflection,
) {
    let interner = project.interner();
    for param in &method_refl.parameters {
        let name_str = interner.lookup(&param.name).to_string();
        // param.name already includes the leading `$` (same as properties).
        let var = if name_str.starts_with('$') {
            name_str
        } else {
            format!("${name_str}")
        };
        let ty = if let Some(type_refl) = &param.type_reflection {
            crate::types::walker::type_kind_to_type_pub(project, interner, &type_refl.kind, env)
        } else {
            crate::types::Type::Mixed
        };
        env.set(var, ty);
    }
}

/// Walk the body of the named method within the given program's AST.
fn walk_method_body(
    ctx: &mut WalkerCtx,
    program: &mago_syntax::ast::Program,
    class: &str,
    method: &str,
) {
    walk_method_body_impl(ctx, program.statements.iter(), class, method);
}

/// Recursively walk statements looking for the target class + method,
/// descending into namespace wrappers as needed.
fn walk_method_body_impl<'s, I>(ctx: &mut WalkerCtx, stmts: I, class: &str, method: &str)
where
    I: Iterator<Item = &'s Statement>,
{
    // Strip any leading namespace from class name for matching purposes.
    // The AST `Class.name` is a LocalIdentifier, so it only contains the
    // unqualified name (e.g. "ServiceTest", not "App\\Tests\\ServiceTest").
    let simple_class = simple_name(class);
    let target_method_lc = method.to_lowercase();

    for stmt in stmts {
        match stmt {
            Statement::Class(c) => {
                let ast_name = ctx.interner.lookup(&c.name.value);
                if ast_name.eq_ignore_ascii_case(simple_class) {
                    for member in c.members.iter() {
                        if let ClassLikeMember::Method(m) = member {
                            if ctx.interner.lookup(&m.name.value).to_lowercase() == target_method_lc
                            {
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
                let ast_name = ctx.interner.lookup(&t.name.value);
                if ast_name.eq_ignore_ascii_case(simple_class) {
                    for member in t.members.iter() {
                        if let ClassLikeMember::Method(m) = member {
                            if ctx.interner.lookup(&m.name.value).to_lowercase() == target_method_lc
                            {
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
                // NamespaceBody has both Implicit and BraceDelimited variants;
                // both expose `.statements` via a method.
                use mago_syntax::ast::namespace::NamespaceBody;
                let inner_stmts = match &ns.body {
                    NamespaceBody::Implicit(b) => b.statements.iter(),
                    NamespaceBody::BraceDelimited(b) => b.statements.iter(),
                };
                walk_method_body_impl(ctx, inner_stmts, class, method);
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
