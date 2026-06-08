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
    let root = test_file.parent().unwrap_or(Path::new("."));
    reduce_in_root(root, test_file)
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
/// and run the body.
#[allow(clippy::too_many_arguments)]
fn reduce_row(
    block: &Block,
    param_names: &[Vec<u8>],
    args: &[Value],
    this_value: Option<Value>,
    resolver: &BridgeResolver,
    names: &mago_names::ResolvedNames,
    source: &[u8],
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
    run_method_body_with_names(block, givens, resolver, names, source)
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
        let root = test_file
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(Path::new("."));
        if !test_file.exists() {
            eprintln!("SKIP: {} not present", test_file.display());
            return;
        }

        let reduced = reduce_in_root(root, test_file).expect("reduce_file");
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
        println!("--- bail histogram ---");
        for (reason, n) in &hist {
            println!("  {n:>3}  {reason}");
        }
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
}
