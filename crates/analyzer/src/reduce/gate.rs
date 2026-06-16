//! The reducibility gate: a PURITY / COMPLETENESS decision over mago's per-node
//! `AnalysisArtifacts`.
//!
//! # What the gate is (and is NOT)
//!
//! A test, given its complete Givens (data-provider row + fixtures), is a trivial
//! deterministic computation. "Reduce" = perform that computation NATIVELY in Rust
//! (in [`super::eval`]) instead of starting the PHP VM. The gate's job is the
//! **reducibility decision**: *are this test's Givens complete — is it pure, with
//! every operand concrete and every called function modelled?* If yes, the native
//! evaluator runs it; if anything is a hidden input (time / DB / random / network /
//! global state / a `mixed`-typed node / an unmodelled call), the gate says **not
//! reducible** and the caller bails (fail-closed — spec §5).
//!
//! mago is the **accelerator** for that decision (and, in [`super::eval`], for
//! resolving user-function calls). It is NOT the source of computed values: the
//! native evaluator computes the values. So the gate reads mago's *types* to
//! classify reducibility; it does not lift mago's folded *results* as answers.
//!
//! # The literal reader
//!
//! [`concrete_literal`] reads the literal a LEAF node is proven to be (`5`,
//! `'hi'`, `true`, `null`) — an accelerator for a statically-known operand. It
//! gates ONLY on the **value-returning getters** (`get_single_literal_int_value`
//! etc.), never the `is_literal_*` booleans (which are true for valueless ranges /
//! `Unspecified` / `UnspecifiedLiteral`). The evaluator uses it only for operands
//! it cannot already see syntactically; it computes every operator RESULT itself
//! in PHP semantics, never lifting a folded result node's literal (spec §12.2).

use mago_analyzer::artifacts::AnalysisArtifacts;
use mago_span::HasSpan;

use super::value::Value;

/// Read the concrete literal a LEAF node is proven to be, or `None`.
///
/// Fail-closed `None` when: no type entry (`get_expression_type → None`), not a
/// single atomic, `mixed`, or the single atomic is not a value-bearing literal
/// scalar (int/float/string/bool) nor `null` (e.g. a range, `Unspecified`,
/// `UnspecifiedLiteral`, an object, an array, a callable).
///
/// Used by the evaluator as an accelerator for statically-known operands; the
/// evaluator computes operator results itself and never lifts a folded result.
pub fn concrete_literal<T: HasSpan>(node: &T, artifacts: &AnalysisArtifacts) -> Option<Value> {
    let ty = artifacts.get_expression_type(node)?;

    if ty.is_mixed() || !ty.is_single() {
        return None;
    }

    // Value-returning getters ONLY (never is_literal_* booleans). A single atomic
    // yields at most one of these, so first-Some is sound.
    if let Some(i) = ty.get_single_literal_int_value() {
        return Some(Value::Int(i));
    }
    if let Some(f) = ty.get_single_literal_float_value() {
        return Some(Value::Float(f));
    }
    if let Some(bytes) = ty.get_single_literal_string_value() {
        return Some(Value::Str(bytes.to_vec()));
    }
    if let Some(b) = ty.get_single_bool() {
        // Only a narrowed true/false carries a value; an unspecified bool bails.
        if b.is_true() {
            return Some(Value::Bool(true));
        }
        if b.is_false() {
            return Some(Value::Bool(false));
        }
        return None;
    }
    if ty.is_null() {
        return Some(Value::Null);
    }

    None
}

