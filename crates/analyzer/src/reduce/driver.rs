//! `reduce_file`: codebase build + per-test reduction + provider rows.
//!
//! This is the top of the reducer (spec §12.7 / §12.4). It:
//! 1. Builds the codebase for the test file's directory (so same-suite helpers,
//!    parent classes, and app code the test touches are visible to substitution).
//! 2. Discovers the test methods (reusing `test_discovery`) and expands their
//!    provider rows: `#[DataProvider]` via the existing `data_provider` path
//!    (rows fed STRAIGHT into the native evaluator — NOT round-tripped through
//!    the analyzer, spec §12.4), and `#[TestWith]` read off the raw method AST.
//! 3. Reduces each (method × row) by binding the row to the method's parameter
//!    names (the Givens) and running the body natively with the substitution
//!    resolver — producing one [`ReducedTest`] per row.
//!
//! Fail-closed throughout: a shared fixture (`setUp` / constructor) the reducer
//! cannot model bails the affected tests; any unmodelled construct/value bails
//! that row (spec §5).

use std::collections::HashMap;
use std::path::Path;

use mago_syntax::ast::ast::block::Block;
use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
use mago_syntax::ast::ast::class_like::method::{Method, MethodBody};
use mago_syntax::ast::ast::statement::Statement;
use mago_syntax::ast::Program;

use super::eval::{run_method_body_with_names, BailReason, Outcome};
use super::subst::BridgeResolver;
use super::value::Value;
use crate::analyzer::data_provider::expand;
use crate::cache::CacheStore;
use crate::mago_bridge::MagoProject;
use crate::test_discovery::{discover, TestMethod};

/// One reduced test invocation (one provider row, or the single no-provider run).
#[derive(Debug, Clone)]
pub struct ReducedTest {
    pub class: String,
    pub method: String,
    /// `Some(name)` for a provider/TestWith row; `None` for a no-provider test.
    pub data_set: Option<String>,
    pub outcome: Outcome,
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("bridge: {0}")]
    Bridge(#[from] crate::mago_bridge::BridgeError),
    #[error("cache: {0}")]
    Cache(String),
    #[error("discovery: {0}")]
    Discovery(String),
}

/// Reduce all tests in `test_file`, building the codebase from its directory.
///
/// Returns one [`ReducedTest`] per provider row (or one per no-provider test).
/// Any row the reducer cannot model has `outcome = Outcome::Bailed(reason)` — it
/// is the caller's job to run those for real (fail-closed).
pub fn reduce_file(test_file: &Path) -> Result<Vec<ReducedTest>, DriverError> {
    let root = project_root_of(test_file);
    reduce_in_root(&root, test_file)
}

/// Resolve the scan root for `test_file` by walking UP to the nearest ancestor
/// directory that contains a `composer.json` (the canonical PHP project-root
/// marker). This widens the scan to include the project's own `src/` so a
/// `new <ProjectClass>` resolves (the narrow parent-of-the-test root omitted
/// `src/`, forcing a scoping-artifact bail — see Task 1). Vendor stays excluded
/// downstream via [`MagoProject::load_excluding_vendor`], so the wider root costs
/// only the project's own (small) source tree, never the 36x vendor scan.
///
/// Fallback (no composer.json found anywhere up the tree): the test file's own
/// parent dir — the prior behavior, fully divergence-safe (a missing class still
/// bails).
pub fn project_root_of(test_file: &Path) -> std::path::PathBuf {
    let start = test_file.parent().unwrap_or(Path::new("."));
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join("composer.json").is_file() {
            return dir.to_path_buf();
        }
        cur = dir.parent();
    }
    start.to_path_buf()
}

/// Like [`reduce_file`] but with an explicit codebase root (the suite dir whose
/// dependency closure should be scanned). Useful when the test file's parent dir
/// is not the right scan root.
pub fn reduce_in_root(root: &Path, test_file: &Path) -> Result<Vec<ReducedTest>, DriverError> {
    // Scope the codebase scan to the project, skipping vendor. The scan dominates
    // the reducer's wall-time, and vendor is ~98% of the classes (measured on
    // doctrine/collections: 2230 of 2269) yet a test's traversal is almost always
    // its own src + test-case ancestry. Excluding vendor is a ~36x scan speedup
    // (1.5s → 42ms there) and is FAIL-CLOSED: a test that genuinely traverses a
    // vendored class can't resolve its body → it bails, never reduces wrong.
    // TODO: scope to the exact transitive dependency closure for the last mile.
    let project = MagoProject::load_excluding_vendor(root)?;
    let cache = CacheStore::open(root, MagoProject::version())
        .map_err(|e| DriverError::Cache(e.to_string()))?;
    let tests = discover(&project, &cache, &[test_file.to_path_buf()])
        .map_err(|e| DriverError::Discovery(e.to_string()))?;

    let resolver = BridgeResolver::new(&project);
    let mut out = Vec::new();
    for test in &tests {
        out.extend(reduce_one_test(&project, &resolver, test));
    }
    Ok(out)
}

/// Reduce a single discovered test across all its rows.
fn reduce_one_test(
    project: &MagoProject,
    resolver: &BridgeResolver,
    test: &TestMethod,
) -> Vec<ReducedTest> {
    // Resolve the BODY class's declaring file (Inc-4 C: an inherited test method's
    // body lives in its declaring `*TestCase`, not the concrete subclass — but the
    // concrete subclass is what `$this` is bound to below).
    let body_class = test.body_class();
    let Some(class_meta) = project.find_class(body_class) else {
        return vec![bailed(test, None, "test class not in codebase")];
    };
    let Some(file) = project.file_of_span(&class_meta.span) else {
        return vec![bailed(test, None, "declaring file not loaded")];
    };
    let logical = String::from_utf8_lossy(&file.name).into_owned();

    // Rows: #[DataProvider] (via the existing expand path) OR #[TestWith] read
    // from the AST OR a single no-provider invocation.
    let rows = collect_rows(project, &logical, test);

    let reduced = project.with_program(&logical, |program, file, names| {
        let source = file.contents.as_ref();
        let Some(method) = find_method(program, body_class, &test.method) else {
            return vec![bailed(test, None, "method body not found")];
        };

        // Class-level fixtures are deferred (Inc-3 Task B): setUpBeforeClass seeds
        // STATIC state we don't model → bail the whole method.
        if test.lifecycle.set_up_before_class {
            return rows
                .iter()
                .map(|(ds, _)| {
                    bailed(
                        test,
                        ds.clone(),
                        "setUpBeforeClass (class fixture) not modelled",
                    )
                })
                .collect();
        }

        // Inc-3 Tasks A+B: bind `$this` to a Value::Object modelling the TEST-CASE
        // instance, seeded by setUp() (walked up the parent chain via the resolver).
        // A setUp that hits an unmodelled/impure construct bails the whole method
        // (its Givens are incomplete) — fail-closed.
        let this_value = match resolver.build_test_case_this(&test.class) {
            Ok(Some(v)) => Some(v),
            // Class not in the codebase → run with no `$this` (a `$this->...` read
            // then bails). Preserves the inc-2 behaviour for non-TestCase classes.
            Ok(None) => None,
            Err(reason) => {
                return rows
                    .iter()
                    .map(|(ds, _)| ReducedTest {
                        class: test.class.clone(),
                        method: test.method.clone(),
                        data_set: ds.clone(),
                        outcome: Outcome::Bailed(reason.clone()),
                    })
                    .collect()
            }
        };

        let MethodBody::Concrete(block) = &method.body else {
            return vec![bailed(test, None, "abstract/interface method")];
        };
        let param_names = match method_param_names(method) {
            Ok(names) => names,
            Err(reason) => {
                return rows
                    .iter()
                    .map(|(ds, _)| ReducedTest {
                        class: test.class.clone(),
                        method: test.method.clone(),
                        data_set: ds.clone(),
                        outcome: Outcome::Bailed(reason.clone()),
                    })
                    .collect()
            }
        };

        rows.iter()
            .map(|(data_set, args)| {
                let outcome = reduce_row(
                    block,
                    &param_names,
                    args,
                    this_value.clone(),
                    resolver,
                    names,
                    source,
                    body_class,
                );
                ReducedTest {
                    class: test.class.clone(),
                    method: test.method.clone(),
                    data_set: data_set.clone(),
                    outcome,
                }
            })
            .collect()
    });

    reduced.unwrap_or_else(|| vec![bailed(test, None, "could not re-parse test file")])
}

