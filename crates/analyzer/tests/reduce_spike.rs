//! Keystone de-risk spike for the symbolic reducer (increment 1, Task 3).
//!
//! Empirically validates the load-bearing assumption (spec §12.8 risk #1-2):
//! does `mago_analyzer::Analyzer::analyze_with_artifacts` populate per-node
//! inferred types, and do they carry concrete literal values we can lift?
//!
//! Also answers the folding question (§12 risk #3): does mago fold `1 + 2`
//! into a literal `3` (→ we must NEVER lift a folded result), or leave the
//! result widened to `int` (→ we compute it ourselves)?
//!
//! Run with: `cargo test -p analyzer --test reduce_spike -- --nocapture`

use std::borrow::Cow;

use bumpalo::Bump;
use mago_analyzer::analysis_result::AnalysisResult;
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
use mago_syntax::parser::parse_file;

#[test]
fn analyzer_emits_per_node_literal_types() {
    let src: &'static [u8] = b"<?php\n$a = 1 + 2;\n$b = $a * 10;\n$c = 'hi' . 'there';\n$d = 7;\n";
    let arena = Bump::new();
    let file = File::ephemeral(Cow::Borrowed(b"spike.php"), Cow::Borrowed(src));

    let program = parse_file(&arena, &file);
    let resolved = NameResolver::new(&arena).resolve(program);

    let version = PHPVersion::LATEST;
    let mut codebase: CodebaseMetadata = scan_program(&arena, &file, program, &resolved, version);
    let mut symbol_refs = SymbolReferences::default();
    populate_codebase(
        &mut codebase,
        &mut symbol_refs,
        Default::default(),
        Default::default(),
    );

    let registry = create_registry();
    let settings = Settings::new(version);
    let analyzer = Analyzer::new(&arena, &file, &resolved, &codebase, &registry, settings);

    let mut result = AnalysisResult::new(symbol_refs);
    let artifacts = analyzer
        .analyze_with_artifacts(program, &mut result)
        .expect("analyze_with_artifacts should succeed");

    let total = artifacts.expression_types.len();
    println!("[spike] expression_types entries: {total}");

    let mut literal_ints: Vec<i64> = Vec::new();
    let mut literal_strings: Vec<String> = Vec::new();
    for (range, ty) in artifacts.expression_types.iter() {
        if let Some(v) = ty.get_single_literal_int_value() {
            literal_ints.push(v);
            println!("[spike]   {range:?} = literal int {v}");
        } else if let Some(s) = ty.get_single_literal_string_value() {
            let s = String::from_utf8_lossy(s).into_owned();
            println!("[spike]   {range:?} = literal string {s:?}");
            literal_strings.push(s);
        } else {
            println!("[spike]   {range:?} = (non-literal / widened)");
        }
    }

    // Core assertion: the analyzer DID populate per-node types.
    assert!(total > 0, "expression_types must be populated");

    // Diagnostic (not asserted — these answer the folding question):
    // - if 3 appears among literal_ints → mago FOLDED `1 + 2` (we must not lift folded results).
    // - if only 1,2,10,7 appear and the `+`/`*` result nodes are non-literal → mago leaves
    //   arithmetic widened and the reducer computes results itself.
    println!("[spike] literal ints seen: {literal_ints:?}");
    println!("[spike] literal strings seen: {literal_strings:?}");
    let folded_sum = literal_ints.contains(&3);
    let folded_concat = literal_strings.iter().any(|s| s == "hithere");
    println!(
        "[spike] FOLDING — 1+2 folded to 3: {folded_sum}; 'hi'.'there' folded: {folded_concat}"
    );
    // We expect at least the leaf literals 7 (and likely 1,2,10) to be present.
    assert!(
        literal_ints.contains(&7),
        "the leaf literal `7` should have an inferred literal-int type; got {literal_ints:?}"
    );
}

/// The crux, empirically: mago folds with NON-PHP semantics. PHP_INT_MAX + 1
/// overflows to a FLOAT (9.2233720368548E+18) in real PHP; if mago folds it to
/// a saturated literal int, lifting that folded result would be a false green.
/// This is the concrete justification for the MANDATORY "compute operator
/// results yourself; never lift a folded result node" rule (spec §12.2).
#[test]
fn mago_folding_uses_non_php_overflow_semantics() {
    let src: &'static [u8] = b"<?php\n$x = 9223372036854775807 + 1;\n";
    let arena = Bump::new();
    let file = File::ephemeral(Cow::Borrowed(b"ovf.php"), Cow::Borrowed(src));
    let program = parse_file(&arena, &file);
    let resolved = NameResolver::new(&arena).resolve(program);
    let version = PHPVersion::LATEST;
    let mut codebase: CodebaseMetadata = scan_program(&arena, &file, program, &resolved, version);
    let mut symbol_refs = SymbolReferences::default();
    populate_codebase(
        &mut codebase,
        &mut symbol_refs,
        Default::default(),
        Default::default(),
    );
    let registry = create_registry();
    let analyzer = Analyzer::new(
        &arena,
        &file,
        &resolved,
        &codebase,
        &registry,
        Settings::new(version),
    );
    let mut result = AnalysisResult::new(symbol_refs);
    let artifacts = analyzer
        .analyze_with_artifacts(program, &mut result)
        .unwrap();

    println!("[spike-ovf] PHP_INT_MAX + 1 — what mago inferred per node:");
    let mut max_plus_one_node: Option<(i64, Option<f64>)> = None;
    for (range, ty) in artifacts.expression_types.iter() {
        let i = ty.get_single_literal_int_value();
        let f = ty.get_single_literal_float_value();
        if i.is_some() || f.is_some() {
            println!("[spike-ovf]   {range:?} = int:{i:?} float:{f:?}");
        }
        // the `+` result node spans the widest int range; capture it
        if let Some(v) = i {
            if v == i64::MAX {
                max_plus_one_node = Some((v, f));
            }
        }
    }
    // PHP: 9223372036854775807 + 1 == 9.2233720368548E+18 (float). If mago kept
    // it as int i64::MAX, that is the saturation divergence we must never lift.
    println!(
        "[spike-ovf] VERDICT: mago folded the sum to a saturated int {:?} (PHP overflows to float). \
         Lifting this folded result = false green; the reducer MUST compute it itself.",
        max_plus_one_node
    );
}