/// The reducibility decision for a single node: does mago prove it is a single,
/// concrete (non-`mixed`, value-bearing) type?
///
/// `false` (→ caller bails) when the node has no type entry, is `mixed`, is not a
/// single atomic, or its single atomic carries no value (a range / `Unspecified` /
/// object / array-with-unknowns / callable / …). This is the per-node half of
/// "are the Givens complete?": a node mago could not prove concrete is a hidden
/// input from the reducer's point of view.
///
/// It is exactly `concrete_literal(...).is_some()` today; kept as a named
/// predicate because the *intent* (a purity/completeness decision) is distinct
/// from reading a value, and later increments may widen "concrete" to sealed
/// arrays / enum cases without those being liftable to a scalar [`Value`].
pub fn node_is_concrete<T: HasSpan>(node: &T, artifacts: &AnalysisArtifacts) -> bool {
    concrete_literal(node, artifacts).is_some()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use bumpalo::Bump;
    use mago_analyzer::analysis_result::AnalysisResult;
    use mago_analyzer::artifacts::AnalysisArtifacts;
    use mago_analyzer::plugin::create_registry;
    use mago_analyzer::settings::Settings;
    use mago_analyzer::Analyzer;
    use mago_codex::metadata::CodebaseMetadata;
    use mago_codex::populator::populate_codebase;
    use mago_codex::reference::SymbolReferences;
    use mago_codex::scanner::scan_program;
    use mago_database::file::File;
    use mago_names::resolver::NameResolver;
    use mago_php_version::PHPVersion;
    use mago_syntax::ast::ast::expression::Expression;
    use mago_syntax::ast::ast::statement::Statement;
    use mago_syntax::parser::parse_file;

    use super::*;
    use crate::reduce::value::Value;

    /// Build artifacts for a snippet, then run `f` over the parsed program + the
    /// artifacts. The AST is arena-bound, so all `concrete_literal`/`node_is_*`
    /// calls (which take AST nodes) happen inside `f`.
    fn with_artifacts<R>(
        src: &str,
        f: impl for<'a> FnOnce(&'a mago_syntax::ast::Program<'a>, &AnalysisArtifacts) -> R,
    ) -> R {
        let full = format!("<?php\n{}", src);
        let arena = Bump::new();
        let file = File::ephemeral(Cow::Borrowed(b"gate.php"), Cow::Owned(full.into_bytes()));
        let program = parse_file(&arena, &file);
        let resolved = NameResolver::new(&arena).resolve(program);
        let version = PHPVersion::LATEST;
        let mut codebase: CodebaseMetadata =
            scan_program(&arena, &file, program, &resolved, version);
        let mut symbol_refs = SymbolReferences::default();
        populate_codebase(
            &mut codebase,
            &mut symbol_refs,
            Default::default(),
            Default::default(),
        );
        let registry = create_registry();
        let mut settings = Settings::new(version);
        settings.find_unused_expressions = false;
        settings.find_unused_definitions = false;
        let analyzer = Analyzer::new(&arena, &file, &resolved, &codebase, &registry, settings);
        let mut result = AnalysisResult::new(symbol_refs);
        let artifacts = analyzer
            .analyze_with_artifacts(program, &mut result)
            .expect("analyze_with_artifacts should succeed");
        f(program, &artifacts)
    }

    /// Return the RHS expression of the first `$x = <expr>;` assignment statement.
    fn first_assignment_rhs<'a>(program: &'a mago_syntax::ast::Program<'a>) -> &'a Expression<'a> {
        use mago_syntax::ast::ast::expression::Expression as E;
        for stmt in program.statements.iter() {
            if let Statement::Expression(es) = stmt {
                if let E::Assignment(a) = es.expression {
                    return a.rhs;
                }
            }
        }
        panic!("no assignment found");
    }

    #[test]
    fn reads_literal_int_off_leaf() {
        with_artifacts("$x = 5;", |program, artifacts| {
            let rhs = first_assignment_rhs(program);
            assert!(matches!(
                concrete_literal(rhs, artifacts),
                Some(Value::Int(5))
            ));
            assert!(node_is_concrete(rhs, artifacts));
        });
    }

    #[test]
    fn reads_literal_string_off_leaf() {
        with_artifacts("$x = 'hello';", |program, artifacts| {
            let rhs = first_assignment_rhs(program);
            match concrete_literal(rhs, artifacts) {
                Some(Value::Str(b)) => assert_eq!(b, b"hello"),
                other => panic!("expected Str(hello), got {other:?}"),
            }
        });
    }

    #[test]
    fn reads_literal_bool_off_leaf() {
        with_artifacts("$x = true;", |program, artifacts| {
            let rhs = first_assignment_rhs(program);
            assert!(matches!(
                concrete_literal(rhs, artifacts),
                Some(Value::Bool(true))
            ));
        });
    }

    #[test]
    fn reads_literal_float_off_leaf() {
        with_artifacts("$x = 1.5;", |program, artifacts| {
            let rhs = first_assignment_rhs(program);
            match concrete_literal(rhs, artifacts) {
                Some(Value::Float(f)) => assert!((f - 1.5).abs() < 1e-12),
                other => panic!("expected Float(1.5), got {other:?}"),
            }
        });
    }

    #[test]
    fn reads_null_literal() {
        with_artifacts("$x = null;", |program, artifacts| {
            let rhs = first_assignment_rhs(program);
            assert!(matches!(
                concrete_literal(rhs, artifacts),
                Some(Value::Null)
            ));
        });
    }

    #[test]
    fn not_reducible_when_result_is_widened() {
        // strlen of an unknown is a non-literal int (range/general) → not concrete.
        with_artifacts("$x = strlen($GLOBALS['z']);", |program, artifacts| {
            let rhs = first_assignment_rhs(program);
            assert!(concrete_literal(rhs, artifacts).is_none());
            assert!(
                !node_is_concrete(rhs, artifacts),
                "a widened-int result is a hidden input → not reducible"
            );
        });
    }

    #[test]
    fn not_reducible_on_mixed() {
        // A superglobal element is `mixed` → a hidden input → not reducible.
        with_artifacts("$x = $GLOBALS['z'];", |program, artifacts| {
            let rhs = first_assignment_rhs(program);
            assert!(
                !node_is_concrete(rhs, artifacts),
                "mixed must not be reducible"
            );
        });
    }
}