/// Gather the rows for a test: provider rows (fed straight into the evaluator),
/// `#[TestWith]` rows from the AST, or one empty no-provider row.
fn collect_rows(
    project: &MagoProject,
    logical: &str,
    test: &TestMethod,
) -> Vec<(Option<String>, Vec<Value>)> {
    // #[DataProvider]: reuse the existing expansion (concrete::compute on the
    // provider's `return [...]`), then convert PhpValue → byte-backed Value.
    if test.has_data_provider.is_some() {
        let expanded = expand(project, test);
        // `expand` falls back to a single no-arg invocation if the provider is not
        // concretely computable; detect that (its data_set is None) and treat as
        // "provider not reducible" so the row bails rather than runs arg-less.
        if expanded.len() == 1 && expanded[0].data_set.is_none() {
            return vec![(None, vec![])];
        }
        return expanded
            .into_iter()
            .map(|e| {
                (
                    e.data_set,
                    e.args.into_iter().map(Value::from_php).collect(),
                )
            })
            .collect();
    }

    // #[TestWith([...])] rows from the raw method AST. Unlike #[DataProvider]
    // (a SEPARATE method a subclass can override independently — resolved against
    // the concrete class in `data_provider::expand`), #[TestWith] lives ON the test
    // method, so its rows come from wherever the test method body is declared. That
    // is exactly `body_class()`: discovery resolves the most-derived declaration of
    // the method (an override sets `declaring_class = None` → the concrete class),
    // so reading from `body_class()` already matches the concrete run class.
    let test_with = project
        .with_program(logical, |program, _file, _names| {
            find_method(program, test.body_class(), &test.method).map(read_test_with_rows)
        })
        .flatten()
        .unwrap_or_default();
    if !test_with.is_empty() {
        return test_with;
    }

    // No provider → a single invocation with no args.
    vec![(None, vec![])]
}

/// Reduce one row: bind args to the method's parameters (+ the test-case `$this`)
/// and run the body. `body_class` is the FQCN of the class DECLARING the test
/// method body (round 18) — property reads in the body enforce PHP visibility
/// from that scope (the doctrine fixture pattern: a test method reading its own
/// class's private fixture prop stays readable; a foreign private prop bails).
#[allow(clippy::too_many_arguments)]
fn reduce_row(
    block: &Block,
    param_names: &[Vec<u8>],
    args: &[Value],
    this_value: Option<Value>,
    resolver: &BridgeResolver,
    names: &mago_names::ResolvedNames,
    source: &[u8],
    body_class: &str,
) -> Outcome {
    // SURPLUS provider columns (more columns than parameters) are NOT an error in
    // PHP/PHPUnit: data-provider rows are bound positionally via
    // call_user_func_array, and a non-variadic method silently ignores the extra
    // columns (verified vs `php -r`). The `zip` below already binds only the first
    // `param_names.len()` columns, matching PHPUnit — so we must NOT bail here
    // (Task G; was the "48 more provider columns than parameters" false bail).
    let mut givens: HashMap<Vec<u8>, Value> = HashMap::new();
    // Bind the test-case `$this` (Inc-3 Task A) so `$this->prop` reads and
    // `$this->helper()` calls resolve against the seeded test-case record.
    if let Some(this) = this_value {
        givens.insert(b"this".to_vec(), this);
    }
    for (name, val) in param_names.iter().zip(args.iter()) {
        givens.insert(name.clone(), val.clone());
    }
    // Parameters past the provided args are unbound; if the body reads one it
    // bails (UnboundVariable) — fail-closed, no defaults invented here.
    run_method_body_with_names(
        block,
        givens,
        resolver,
        names,
        source,
        Some(body_class.as_bytes()),
    )
}

// ─── AST helpers ──────────────────────────────────────────────────────────────

/// Find `class_name::method_name` in the program (descends namespaces).
fn find_method<'a>(
    program: &'a Program<'a>,
    class_name: &str,
    method_name: &str,
) -> Option<&'a Method<'a>> {
    let simple_class = class_name.rsplit('\\').next().unwrap_or(class_name);
    find_method_in(program.statements.iter(), simple_class, method_name)
}

fn find_method_in<'a, 's, I>(
    stmts: I,
    simple_class: &str,
    method_name: &str,
) -> Option<&'s Method<'s>>
where
    's: 'a,
    I: Iterator<Item = &'s Statement<'s>>,
{
    for stmt in stmts {
        match stmt {
            Statement::Class(class)
                if class
                    .name
                    .value
                    .eq_ignore_ascii_case(simple_class.as_bytes()) =>
            {
                for member in class.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if m.name.value.eq_ignore_ascii_case(method_name.as_bytes()) {
                            return Some(m);
                        }
                    }
                }
            }
            Statement::Namespace(ns) => {
                use mago_syntax::ast::ast::namespace::NamespaceBody;
                let found = match &ns.body {
                    NamespaceBody::Implicit(b) => {
                        find_method_in(b.statements.iter(), simple_class, method_name)
                    }
                    NamespaceBody::BraceDelimited(b) => {
                        find_method_in(b.statements.iter(), simple_class, method_name)
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

/// The bare parameter names of a method (in order). Variadic/by-ref bail.
fn method_param_names(method: &Method) -> Result<Vec<Vec<u8>>, BailReason> {
    let mut names = Vec::new();
    for p in method.parameter_list.parameters.iter() {
        if p.ellipsis.is_some() {
            return Err(BailReason::UnsupportedConstruct(
                "variadic test parameter".into(),
            ));
        }
        if p.ampersand.is_some() {
            return Err(BailReason::UnsupportedConstruct(
                "by-reference test parameter".into(),
            ));
        }
        let bare = p
            .variable
            .name
            .strip_prefix(b"$")
            .unwrap_or(p.variable.name)
            .to_vec();
        names.push(bare);
    }
    Ok(names)
}

/// Read `#[TestWith([...])]` rows off a method's attributes. Each `TestWith`
/// attribute carries one positional array literal → one row. Rows whose elements
/// are not concretely-computable are dropped (those tests then run for real via a
/// later no-row fallback — but we keep computable ones).
fn read_test_with_rows(method: &Method) -> Vec<(Option<String>, Vec<Value>)> {
    use crate::concrete::{compute, Context, PhpValue};
    use mago_syntax::ast::ast::argument::Argument;

    let mut rows = Vec::new();
    let mut idx = 0usize;
    for list in method.attribute_lists.iter() {
        for attr in list.attributes.iter() {
            if !attribute_simple_name(&attr.name).eq_ignore_ascii_case(b"TestWith") {
                continue;
            }
            let Some(arg_list) = &attr.argument_list else {
                continue;
            };
            // First positional argument is the row array literal.
            let Some(Argument::Positional(p)) = arg_list.arguments.iter().next() else {
                continue;
            };
            // Evaluate the array literal concretely.
            let mut ctx = Context::new();
            let Ok(value) = compute(p.value, &mut ctx) else {
                continue;
            };
            let PhpValue::Array(map) = value else {
                continue;
            };
            let args: Vec<Value> = map.into_values().map(Value::from_php).collect();
            rows.push((Some(idx.to_string()), args));
            idx += 1;
        }
    }
    rows
}

/// The simple (unqualified) name of an attribute identifier.
fn attribute_simple_name<'a>(
    name: &'a mago_syntax::ast::ast::identifier::Identifier<'a>,
) -> &'a [u8] {
    use mago_syntax::ast::ast::identifier::Identifier;
    let full = match name {
        Identifier::Local(l) => l.value,
        Identifier::Qualified(q) => q.value,
        Identifier::FullyQualified(f) => f.value,
    };
    full.rsplit(|b| *b == b'\\').next().unwrap_or(full)
}

fn bailed(test: &TestMethod, data_set: Option<String>, reason: &str) -> ReducedTest {
    ReducedTest {
        class: test.class.clone(),
        method: test.method.clone(),
        data_set,
        outcome: Outcome::Bailed(BailReason::Other(reason.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measurement harness (Inc-4 acceptance): run the reducer on the real
    /// doctrine/collections CONCRETE test file via the NORMAL `reduce_file` path
    /// (no special harness) and print the reduced fraction + bail histogram.
    /// Ignored by default (needs a cloned + composer-installed checkout at
    /// /tmp/doctrine-collections). Run with:
    ///   cargo test -p analyzer --lib reduce::driver::tests::measure_doctrine_collections -- --ignored --nocapture
    #[test]
    #[ignore]
    fn measure_doctrine_collections() {
        use std::collections::BTreeMap;
        let root = Path::new("/tmp/doctrine-collections");
        // Query the CONCRETE subclass file. Its accessor tests are INHERITED from
        // the abstract ArrayCollectionTestCase; Inc-4 Task C surfaces them to this
        // concrete class (bound to its `$this`, whose `buildCollection` returns a
        // real ArrayCollection) through production discovery — so `reduce_file`
        // reproduces the measurement with NO `reduce_as_concrete` harness.
        let test_file = root.join("tests/ArrayCollectionTest.php");
        if !test_file.exists() {
            eprintln!("SKIP: {} not present", test_file.display());
            return;
        }

        let reduced = reduce_in_root(root, &test_file).expect("reduce_file");

        let total = reduced.len();
        let mut pass = 0usize;
        let mut fail = 0usize;
        let mut bail = 0usize;
        let mut hist: BTreeMap<String, usize> = BTreeMap::new();
        for r in &reduced {
            match &r.outcome {
                Outcome::Pass => pass += 1,
                Outcome::Fail(_) => fail += 1,
                Outcome::Bailed(reason) => {
                    bail += 1;
                    let detail = match reason {
                        BailReason::UnsupportedConstruct(m)
                        | BailReason::UnknownCall(m)
                        | BailReason::Other(m)
                        | BailReason::TypeError(m)
                        | BailReason::UnboundVariable(m) => {
                            format!("{}: {}", reason.tag(), m)
                        }
                        other => other.tag().to_string(),
                    };
                    *hist.entry(detail).or_default() += 1;
                }
            }
        }
        println!("\n=== doctrine/collections ArrayCollectionTest.php ===");
        println!(
            "rows={total}  PASS={pass}  FAIL={fail}  BAIL={bail}  reduced={:.1}%",
            if total == 0 {
                0.0
            } else {
                100.0 * (pass + fail) as f64 / total as f64
            }
        );
        println!("--- per-(method,row) ---");
        for r in &reduced {
            let tag = match &r.outcome {
                Outcome::Pass => "PASS".to_string(),
                Outcome::Fail(_) => "FAIL".to_string(),
                Outcome::Bailed(b) => format!("BAIL/{}", b.tag()),
            };
            println!(
                "  {} [{}] -> {}",
                r.method,
                r.data_set.as_deref().unwrap_or("-"),
                tag
            );
        }
        println!("--- bail histogram ---");
        for (reason, n) in &hist {
            println!("  {n:>3}  {reason}");
        }
    }

    /// Second-suite breadth harness (Inc-4 acceptance): run the reducer via the
    /// NORMAL `reduce_file` path on an arbitrary cloned suite file and print the
    /// fraction + bail histogram. Path is taken from `REDUCE_MEASURE_FILE` so any
    /// suite can be measured without editing code. Ignored by default.
    ///   REDUCE_MEASURE_FILE=/tmp/x/tests/FooTest.php cargo test -p analyzer --lib \
    ///     reduce::driver::tests::measure_arbitrary_suite -- --ignored --nocapture
    #[test]
    #[ignore]
    fn measure_arbitrary_suite() {
        use std::collections::BTreeMap;
        let Ok(file) = std::env::var("REDUCE_MEASURE_FILE") else {
            eprintln!("SKIP: set REDUCE_MEASURE_FILE=/abs/path/to/SomeTest.php");
            return;
        };
        let test_file = Path::new(&file);
        // Default scan root: the composer.json PROJECT ROOT (same as production
        // `reduce_file` via `project_root_of`) so the suite's own src/ resolves.
        // An optional REDUCE_MEASURE_ROOT override pins an explicit root. Benign
        // measurement knob.
        let root_env = std::env::var("REDUCE_MEASURE_ROOT").ok();
        let root = match &root_env {
            Some(r) => Path::new(r).to_path_buf(),
            None => project_root_of(test_file),
        };
        if !test_file.exists() {
            eprintln!("SKIP: {} not present", test_file.display());
            return;
        }
        eprintln!("SCAN_ROOT={}", root.display());

        let scan_t0 = std::time::Instant::now();
        let reduced = reduce_in_root(&root, test_file).expect("reduce_file");
        eprintln!("SCAN+REDUCE_MS={}", scan_t0.elapsed().as_millis());
        let total = reduced.len();
        let mut pass = 0usize;
        let mut fail = 0usize;
        let mut bail = 0usize;
        let mut hist: BTreeMap<String, usize> = BTreeMap::new();
        for r in &reduced {
            match &r.outcome {
                Outcome::Pass => pass += 1,
                Outcome::Fail(_) => fail += 1,
                Outcome::Bailed(reason) => {
                    bail += 1;
                    let detail = match reason {
                        BailReason::UnsupportedConstruct(m)
                        | BailReason::UnknownCall(m)
                        | BailReason::Other(m)
                        | BailReason::TypeError(m)
                        | BailReason::UnboundVariable(m) => format!("{}: {}", reason.tag(), m),
                        other => other.tag().to_string(),
                    };
                    *hist.entry(detail).or_default() += 1;
                }
            }
        }
        println!("\n=== {} ===", test_file.display());
        println!(
            "rows={total}  PASS={pass}  FAIL={fail}  BAIL={bail}  reduced={:.1}%",
            if total == 0 {
                0.0
            } else {
                100.0 * (pass + fail) as f64 / total as f64
            }
        );
        println!("--- per-(method,row) ---");
        for r in &reduced {
            let tag = match &r.outcome {
                Outcome::Pass => "PASS".to_string(),
                Outcome::Fail(m) => format!("FAIL({m})"),
                Outcome::Bailed(b) => format!("BAIL/{}", b.tag()),
            };
            println!(
                "  {} [{}] -> {}",
                r.method,
                r.data_set.as_deref().unwrap_or("-"),
                tag
            );
        }
        println!("--- bail histogram ---");
        for (reason, n) in &hist {
            println!("  {n:>3}  {reason}");
        }
    }

    /// Build a temp PHP PROJECT (composer.json + src/ + tests/) and return the dir.
    /// Unlike `write_suite` (flat), this lays out a real project tree so the
    /// project-root scan (Task 1) can resolve the library's own `src/` classes.
    fn write_project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("composer.json"), "{\n}\n").unwrap();
        // Minimal PHPUnit stub under the project root so test classes resolve.
        std::fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        )
        .unwrap();
        for (name, src) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, src).unwrap();
        }
        dir
    }

    const MONEY_SRC: &str = r#"<?php
final class Money {
    public function __construct(public int $cents) {}
    public function cents(): int { return $this->cents; }
}
"#;
    const MONEY_TEST_SRC: &str = r#"<?php
use PHPUnit\Framework\TestCase;
class MoneyTest extends TestCase {
    public function testCents(): void {
        $m = new Money(500);
        $this->assertSame(500, $m->cents());
    }
}
"#;

    /// Task 1 RED: with the NARROW root (the test file's own directory `tests/`),
    /// `src/Money.php` is not scanned, so `new Money` cannot resolve and the row
    /// BAILS on `new Money` — proving the bail is a SCOPING artifact, not a real
    /// modelling gap.
    #[test]
    fn narrow_root_bails_on_unscanned_project_class() {
        let dir = write_project(&[
            ("src/Money.php", MONEY_SRC),
            ("tests/MoneyTest.php", MONEY_TEST_SRC),
        ]);
        let test_file = dir.path().join("tests/MoneyTest.php");
        // The narrow root = the test file's parent dir (`tests/`), which excludes src/.
        let narrow = test_file.parent().unwrap().to_path_buf();
        let reduced = reduce_in_root(&narrow, &test_file).unwrap();
        assert_eq!(reduced.len(), 1);
        match &reduced[0].outcome {
            Outcome::Bailed(BailReason::UnknownCall(m)) => {
                assert!(
                    m.contains("new Money") || m == "new Money",
                    "expected a `new Money` bail under the narrow root; got {m:?}"
                );
            }
            other => {
                panic!("expected Bailed(UnknownCall new Money) under narrow root; got {other:?}")
            }
        }
    }

    /// Task 1 GREEN: `reduce_file` walks up to the composer.json project root, so
    /// `src/Money.php` IS scanned and `new Money` RESOLVES — the bail is no longer
    /// `new Money`. (The full Pass is unlocked once instance dispatch lands in
    /// Task 3; here we assert only that `new Money` is no longer the wall.)
    #[test]
    fn project_root_scan_resolves_project_class() {
        let dir = write_project(&[
            ("src/Money.php", MONEY_SRC),
            ("tests/MoneyTest.php", MONEY_TEST_SRC),
        ]);
        let test_file = dir.path().join("tests/MoneyTest.php");
        // Sanity: project_root_of must find the composer.json dir, not `tests/`.
        assert_eq!(project_root_of(&test_file), dir.path());
        let reduced = reduce_file(&test_file).unwrap();
        assert_eq!(reduced.len(), 1);
        match &reduced[0].outcome {
            Outcome::Bailed(BailReason::UnknownCall(m)) => assert!(
                !m.contains("new Money"),
                "`new Money` must resolve under the project root; still bailing on it: {m:?}"
            ),
            // Pass is also acceptable (Task 3 unlocks the getter); never a wrong Fail.
            Outcome::Pass => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn static_prop_shadowing_ancestor_private_bails() {
        // Round 19 (Fix C). Gold (php8.4, runs clean): StaticShadowTest's
        // `public static $x` shadows StaticShadowBase's `private $x`. Reading
        // $this->x is an undefined-INSTANCE-property access (a static is not
        // served through `->`) → null → assertSame(1, null) FAILS in PHPUnit.
        // The tolerant test-case seeder skipped the static BEFORE declaring it,
        // fusing with the parent private slot → a definitive false green.
        // Declaring the static now triggers the shadowed-private slot bail.
        let dir = write_suite(&[(
            "StaticShadowTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
abstract class StaticShadowBase extends TestCase { private $x = 1; }
final class StaticShadowTest extends StaticShadowBase {
    public static $x = 2;
    public function testRead(): void {
        $this->assertSame(1, $this->x);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("StaticShadowTest.php")).unwrap();
        let got = outcomes(&reduced);
        assert!(
            got.iter()
                .any(|(m, _, o)| *m == "testRead" && matches!(o, Outcome::Bailed(_))),
            "static-shadow test must BAIL (not a definitive verdict); got {got:?}"
        );
    }

    #[test]
    fn property_hook_in_test_case_chain_bails() {
        // Round 19 (Fix D). Gold (php8.4, runs clean): HookShadowTest's virtual
        // hooked $h routes $this->h through the `get` hook → 99 → assertSame(99,
        // 99) PASSES. The tolerant seeder skipped the hooked prop, reading the
        // parent private slot 1 → a definitive false FAIL. A property hook
        // anywhere in the test-case chain now bails the $this build.
        let dir = write_suite(&[(
            "HookShadowTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
abstract class HookBase extends TestCase { private $h = 1; }
final class HookShadowTest extends HookBase {
    public int $h { get => 99; set(int $v) {} }
    public function testRead(): void {
        $this->assertSame(99, $this->h);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("HookShadowTest.php")).unwrap();
        let got = outcomes(&reduced);
        assert!(
            got.iter()
                .any(|(m, _, o)| *m == "testRead" && matches!(o, Outcome::Bailed(_))),
            "hooked-prop test must BAIL; got {got:?}"
        );
    }

    fn write_suite(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // Minimal PHPUnit stubs so the test classes resolve as TestCase subclasses.
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
            "<?php namespace PHPUnit\\Framework\\Attributes; class DataProvider { public function __construct(string $n) {} }",
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/Attributes/TestWith.php"),
            "<?php namespace PHPUnit\\Framework\\Attributes; class TestWith { public function __construct(array $d) {} }",
        )
        .unwrap();
        for (name, src) in files {
            std::fs::write(dir.path().join(name), src).unwrap();
        }
        dir
    }

    fn outcomes(reduced: &[ReducedTest]) -> Vec<(&str, Option<&str>, &Outcome)> {
        reduced
            .iter()
            .map(|r| (r.method.as_str(), r.data_set.as_deref(), &r.outcome))
            .collect()
    }

    #[test]
    fn inherited_test_method_reduces_via_normal_path() {
        // Inc-4 C: an abstract *TestCase declares `testInherited`; the concrete
        // subclass runs it with `$this->value()` returning a concrete int. The
        // NORMAL `reduce_file` (querying the CONCRETE file) must surface the
        // inherited test, bind `$this` to the concrete class, and reduce to Pass —
        // no special harness.
        let dir = write_suite(&[
            (
                "AbstractValueTestCase.php",
                r#"<?php
use PHPUnit\Framework\TestCase;
abstract class AbstractValueTestCase extends TestCase {
    abstract protected function value(): int;
    public function testInherited(): void {
        $this->assertSame(42, $this->value());
    }
}
"#,
            ),
            (
                "ConcreteValueTest.php",
                r#"<?php
class ConcreteValueTest extends AbstractValueTestCase {
    protected function value(): int { return 42; }
    public function testOwn(): void {
        $this->assertSame(1, 1);
    }
}
"#,
            ),
        ]);
        let reduced = reduce_file(&dir.path().join("ConcreteValueTest.php")).unwrap();
        let got = outcomes(&reduced);
        // Both the inherited and the own test must be present AND pass.
        assert!(
            got.iter()
                .any(|(m, _, o)| *m == "testInherited" && **o == Outcome::Pass),
            "inherited test must reduce to Pass via the normal path; got {got:?}"
        );
        assert!(
            got.iter()
                .any(|(m, _, o)| *m == "testOwn" && **o == Outcome::Pass),
            "own test must still reduce; got {got:?}"
        );
        // The abstract parent must NOT contribute its own (duplicate) row.
        assert!(
            reduced
                .iter()
                .all(|r| r.class.eq_ignore_ascii_case("ConcreteValueTest")),
            "all rows must be attributed to the concrete class; got {reduced:?}"
        );
    }

    #[test]
    fn reduces_a_no_provider_test() {
        let dir = write_suite(&[(
            "MathTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class MathTest extends TestCase {
    public function testAddition(): void {
        $this->assertSame(4, 2 + 2);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("MathTest.php")).unwrap();
        assert_eq!(reduced.len(), 1);
        assert_eq!(reduced[0].outcome, Outcome::Pass);
        assert_eq!(reduced[0].method, "testAddition");
        assert_eq!(reduced[0].data_set, None);
    }

    #[test]
    fn reduces_a_data_provider_test_per_row() {
        let dir = write_suite(&[(
            "AddTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
class AddTest extends TestCase {
    public static function rows(): array {
        return [
            'two_and_three' => [2, 3, 5],
            'fails'         => [2, 2, 5],
        ];
    }

    #[DataProvider('rows')]
    public function testAdd(int $a, int $b, int $expected): void {
        $this->assertSame($expected, $a + $b);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("AddTest.php")).unwrap();
        let got = outcomes(&reduced);
        assert!(
            got.contains(&("testAdd", Some("two_and_three"), &Outcome::Pass)),
            "got {got:?}"
        );
        assert!(
            got.iter()
                .any(|(_, ds, o)| *ds == Some("fails") && matches!(o, Outcome::Fail(_))),
            "the 2+2==5 row must Fail; got {got:?}"
        );
    }

    #[test]
    fn reduces_a_test_with_user_function_substitution() {
        let dir = write_suite(&[(
            "HelperTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
function triple(int $x): int { return $x * 3; }
class HelperTest extends TestCase {
    public function testTriple(): void {
        $this->assertSame(9, triple(3));
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("HelperTest.php")).unwrap();
        assert_eq!(reduced.len(), 1);
        assert_eq!(reduced[0].outcome, Outcome::Pass, "got {reduced:?}");
    }

    #[test]
    fn reduces_test_with_rows() {
        let dir = write_suite(&[(
            "TwTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\TestWith;
class TwTest extends TestCase {
    #[TestWith([2, 3, 5])]
    #[TestWith([10, 20, 30])]
    public function testAdd(int $a, int $b, int $expected): void {
        $this->assertSame($expected, $a + $b);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("TwTest.php")).unwrap();
        assert_eq!(reduced.len(), 2, "two TestWith rows; got {reduced:?}");
        assert!(
            reduced.iter().all(|r| r.outcome == Outcome::Pass),
            "got {reduced:?}"
        );
    }

    #[test]
    fn surplus_provider_columns_are_ignored_not_bailed() {
        // PHPUnit binds provider columns positionally; a row with MORE columns
        // than the method has parameters silently ignores the surplus (verified vs
        // `php -r`). The reducer must reduce, not bail (Task G).
        let dir = write_suite(&[(
            "SurplusTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
class SurplusTest extends TestCase {
    public static function rows(): array {
        // 3 columns, but the test takes only 2 params — the 3rd is surplus.
        return ['r' => [2, 3, 999]];
    }

    #[DataProvider('rows')]
    public function testAdd(int $a, int $b): void {
        $this->assertSame(5, $a + $b);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("SurplusTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "surplus columns must be ignored, not bailed; got {reduced:?}"
        );
    }

    #[test]
    fn reduces_an_object_value_test() {
        // A method-level object test reduces through the full driver path: new +
        // instance method + $this read + scalar assertSame.
        let dir = write_suite(&[(
            "PointDriverTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
final class Point {
    public function __construct(public int $x, public int $y) {}
    public function plus(Point $p): Point { return new Point($this->x + $p->x, $this->y + $p->y); }
    public function getX(): int { return $this->x; }
}
class PointDriverTest extends TestCase {
    public function testPlus(): void {
        $this->assertSame(4, (new Point(1, 2))->plus(new Point(3, 0))->getX());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("PointDriverTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(reduced[0].outcome, Outcome::Pass, "got {reduced:?}");
    }

    #[test]
    fn handwritten_test_case_object_reduces_setup_and_helper() {
        // Inc-3 acceptance (Tasks A–C): a TestCase subclass whose setUp() seeds
        // $this->items = [1,2,3], a pure helper sum(): int, and a test that asserts
        // $this->assertSame(6, $this->sum()). Proves: $this bound to a Value::Object
        // (A), setUp run as the seeding phase (B), and $this->helper() inlined (C).
        let dir = write_suite(&[(
            "SumTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class SumTest extends TestCase {
    private array $items;

    protected function setUp(): void {
        $this->items = [1, 2, 3];
    }

    private function sum(): int {
        $total = 0;
        foreach ($this->items as $v) {
            $total = $total + $v;
        }
        return $total;
    }

    public function testSum(): void {
        $this->assertSame(6, $this->sum());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("SumTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "setUp-seeded $this + $this->sum() helper must reduce to Pass; got {reduced:?}"
        );
    }

    #[test]
    fn body_mutator_on_this_prop_bails() {
        // setUp is the ONLY sanctioned write phase. A test BODY that writes
        // $this->prop is a mutator → bail (frontier §2), even with $this bound.
        let dir = write_suite(&[(
            "MutTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class MutTest extends TestCase {
    private int $n;
    protected function setUp(): void {
        $this->n = 1;
    }
    public function testMutate(): void {
        $this->n = 2;
        $this->assertSame(2, $this->n);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("MutTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "a $this->prop write in the test body is a mutator → must bail; got {reduced:?}"
        );
    }

    #[test]
    fn impure_setup_bails_the_whole_test() {
        // A setUp that touches an unmodelled/impure construct (time()) has
        // incomplete Givens → bail the whole test (B), never guess.
        let dir = write_suite(&[(
            "ImpureSetupTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class ImpureSetupTest extends TestCase {
    private int $t;
    protected function setUp(): void {
        $this->t = time();
    }
    public function testT(): void {
        $this->assertSame(0, $this->t);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("ImpureSetupTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "an impure setUp must bail the test (incomplete Givens); got {reduced:?}"
        );
    }

    #[test]
    fn set_up_before_class_still_bails() {
        // Class-level setUpBeforeClass is deferred → bail (B).
        let dir = write_suite(&[(
            "ClassFixtureTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class ClassFixtureTest extends TestCase {
    public static function setUpBeforeClass(): void {}
    public function testX(): void {
        $this->assertSame(1, 1);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("ClassFixtureTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "setUpBeforeClass is deferred → must bail; got {reduced:?}"
        );
    }

    #[test]
    fn unmodelled_test_bails_not_lies() {
        let dir = write_suite(&[(
            "TimeTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class TimeTest extends TestCase {
    public function testNow(): void {
        $this->assertSame(0, time());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("TimeTest.php")).unwrap();
        assert_eq!(reduced.len(), 1);
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "an unknown stateful call must BAIL, not guess; got {reduced:?}"
        );
    }

    #[test]
    fn closure_returned_from_helper_reduces() {
        // A closure CREATED inside an inlined helper function escapes that helper's
        // own re-parse arena (subst.rs nested `with_program`) and is invoked back in
        // the test body. With the old raw-pointer Value::Closure this read a dropped
        // arena (UAF); the owned-source closure must reduce cleanly to Pass.
        let dir = write_suite(&[(
            "AdderTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
function makeAdder(int $x) { return fn($y) => $x + $y; }
class AdderTest extends TestCase {
    public function testAdder(): void {
        $add = makeAdder(5);
        $this->assertSame(8, $add(3));
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("AdderTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "a closure returned from an inlined helper must reduce to Pass (was UAF); got {reduced:?}"
        );
    }

    #[test]
    fn closure_stored_in_this_via_setup_reduces() {
        // setUp() stores a closure into $this->cb; the closure is created inside the
        // setUp re-parse arena and survives into the test body via the seeded
        // Value::Object. The owned-source closure must invoke cleanly (was UAF).
        let dir = write_suite(&[(
            "CbTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class CbTest extends TestCase {
    private $cb;
    protected function setUp(): void {
        $this->cb = fn() => 7;
    }
    public function testCb(): void {
        $this->assertSame(7, ($this->cb)());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("CbTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "a closure stored into $this via setUp must reduce to Pass (was UAF); got {reduced:?}"
        );
    }

    #[test]
    fn scalar_return_type_coercion_bails_not_fails() {
        // PHP coerces an inlined body's return to the declared scalar type: a
        // `: string` method returning `true` yields "1", so assertSame('1', resolve(true))
        // PASSES in real PHP. The reducer does not model that coercion; returning the
        // un-coerced bool produced a divergent FAIL (the symfony LazyString::resolve
        // false-FAIL). It must BAIL instead — never a wrong Fail on a green suite.
        let dir = write_suite(&[(
            "CoerceTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
final class Box {
    public static function resolve(bool $v): string { return $v; }
}
class CoerceTest extends TestCase {
    public function testResolve(): void {
        $this->assertSame('1', Box::resolve(true));
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("CoerceTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "scalar return-type coercion must BAIL, not produce a divergent Fail; got {reduced:?}"
        );
    }

    #[test]
    fn inherited_typed_property_write_coercion_bails() {
        // A CHILD ctor writes a scalar value into a typed property DECLARED IN A
        // PARENT. PHP coerces "42" → int(42) at the typed parent-property write,
        // so assertSame(42, …) PASSES. The reducer must collect the inherited
        // scalar hint (parent's `int $n`) so the typed-write coercion guard fires
        // and BAILS — storing Str("42") verbatim produced a divergent FAIL.
        // The `: mixed` getter keeps the return-coercion guard from masking the
        // property-write site under test.
        let dir = write_suite(&[(
            "InheritedWriteTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class Base { public int $n; }
class Child extends Base {
    public function __construct(string $s) { $this->n = $s; }
    public function n(): mixed { return $this->n; }
}
class InheritedWriteTest extends TestCase {
    public function testInherited(): void {
        $c = new Child("42");
        $this->assertSame(42, $c->n());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("InheritedWriteTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "a scalar write into an inherited typed property must BAIL on coercion, \
             not produce a divergent Fail; got {reduced:?}"
        );
    }

    #[test]
    fn inherited_typed_property_matching_write_does_not_over_bail() {
        // Guard (a): a CHILD ctor writes a value of the MATCHING type (int 42)
        // into the inherited typed property `int $n`. No coercion happens, so the
        // guard must NOT bail — the test resolves to Pass.
        let dir = write_suite(&[(
            "InheritedMatchTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class BaseM { public int $n; }
class ChildM extends BaseM {
    public function __construct(int $v) { $this->n = $v; }
    public function n(): mixed { return $this->n; }
}
class InheritedMatchTest extends TestCase {
    public function testMatch(): void {
        $c = new ChildM(42);
        $this->assertSame(42, $c->n());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("InheritedMatchTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            !matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "a matching-type write into an inherited typed property must NOT over-bail; \
             got {reduced:?}"
        );
        assert_eq!(reduced[0].outcome, Outcome::Pass, "got {reduced:?}");
    }

    #[test]
    fn inherited_untyped_property_write_does_not_over_bail() {
        // Guard (b): the inherited property is UNTYPED, so there is no scalar hint
        // and no coercion contract. Writing a string into it must NOT trip the
        // coercion guard — the reducer resolves it verbatim (Pass), not a bail.
        let dir = write_suite(&[(
            "InheritedUntypedTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class BaseU { public $n; }
class ChildU extends BaseU {
    public function __construct(string $s) { $this->n = $s; }
    public function n(): mixed { return $this->n; }
}
class InheritedUntypedTest extends TestCase {
    public function testUntyped(): void {
        $c = new ChildU("42");
        $this->assertSame("42", $c->n());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("InheritedUntypedTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            !matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "an untyped inherited property has no scalar hint → the coercion guard \
             must NOT fire; got {reduced:?}"
        );
        assert_eq!(reduced[0].outcome, Outcome::Pass, "got {reduced:?}");
    }

    #[test]
    fn trait_typed_property_write_coercion_bails() {
        // Round 5 RED: a ctor writes a scalar value into a typed property declared
        // in a USED TRAIT (not the leaf class, not a parent). PHP coerces "10" →
        // int(10) at the typed trait-property write, so assertSame(10, …) PASSES.
        // The reducer must collect the trait's scalar hint (`int $n`) so the
        // typed-write coercion guard fires and BAILS — before round 5 the trait
        // hint was missed (used_traits is a separate set, never an
        // all_parent_classes member), the value was stored Str("10") verbatim, and
        // the test produced a divergent definitive FAIL. `: mixed` getter keeps the
        // return-coercion guard from masking the property-write site under test.
        let dir = write_suite(&[(
            "TraitWriteTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
trait HasN { public int $n; }
class P {
    use HasN;
    public function __construct(string $s) { $this->n = $s; }
    public function n(): mixed { return $this->n; }
}
class TraitWriteTest extends TestCase {
    public function testTrait(): void {
        $p = new P("10");
        $this->assertSame(10, $p->n());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("TraitWriteTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "a scalar write into a TRAIT-declared typed property must BAIL on \
             coercion, not produce a divergent Fail; got {reduced:?}"
        );
    }

    #[test]
    fn trait_typed_property_matching_write_does_not_over_bail() {
        // Round 5 guard (a): a ctor writes a value of the MATCHING type (int 10)
        // into a trait-declared typed property `int $n`. No coercion happens, so
        // the guard must NOT bail — the test resolves to Pass.
        let dir = write_suite(&[(
            "TraitMatchTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
trait HasNm { public int $n; }
class Pm {
    use HasNm;
    public function __construct(int $v) { $this->n = $v; }
    public function n(): mixed { return $this->n; }
}
class TraitMatchTest extends TestCase {
    public function testMatch(): void {
        $p = new Pm(10);
        $this->assertSame(10, $p->n());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("TraitMatchTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            !matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "a matching-type write into a trait-declared typed property must NOT \
             over-bail; got {reduced:?}"
        );
        assert_eq!(reduced[0].outcome, Outcome::Pass, "got {reduced:?}");
    }

    #[test]
    fn array_map_object_element_mutation_in_callback_bails() {
        // Inc-5 RED (closure PARAM binding aliasing): array_map builds an array of
        // objects, a second array_map's callback MUTATES each element. php8.4
        // prints v=2 (the callback mutates the caller's object through the shared
        // handle), but the by-value model used to bind the element to `$o` raw —
        // the element was UNMARKED (array_map output pushes skipped store-time
        // marking, and the closure builtins bypass eval_arguments' caller-side
        // marking) — so `$o->inc()` succeeded on a closure-local clone, the
        // caller's array kept v=1, and the model produced a DIVERGENT Fail.
        // Marking object args at the closure param-binding site makes this BAIL.
        let dir = write_suite(&[(
            "MapMutateTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class Foo {
    public int $v;
    public function __construct(int $v) { $this->v = $v; }
    public function inc() { $this->v = $this->v + 1; }
}
class MapMutateTest extends TestCase {
    public function testMapMutate(): void {
        $objs = array_map(fn($i) => new Foo($i), [1]);
        array_map(function($o) { $o->inc(); return true; }, $objs);
        $this->assertSame(2, $objs[0]->v);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("MapMutateTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "a callback mutating an object element bound to a closure parameter \
             must BAIL (php8.4 mutates through the shared handle → v=2), not \
             produce a divergent Fail; got {reduced:?}"
        );
    }

    #[test]
    fn array_map_object_elements_pure_callback_still_passes() {
        // Inc-5 over-bail guard: the same array_map-of-objects shape with a PURE
        // callback (a read-only getter) must still resolve — marking the bound
        // object aliased only forbids MUTATION, never reads.
        let dir = write_suite(&[(
            "MapPureTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class Fooo {
    public int $v;
    public function __construct(int $v) { $this->v = $v; }
    public function v(): int { return $this->v; }
}
class MapPureTest extends TestCase {
    public function testMapPure(): void {
        $objs = [new Fooo(1), new Fooo(2)];
        $vals = array_map(fn($o) => $o->v(), $objs);
        $this->assertSame([1, 2], $vals);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("MapPureTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "a pure callback over object elements must NOT over-bail; got {reduced:?}"
        );
    }

    #[test]
    fn array_map_produced_object_pure_read_still_passes() {
        // Inc-5 over-bail guard for the builtin-output marking (defense-in-depth):
        // objects synthesized BY the array_map callback are marked as they enter
        // the output array; a later pure property READ through the array must
        // still resolve to Pass (reads of aliased objects are exact).
        let dir = write_suite(&[(
            "MapReadTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class Foor {
    public int $v;
    public function __construct(int $v) { $this->v = $v; }
}
class MapReadTest extends TestCase {
    public function testMapRead(): void {
        $objs = array_map(fn($i) => new Foor($i), [1]);
        $this->assertSame(1, $objs[0]->v);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("MapReadTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "a pure read of an array_map-produced object must NOT over-bail; got {reduced:?}"
        );
    }

    #[test]
    fn trait_untyped_property_write_does_not_over_bail() {
        // Round 5 guard (b): the trait-declared property is UNTYPED, so there is no
        // scalar hint and no coercion contract. Writing a string into it must NOT
        // trip the coercion guard — the reducer resolves it verbatim (Pass).
        let dir = write_suite(&[(
            "TraitUntypedTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
trait HasNu { public $n; }
class Pu {
    use HasNu;
    public function __construct(string $s) { $this->n = $s; }
    public function n(): mixed { return $this->n; }
}
class TraitUntypedTest extends TestCase {
    public function testUntyped(): void {
        $p = new Pu("10");
        $this->assertSame("10", $p->n());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("TraitUntypedTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            !matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "an untyped trait property has no scalar hint → the coercion guard must \
             NOT fire; got {reduced:?}"
        );
        assert_eq!(reduced[0].outcome, Outcome::Pass, "got {reduced:?}");
    }

    #[test]
    fn ext_ancestor_construction_bails_not_false_green() {
        // FALSE GREEN (gold-verified): `class MyDate extends \DateTime {}` — the
        // parent is NOT in a project-only scan (no PHP stubs), so its internal
        // state (the wall-clock instant) is unmodelled and a no-arg `new MyDate()`
        // built an EMPTY record. `assertEquals(new MyDate(), new MyDate())` then
        // compared {} == {} → Pass, while real PHPUnit routes the pair through
        // DateTimeComparator (compares the two instants). Construction must BAIL.
        let dir = write_suite(&[(
            "ExtDateTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class MyDate extends \DateTime {}
class ExtDateTest extends TestCase {
    public function testEquals(): void {
        $this->assertEquals(new MyDate(), new MyDate());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("ExtDateTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        match &reduced[0].outcome {
            Outcome::Bailed(BailReason::UnsupportedConstruct(m)) => assert!(
                m.contains("not in the codebase"),
                "the bail must name the unresolvable ancestor; got {m:?}"
            ),
            other => panic!(
                "constructing a subclass of an unscanned ext class must BAIL \
                 (ext-internal state unmodelled), never a false green; got {other:?}"
            ),
        }
    }

    #[test]
    fn ext_ancestor_loose_eq_bails_not_false_green() {
        // FALSE GREEN, `==` operator path: `class MyEx extends \Exception {}` —
        // two fresh instances are NOT loosely equal in PHP (file/line/trace
        // differ), but two empty records compared {} == {} → true → a false
        // green on assertTrue. Construction must BAIL before `==` is reached.
        let dir = write_suite(&[(
            "ExtExTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class MyEx extends \Exception {}
class ExtExTest extends TestCase {
    public function testLooseEq(): void {
        $a = new MyEx();
        $b = new MyEx();
        $this->assertTrue($a == $b);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("ExtExTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Bailed(_)),
            "an ext-subclass instance must never reach `==` as an empty record; \
             got {reduced:?}"
        );
    }

    #[test]
    fn ext_ancestor_two_hops_up_still_bails() {
        // The unresolvable ancestor is the GRANDPARENT — the DECLARED extends
        // chain must be walked transitively (mago's `all_parent_classes` may
        // omit unknown parents, so each hop's declared name is checked).
        let dir = write_suite(&[(
            "ExtDeepTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class Mid extends \ArrayObject {}
class Leaf extends Mid {}
class ExtDeepTest extends TestCase {
    public function testEquals(): void {
        $this->assertEquals(new Leaf(), new Leaf());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("ExtDeepTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(
                reduced[0].outcome,
                Outcome::Bailed(BailReason::UnsupportedConstruct(_))
            ),
            "the unresolvable GRANDPARENT must bail construction too; got {reduced:?}"
        );
    }

    #[test]
    fn untyped_defaultless_prop_seeds_null_not_false_red() {
        // FALSE RED (gold-verified): PHP initializes an UNTYPED defaultless
        // `public $x;` to NULL. Leaving it unseeded made {x:null} vs {} a
        // prop-count mismatch → reducer Fail while real PHPUnit PASSES.
        // Seeding NULL is exact PHP semantics → Pass (EXACT, not a bail).
        let dir = write_suite(&[(
            "UntypedPropTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class Holder {
    public $x;
    public function __construct(int $w) { if ($w) { $this->x = null; } }
}
class UntypedPropTest extends TestCase {
    public function testEquals(): void {
        $this->assertEquals(new Holder(1), new Holder(0));
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("UntypedPropTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "an untyped defaultless property defaults to NULL in PHP — both \
             records are {{x:null}}; got {reduced:?}"
        );
    }

    #[test]
    fn typed_defaultless_prop_control_keeps_failing() {
        // Control (gold-aligned): a TYPED defaultless `public ?int $x;` is
        // UNINITIALIZED in PHP — absent from `==` and from the comparator
        // chain's property set on BOTH sides. It must NOT be seeded NULL:
        // {x:null} vs {} is a real FAIL in PHPUnit too (ObjectComparator's
        // toArray omits the uninitialized side). Current behavior is correct
        // and must stay byte-identical after the untyped-prop fix.
        let dir = write_suite(&[(
            "TypedPropTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class THolder {
    public ?int $x;
    public function __construct(int $w) { if ($w) { $this->x = null; } }
}
class TypedPropTest extends TestCase {
    public function testEquals(): void {
        $this->assertEquals(new THolder(1), new THolder(0));
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("TypedPropTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Fail(_)),
            "a typed defaultless property stays UNSET (gold-aligned Fail); \
             got {reduced:?}"
        );
    }

    const INHERITED_DEFAULT_SRC: &str = r#"<?php
class P { public $z = 7; }
class C extends P {
    public function __construct($v = null) { if ($v !== null) { $this->z = $v; } }
}
"#;

    #[test]
    fn inherited_default_assert_equals_now_exact() {
        // FALSE RED (gold-verified): `new C(7)` and `new C()` BOTH carry z=7 in
        // PHP (the default lives in parent P). Leaf-only seeding recorded
        // {z:7} vs {} → a structural prop-count mismatch → reducer Fail while
        // real PHPUnit PASSES. Chain seeding makes both records {z:7} → Pass
        // (EXACT, not a bail).
        let dir = write_suite(&[(
            "InheritedDefaultEqTest.php",
            &format!(
                r#"{INHERITED_DEFAULT_SRC}
use PHPUnit\Framework\TestCase;
class InheritedDefaultEqTest extends TestCase {{
    public function testEquals(): void {{
        $this->assertEquals(new C(7), new C());
    }}
}}
"#
            ),
        )]);
        let reduced = reduce_file(&dir.path().join("InheritedDefaultEqTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "an inherited class-property default must be seeded — both records \
             are {{z:7}} (real PHPUnit passes); got {reduced:?}"
        );
    }

    #[test]
    fn inherited_default_assert_not_equals_matches_phpunit_fail() {
        // FALSE GREEN (gold-verified, masks a real failure): real PHPUnit FAILS
        // assertNotEquals(new C(7), new C()) — both objects have z=7. The
        // unseeded {z:7} vs {} mismatch made the reducer report Pass. After
        // chain seeding both records are {z:7} → the reducer must Fail, which
        // MATCHES PHPUnit's Fail (a true red, 0 divergence).
        let dir = write_suite(&[(
            "InheritedDefaultNeqTest.php",
            &format!(
                r#"{INHERITED_DEFAULT_SRC}
use PHPUnit\Framework\TestCase;
class InheritedDefaultNeqTest extends TestCase {{
    public function testNotEquals(): void {{
        $this->assertNotEquals(new C(7), new C());
    }}
}}
"#
            ),
        )]);
        let reduced = reduce_file(&dir.path().join("InheritedDefaultNeqTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Fail(_)),
            "assertNotEquals on two z=7 records FAILS in real PHPUnit — the \
             reducer must match (never a false green); got {reduced:?}"
        );
    }

    const TRAIT_DEFAULT_SRC: &str = r#"<?php
trait HasX { public $x = 5; }
class A1 {
    use HasX;
    public function __construct($v = null) { if ($v !== null) { $this->x = $v; } }
}
"#;

    #[test]
    fn trait_default_assert_equals_now_exact() {
        // FALSE RED (gold-verified): a trait property is flattened into the
        // using class — `new A1(5)` and `new A1()` BOTH carry x=5 in PHP.
        // used_traits was consulted only for coercion HINTS, never for
        // seeding → {x:5} vs {} → reducer Fail while real PHPUnit PASSES.
        let dir = write_suite(&[(
            "TraitDefaultEqTest.php",
            &format!(
                r#"{TRAIT_DEFAULT_SRC}
use PHPUnit\Framework\TestCase;
class TraitDefaultEqTest extends TestCase {{
    public function testEquals(): void {{
        $this->assertEquals(new A1(5), new A1());
    }}
}}
"#
            ),
        )]);
        let reduced = reduce_file(&dir.path().join("TraitDefaultEqTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "a trait-declared property default must be seeded — both records \
             are {{x:5}} (real PHPUnit passes); got {reduced:?}"
        );
    }

    #[test]
    fn trait_default_assert_not_equals_matches_phpunit_fail() {
        // FALSE GREEN twin: real PHPUnit FAILS assertNotEquals(new A1(5),
        // new A1()) — both have x=5. The reducer's Fail after seeding MATCHES
        // PHPUnit's Fail (true red, 0 divergence).
        let dir = write_suite(&[(
            "TraitDefaultNeqTest.php",
            &format!(
                r#"{TRAIT_DEFAULT_SRC}
use PHPUnit\Framework\TestCase;
class TraitDefaultNeqTest extends TestCase {{
    public function testNotEquals(): void {{
        $this->assertNotEquals(new A1(5), new A1());
    }}
}}
"#
            ),
        )]);
        let reduced = reduce_file(&dir.path().join("TraitDefaultNeqTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(reduced[0].outcome, Outcome::Fail(_)),
            "assertNotEquals on two x=5 records FAILS in real PHPUnit — the \
             reducer must match (never a false green); got {reduced:?}"
        );
    }

    #[test]
    fn ancestor_used_trait_default_seeded() {
        // The trait is used by an ANCESTOR, not the leaf: PHP flattens it into
        // B1 and D1 inherits it — `new D1(5)` and `new D1()` both carry x=5.
        let dir = write_suite(&[(
            "AncestorTraitTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
trait HasY { public $x = 5; }
class B1 { use HasY; }
class D1 extends B1 {
    public function __construct($v = null) { if ($v !== null) { $this->x = $v; } }
}
class AncestorTraitTest extends TestCase {
    public function testEquals(): void {
        $this->assertEquals(new D1(5), new D1());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("AncestorTraitTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "an ancestor-used trait's property default must be seeded; got {reduced:?}"
        );
    }

    #[test]
    fn trait_untyped_defaultless_prop_seeds_null() {
        // Round-15's untyped-defaultless-NULL semantics must hold for a
        // TRAIT-declared property too: PHP initializes `public $u;` to NULL
        // wherever it is flattened from.
        let dir = write_suite(&[(
            "TraitNullTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
trait HasU { public $u; }
class HU {
    use HasU;
    public function __construct(int $w) { if ($w) { $this->u = null; } }
}
class TraitNullTest extends TestCase {
    public function testEquals(): void {
        $this->assertEquals(new HU(1), new HU(0));
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("TraitNullTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert_eq!(
            reduced[0].outcome,
            Outcome::Pass,
            "an untyped defaultless TRAIT property defaults to NULL in PHP — \
             both records are {{u:null}}; got {reduced:?}"
        );
    }

    #[test]
    fn absent_used_trait_construction_bails() {
        // The trait sibling of the ext-ancestor bail: a used trait absent from
        // the scanned codebase carries unmodelled state (its property set is
        // unknown) — construction must BAIL, never build a partial record.
        // (PHP itself fatals "Trait not found" at class-declaration time.)
        let dir = write_suite(&[(
            "AbsentTraitTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
class UsesAbsent {
    use \Vendor\Tools\Helper;
    public $k = 1;
}
class AbsentTraitTest extends TestCase {
    public function testEquals(): void {
        $this->assertEquals(new UsesAbsent(), new UsesAbsent());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("AbsentTraitTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        match &reduced[0].outcome {
            Outcome::Bailed(BailReason::UnsupportedConstruct(m)) => assert!(
                m.contains("not in the codebase"),
                "the bail must name the unresolvable trait; got {m:?}"
            ),
            other => panic!(
                "constructing a class that uses an unscanned trait must BAIL \
                 (trait state unmodelled), never a definitive verdict; got {other:?}"
            ),
        }
    }

    #[test]
    fn absent_nested_trait_construction_bails() {
        // The absent trait is one level down (trait-of-trait): the walk must
        // recurse, mirroring the transitive ext-ancestor walk.
        let dir = write_suite(&[(
            "AbsentNestedTraitTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
trait Outer { use \Vendor\Tools\Absent; }
class UsesOuter { use Outer; }
class AbsentNestedTraitTest extends TestCase {
    public function testEquals(): void {
        $this->assertEquals(new UsesOuter(), new UsesOuter());
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("AbsentNestedTraitTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        assert!(
            matches!(
                reduced[0].outcome,
                Outcome::Bailed(BailReason::UnsupportedConstruct(_))
            ),
            "an absent NESTED trait must bail construction too; got {reduced:?}"
        );
    }

    #[test]
    fn abstract_class_instantiation_bails() {
        // PHP throws Error "Cannot instantiate abstract class Shape" — the
        // test ERRORS in PHPUnit. Building a record instead let the test
        // reduce to a definitive Pass (false green). Bail, fail-closed.
        let dir = write_suite(&[(
            "AbstractNewTest.php",
            r#"<?php
use PHPUnit\Framework\TestCase;
abstract class Shape { public $sides = 0; }
class AbstractNewTest extends TestCase {
    public function testNew(): void {
        $s = new Shape();
        $this->assertSame(0, $s->sides);
    }
}
"#,
        )]);
        let reduced = reduce_file(&dir.path().join("AbstractNewTest.php")).unwrap();
        assert_eq!(reduced.len(), 1, "got {reduced:?}");
        match &reduced[0].outcome {
            Outcome::Bailed(BailReason::UnsupportedConstruct(m)) => assert!(
                m.contains("abstract"),
                "the bail must name the abstract-instantiation error; got {m:?}"
            ),
            other => panic!(
                "`new` on an abstract class ERRORS in PHP — the reducer must \
                 BAIL, never a definitive verdict; got {other:?}"
            ),
        }
    }
}
