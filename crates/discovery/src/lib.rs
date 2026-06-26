//! Shared PHPUnit test discovery.
//!
//! Walks PHP source directories with tree-sitter, builds the class
//! inheritance graph, and emits one [`TestCase`] per discovered test method.
//! Recognises the `testXxx` naming convention, `/** @test */` PHPDoc,
//! `#[Test]` attribute, plus `#[DataProvider]` / `@dataProvider` markers
//! per method.
//!
//! Used by both the proust runner (for dispatch) and the analyzer
//! (for coverage tracing entry points).

use anyhow::{anyhow, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor};
use walkdir::WalkDir;

/// A single discovered test method: (class, method) plus the file it lives
/// in. `data_provider` is the name of the provider method declared via
/// `#[DataProvider("name")]` or `/** @dataProvider name */`, used by
/// downstream consumers to enumerate row counts and split heavy providers
/// across workers.
///
/// `groups` is the union of every group declared on the class and on the
/// method via `#[Group('name')]` and `/** @group name */`. The runner uses
/// it to filter out tests in groups excluded by `phpunit.xml`'s
/// `<groups><exclude>` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub file: PathBuf,
    pub class: String,
    pub method: String,
    pub data_provider: Option<String>,
    pub groups: Vec<String>,
    pub external_providers: Vec<(String, String)>,
    /// True when the method body consists entirely of trivially-true assertions
    /// and has at least one such assertion. See [`GroupedMethod::is_tautological`].
    pub is_tautological: bool,
    /// True when the class has no `setUpBeforeClass`/`tearDownAfterClass` override.
    /// See [`TestClass::has_lifecycle_overrides`].
    pub has_lifecycle_overrides: bool,
    /// Mirrors [`GroupedMethod::depends_on`].
    pub depends_on: Vec<String>,
    /// Mirrors [`GroupedMethod::is_dispatch_safe`].
    pub is_dispatch_safe: bool,
    /// Mirrors [`GroupedMethod::fingerprint`].
    pub fingerprint: std::collections::HashSet<String>,
    /// True when the test class calls a PHP API that mutates process-global
    /// state (stream wrapper registry, error handler, ini values, locale, …)
    /// without a reliable restore mechanism. The runner forces a fresh fork
    /// for each batch of such a class so cross-batch pollution can't carry
    /// over. See [`TestClass::is_stateful`].
    pub is_stateful: bool,
    /// True when the class (or any of its methods) carries a PHPUnit
    /// "run in separate process" marker — `@runInSeparateProcess`,
    /// `@runTestsInSeparateProcesses`, `@runClassInSeparateProcess`, or the
    /// equivalent PHP-8 attributes. Our worker is already a separate process
    /// per batch, so we satisfy PHPUnit's request by routing the class
    /// through K=1 (force_exit_after) and clearing the in-PHP flag before
    /// invoking the test — preventing PHPUnit from `proc_open`-ing a nested
    /// sub-process inside the worker. See [`TestClass::is_isolated`].
    pub is_isolated: bool,
    /// True when the test class (or any ancestor) requires a provisioned
    /// database. Detected via OPT-IN MARKERS ONLY: an in-class marker trait
    /// (`RefreshDatabase` / `DatabaseTransactions` by default) or a configured
    /// marker base-class (default list empty). No type-reference inference.
    /// Disjoint from `is_stateful` / `is_isolated` — never contributes to
    /// `must_force_exit`.
    pub needs_db: bool,
    /// True when this test's class or any ancestor extends a known functional
    /// base class (Symfony `KernelTestCase`/`WebTestCase`, etc.). A DECLARED
    /// marker, OR-folded down the inheritance chain. Used ONLY to tune the
    /// default worker count and suggest `--warmup` — never a correctness gate.
    pub is_functional: bool,
}

/// One class discovered during the pass-1 scan: enough information to build
/// the inheritance graph (pass 2) and emit test cases (pass 3) without
/// re-parsing the source.
#[derive(Debug, Clone)]
struct ParsedClass {
    file: PathBuf,
    /// Fully-qualified class name (namespace + "\\" + short name).
    fqcn: String,
    /// Resolved FQCN of the parent (post namespace + use-alias resolution),
    /// or `None` if the class has no `extends` clause.
    parent_fqcn: Option<String>,
    /// All public methods recognised as tests (by name, `@test` annotation,
    /// or `#[Test]` attribute). Each has its data-provider name (if any)
    /// and its set of `#[Group]`/`@group` annotations.
    test_methods: Vec<MethodInfo>,
    /// `#[Group]` and `@group` annotations on the class declaration itself;
    /// applied to every test method via union with method-level groups.
    class_groups: Vec<String>,
    /// `true` if the class is `abstract`. Abstract classes can never be
    /// instantiated by PHPUnit; we skip emitting test cases for them even
    /// when their chain reaches TestCase.
    is_abstract: bool,
    has_lifecycle_overrides: bool,
    /// True when ANY method in the class (test or otherwise: setUp,
    /// tearDown, helpers, setUpBeforeClass…) statically calls a
    /// process-global mutator like `stream_wrapper_register`,
    /// `set_error_handler`, `ini_set`, `setlocale`, etc. Such classes
    /// can't safely share a recycled worker with other batches because
    /// their global side effects bleed across tests.
    is_stateful: bool,
    /// True when the class is annotated with a PHPUnit "separate process"
    /// marker (class-level docblock, attribute, or any method-level
    /// equivalent). See [`TestCase::is_isolated`].
    is_isolated: bool,
    /// True when the class (or its trait/inheritance chain) requires a
    /// database clone. OR-folded down the chain in [`emit_test_cases`].
    needs_db: bool,
    /// True when THIS class's immediate parent matches a functional base-class
    /// marker (KernelTestCase/WebTestCase/…). OR-folded down the chain so a
    /// concrete test extending a non-functional-named intermediate that itself
    /// extends a functional base is still flagged. Perf/UX only (worker clamp +
    /// `--warmup` hint); never a correctness gate.
    is_functional: bool,
    /// True when this declaration is a `trait` (parsed for its test methods,
    /// folded into using classes; never emitted as a test class itself).
    is_trait: bool,
    /// Resolved FQCNs of traits this class/trait pulls in via an in-class
    /// `use Trait;` member. Their test methods (and flags) fold into using
    /// classes in [`emit_test_cases`], transitively for trait-of-trait.
    used_traits: Vec<String>,
    /// In-class `use SharedTransactionalFixture;` (the opt-in marker). Folded across the
    /// inheritance chain in [`shared_fixture_report`]. Advisory only — no runtime effect.
    uses_shared_fixture: bool,
    /// This class's OWN setUp calls a recognised DB fixture builder.
    setup_builds_fixture: bool,
    /// This class has a test/helper method using a rollback-incompatible construct.
    has_tx_disqualifier: bool,
    /// Way-3 setUp-hoist signals (advisory; `--report-hoistable-setup`). This class's
    /// own setUp `$this->P = RHS` candidates (prop, rhs, nondet), the ambient-context
    /// scopes its setUp establishes, and the properties its per-test methods mutate.
    /// Folded across the inheritance chain in [`setup_hoist_report`].
    setup_hoist_candidates: Vec<(String, String, bool)>,
    setup_ctx_scopes: Vec<String>,
    mutated_props: Vec<String>,
}

/// Per-method discovery info collected during the tree-sitter walk.
#[derive(Debug, Clone)]
struct MethodInfo {
    name: String,
    data_provider: Option<String>,
    groups: Vec<String>,
    external_providers: Vec<(String, String)>,
    is_tautological: bool,
    depends_on: Vec<String>,
    /// FQCNs statically referenced in the method body — `new Foo(...)`,
    /// `Foo::class`, `Foo::method()`, `createMock(Foo::class)`, etc. Used
    /// by the runner's slot-affinity dispatcher to route batches to workers
    /// that have already loaded matching classes (warm-cache routing).
    fingerprint: std::collections::HashSet<String>,
}

/// Maps every discovered class FQCN to its resolved parent FQCN (or None).
/// Used by the BFS to decide whether a class reaches TestCase.
type ClassGraph = HashMap<String, Option<String>>;

/// A test method within a discovered class, with its optional data-provider
/// reference. The runner uses `data_provider` to look up row counts and
/// schedule heavy methods first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedMethod {
    pub name: String,
    pub data_provider: Option<String>,
    pub groups: Vec<String>,
    pub external_providers: Vec<(String, String)>,
    /// True when the method body consists entirely of trivially-true assertions
    /// (assertTrue(true), assertFalse(false), assertNull(null), assertEquals(X,X),
    /// assertSame(X,X)) and has at least one such assertion. The runner can skip
    /// dispatching these to a PHP worker and emit a synthetic Pass outcome instead.
    pub is_tautological: bool,
    /// Names of methods this one depends on via `#[Depends('name')]` or
    /// `@depends name`. Empty when there are no declared dependencies.
    /// The runner uses this to group dependency chains together so
    /// return-value injection works correctly.
    pub depends_on: Vec<String>,
    /// True when `depends_on` is empty — the method has no ordering constraint
    /// and can be dispatched to any worker independently.
    pub is_dispatch_safe: bool,
    /// FQCNs statically referenced in the method body. See
    /// [`MethodInfo::fingerprint`]. Used by the runner's slot-affinity
    /// dispatcher for warm-cache routing across batches.
    pub fingerprint: std::collections::HashSet<String>,
}

/// A discovered test class with all of its methods, grouped for batched
/// dispatch (one request per class to the worker).
///
/// `has_lifecycle_overrides` is `true` when the class defines `setUpBeforeClass`
/// or `tearDownAfterClass`. Such hooks must run exactly once per class; dispatching
/// methods to separate workers would cause them to fire N times.
///
/// Per-method dispatch eligibility is checked per-method via
/// [`GroupedMethod::is_dispatch_safe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestClass {
    pub file: PathBuf,
    pub class: String,
    pub methods: Vec<GroupedMethod>,
    pub has_lifecycle_overrides: bool,
    /// True when the class statically references at least one PHP API that
    /// mutates process-global state without a guaranteed restore (stream
    /// wrapper registry, error/exception handler, ini values, locale, …).
    /// Such classes must run in an isolated fork (one batch per process)
    /// so cross-batch pollution can't bleed in. Detected at discovery by
    /// walking ALL method bodies in the class (including setUp/tearDown).
    pub is_stateful: bool,
    /// True when the class or any of its methods carries a PHPUnit
    /// "separate process" annotation/attribute. The runner forces K=1
    /// (force_exit_after) on these classes AND signals the PHP executor
    /// to clear `runTestInSeparateProcess` on the test instance so PHPUnit
    /// does not spawn a nested sub-process inside our already-forked worker.
    pub is_isolated: bool,
    /// True when the class (or any ancestor) requires a provisioned database.
    /// OR-folded from all per-method `TestCase::needs_db` values during
    /// grouping. Disjoint from `is_stateful`/`is_isolated`.
    pub needs_db: bool,
}

/// Group a flat list of TestCases by class. Preserves discovery order
/// and per-method data-provider attribution.
///
/// Groups by `(file, class)` rather than `class` alone so that the same
/// FQCN defined in multiple files (e.g. PHPUnit end-to-end fixture
/// sub-directories each with their own phpunit.xml context) produces
/// separate batches with the correct file and methods, rather than one
/// merged batch where the loaded class definition and the dispatched
/// method names disagree.
pub fn group_by_class(cases: Vec<TestCase>) -> Vec<TestClass> {
    let mut groups: Vec<TestClass> = Vec::new();
    for case in cases {
        let has_lifecycle_overrides = case.has_lifecycle_overrides;
        let is_stateful = case.is_stateful;
        let is_isolated = case.is_isolated;
        let needs_db = case.needs_db;
        let gm = GroupedMethod {
            name: case.method,
            data_provider: case.data_provider,
            groups: case.groups,
            external_providers: case.external_providers,
            is_tautological: case.is_tautological,
            is_dispatch_safe: case.is_dispatch_safe,
            depends_on: case.depends_on,
            fingerprint: case.fingerprint,
        };
        if let Some(existing) = groups
            .iter_mut()
            .find(|g| g.class == case.class && g.file == case.file)
        {
            existing.methods.push(gm);
            // Any TestCase from a stateful class makes the whole grouping
            // stateful — pollution is per-class, not per-method.
            existing.is_stateful = existing.is_stateful || is_stateful;
            // Likewise for isolation: a single method-level
            // @runInSeparateProcess promotes the whole class.
            existing.is_isolated = existing.is_isolated || is_isolated;
            // OR-fold: any DB-needing method makes the whole class DB-needing.
            existing.needs_db = existing.needs_db || needs_db;
        } else {
            groups.push(TestClass {
                file: case.file,
                class: case.class,
                methods: vec![gm],
                has_lifecycle_overrides,
                is_stateful,
                is_isolated,
                needs_db,
            });
        }
    }
    groups
}

/// True when the parsed tree contains any tree-sitter ERROR/missing node.
///
/// tree-sitter only returns `None` from `parse` on incompatible-language or
/// timeout; for syntactically broken (or grammar-unrecognised) PHP it returns
/// `Some(tree)` built via error recovery, with the bad region flagged on the
/// root. Callers use this to warn that the resulting partial tree may drop
/// classes/methods, rather than silently under-counting tests.
fn has_syntax_errors(tree: &tree_sitter::Tree) -> bool {
    tree.root_node().has_error()
}

/// Pass 1 (per file): parse a PHP source file and return every class
/// declaration in it, with its resolved parent FQCN and test methods.
///
/// "Resolved parent FQCN" applies the file's namespace + use-alias context so
/// the BFS in pass 3 can compare on FQCN strings alone.
fn parse_file_classes(path: &Path) -> Result<Vec<ParsedClass>> {
    // PHP source is byte-oriented: a file with a stray non-UTF-8 byte (latin-1
    // text, a binary blob in a heredoc) must still have its tests discovered,
    // not be silently dropped by callers' `unwrap_or_default()`. Read bytes and
    // decode lossily — only paying the copy (and warning) on the rare bad file.
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let src = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "proust: warning: {} contains non-UTF-8 bytes \u{2014} decoding lossily; some characters may be replaced",
                path.display()
            );
            String::from_utf8_lossy(e.as_bytes()).into_owned()
        }
    };

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_php::language_php())
        .context("setting tree-sitter-php language")?;
    let tree = parser
        .parse(&src, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {}", path.display()))?;

    // tree-sitter is error-recovering: a broken (or unrecognised) PHP file
    // still yields a *partial* tree, so classes/methods can be silently
    // dropped. Surface that to the user so the resulting test-count
    // discrepancy is observable rather than silent.
    if has_syntax_errors(&tree) {
        eprintln!(
            "proust: warning: tree-sitter found syntax errors in {} \u{2014} some tests may be missed",
            path.display()
        );
    }

    let root = tree.root_node();
    let bytes = src.as_bytes();

    let namespace = find_namespace(root, bytes);
    let aliases = parse_use_aliases(root, bytes);

    let mut out = Vec::new();
    collect_parsed_classes(root, bytes, namespace.as_deref(), &aliases, path, &mut out)?;
    Ok(out)
}

/// Parse `use Foo\Bar;` and `use Foo\Bar as Baz;` into a local-name → FQCN map.
/// Grouped uses (`use Foo\{Bar, Baz};`) are best-effort: tree-sitter-php
/// represents them with multiple `qualified_name` children we walk in order.
fn parse_use_aliases(root: Node, bytes: &[u8]) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        // Top-level `namespace_use_declaration` per PHP grammar.
        if child.kind() != "namespace_use_declaration" {
            continue;
        }
        let mut inner = child.walk();
        for clause in child.children(&mut inner) {
            if clause.kind() != "namespace_use_clause" {
                continue;
            }
            // The clause's first qualified_name (or name) is the imported FQCN;
            // an optional namespace_aliasing_clause supplies the alias.
            let mut imported: Option<String> = None;
            let mut alias: Option<String> = None;
            let mut clause_cur = clause.walk();
            for cc in clause.children(&mut clause_cur) {
                match cc.kind() {
                    "qualified_name" | "name" if imported.is_none() => {
                        imported = cc
                            .utf8_text(bytes)
                            .ok()
                            .map(|s| s.trim_start_matches('\\').to_string());
                    }
                    "namespace_aliasing_clause" => {
                        // Child is the alias name.
                        let mut acur = cc.walk();
                        for ac in cc.children(&mut acur) {
                            if ac.kind() == "name" {
                                alias = ac.utf8_text(bytes).ok().map(String::from);
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let Some(fqcn) = imported {
                // Default local name is the last segment of the FQCN.
                let local =
                    alias.unwrap_or_else(|| fqcn.rsplit('\\').next().unwrap_or(&fqcn).to_string());
                aliases.insert(local, fqcn);
            }
        }
    }
    aliases
}

/// Resolve a parent class name (as written after `extends`) into a FQCN,
/// using PHP's name-resolution rules: absolute names start with `\`;
/// relative names check use-aliases on their first segment, else prefix
/// with the current namespace.
fn resolve_class_reference(
    raw: &str,
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
) -> String {
    // Absolute: `\Foo\Bar` → `Foo\Bar`.
    if let Some(stripped) = raw.strip_prefix('\\') {
        return stripped.to_string();
    }
    let mut segments = raw.splitn(2, '\\');
    let first = segments.next().unwrap_or("");
    let rest = segments.next();
    if let Some(aliased_fqcn) = aliases.get(first) {
        match rest {
            Some(tail) => format!("{aliased_fqcn}\\{tail}"),
            None => aliased_fqcn.clone(),
        }
    } else if let Some(ns) = namespace {
        format!("{ns}\\{raw}")
    } else {
        raw.to_string()
    }
}

fn find_namespace(root: Node, bytes: &[u8]) -> Option<String> {
    // PHP: `namespace Foo\Bar;` produces a `namespace_definition` node.
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "namespace_definition" {
            if let Some(name) = child.child_by_field_name("name") {
                return name.utf8_text(bytes).ok().map(String::from);
            }
        }
    }
    None
}

/// For files with multiple `namespace Foo { ... }` blocks (braced form), find
/// the namespace that directly encloses the given class_declaration node.
/// Returns Some(namespace) when the class sits inside a braced namespace body,
/// None when it is at file level (the semicolon-form namespace applies instead).
fn find_enclosing_namespace(class_node: Node, bytes: &[u8]) -> Option<String> {
    let parent = class_node.parent()?;
    // Braced namespace body is a declaration_list (or compound_statement in some
    // grammar versions) that is itself a direct child of namespace_definition.
    if !matches!(parent.kind(), "declaration_list" | "compound_statement") {
        return None;
    }
    let ns_def = parent.parent()?;
    if ns_def.kind() != "namespace_definition" {
        return None;
    }
    ns_def
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(bytes).ok())
        .map(String::from)
}

/// For the semicolon form (`namespace Foo;`), a class belongs to the most recent
/// such declaration that PRECEDES it in source order — PHP applies a semicolon
/// namespace to all following top-level code until the next one. Braced blocks
/// (`namespace Foo { ... }`) are handled by [`find_enclosing_namespace`]; this
/// covers the sequential form, where a second `namespace Bar;` must NOT inherit
/// the file's first namespace (the bug behind monolog's co-located `Acme` helper
/// + the real test under `Monolog\Processor`).
fn find_preceding_semicolon_namespace(
    root: Node,
    bytes: &[u8],
    class_start: usize,
) -> Option<String> {
    let mut cursor = root.walk();
    let mut best: Option<(usize, String)> = None;
    for child in root.children(&mut cursor) {
        if child.kind() != "namespace_definition" || child.start_byte() >= class_start {
            continue;
        }
        // Semicolon form only: the braced form carries a declaration_list /
        // compound_statement body and is resolved by enclosure, not by position.
        let mut body = child.walk();
        let braced = child
            .children(&mut body)
            .any(|c| matches!(c.kind(), "declaration_list" | "compound_statement"));
        if braced {
            continue;
        }
        if let Some(name) = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
        {
            if best.as_ref().is_none_or(|(b, _)| child.start_byte() > *b) {
                best = Some((child.start_byte(), name.to_string()));
            }
        }
    }
    best.map(|(_, n)| n)
}

fn collect_parsed_classes(
    root: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
    path: &Path,
    out: &mut Vec<ParsedClass>,
) -> Result<()> {
    // Match BOTH classes and traits: PHPUnit runs test methods a class pulls in
    // via `use SomeTrait;`, so a trait body must be parsed too (its methods fold
    // into using classes in emit_test_cases; a trait is never run on its own).
    let query_src = r#"
        [
          (class_declaration
            name: (name) @class_name
            (base_clause [(name) (qualified_name)] @base)?
            body: (declaration_list) @body) @decl
          (trait_declaration
            name: (name) @class_name
            body: (declaration_list) @body) @decl
        ]
    "#;
    let lang = tree_sitter_php::language_php();
    let query = Query::new(&lang, query_src).context("compiling class query")?;
    let mut cursor = QueryCursor::new();
    let captures = query.capture_names();
    let class_idx = captures.iter().position(|n| *n == "decl").unwrap();
    let class_name_idx = captures.iter().position(|n| *n == "class_name").unwrap();
    let base_idx = captures.iter().position(|n| *n == "base").unwrap();
    let body_idx = captures.iter().position(|n| *n == "body").unwrap();

    // Functional base-class markers (defaults + PROUST_FUNCTIONAL_BASE_CLASSES),
    // built once per file. `reference_matches_marker` wants &[&str].
    let functional_markers = functional_base_markers();
    let functional_refs: Vec<&str> = functional_markers.iter().map(String::as_str).collect();

    for m in cursor.matches(&query, root, bytes) {
        let mut class_node: Option<Node> = None;
        let mut class_name: Option<&str> = None;
        let mut base_name: Option<&str> = None;
        let mut body_node: Option<Node> = None;
        for cap in m.captures {
            let idx = cap.index as usize;
            if idx == class_idx {
                class_node = Some(cap.node);
            } else if idx == class_name_idx {
                class_name = cap.node.utf8_text(bytes).ok();
            } else if idx == base_idx {
                base_name = cap.node.utf8_text(bytes).ok();
            } else if idx == body_idx {
                body_node = Some(cap.node);
            }
        }

        let (Some(name), Some(body), Some(decl)) = (class_name, body_node, class_node) else {
            continue;
        };

        // For multi-namespace files (namespace Foo { ... } braced form), each
        // class lives in the namespace of its own enclosing block, not the
        // first namespace encountered in the file.
        let class_ns: Option<String> = find_enclosing_namespace(decl, bytes)
            .or_else(|| find_preceding_semicolon_namespace(root, bytes, decl.start_byte()))
            .or_else(|| namespace.map(String::from));
        let fqcn = match class_ns.as_deref() {
            Some(ns) => format!("{ns}\\{name}"),
            None => name.to_string(),
        };
        let parent_fqcn =
            base_name.map(|b| resolve_class_reference(b, class_ns.as_deref(), aliases));
        // Functional iff THIS class's immediate parent matches a marker; the
        // chain-fold in emit_test_cases propagates it through intermediates.
        let is_functional = parent_fqcn
            .as_deref()
            .is_some_and(|p| reference_matches_marker(p, &functional_refs));
        let test_methods = collect_test_methods(body, bytes, class_ns.as_deref(), aliases);
        // A trait: parsed for its test methods (folded into using classes), but
        // never emitted as a test class itself. Its `use OtherTrait;` members are
        // captured so trait-of-trait composition folds transitively.
        let is_trait = decl.kind() == "trait_declaration";
        let used_traits = collect_used_traits(body, bytes, class_ns.as_deref(), aliases);
        let is_abstract = class_has_modifier(decl, bytes, "abstract");

        // Class-level groups apply to every test method in the class.
        // Collect both attribute and PHPDoc forms; the docblock immediately
        // preceding the class declaration is its docblock (tree-sitter
        // attaches it to the class_declaration node's preceding `comment`
        // sibling).
        let mut class_groups = extract_groups_attr(decl, bytes);
        if let Some(doc) = preceding_docblock(decl, bytes) {
            extract_groups_phpdoc(&doc, &mut class_groups);
        }

        let has_lifecycle_overrides = !has_no_lifecycle_overrides(body, bytes);
        let is_stateful = class_has_stateful_calls(body, bytes);
        let is_isolated = class_has_run_in_separate_process(decl, bytes);
        let needs_db = class_needs_db(
            decl,
            body,
            bytes,
            DEFAULT_DB_MARKER_TRAITS,
            DEFAULT_DB_MARKER_BASE_CLASSES,
        );
        let uses_shared_fixture = class_uses_shared_fixture(body, bytes);
        let (setup_builds_fixture, has_tx_disqualifier) =
            scan_tx_eligibility_signals(body, bytes, &test_methods);
        let (setup_hoist_candidates, setup_ctx_scopes, mutated_props) =
            scan_hoist_signals(body, bytes);

        out.push(ParsedClass {
            file: path.to_path_buf(),
            fqcn,
            parent_fqcn,
            test_methods,
            class_groups,
            is_abstract,
            has_lifecycle_overrides,
            is_stateful,
            is_isolated,
            needs_db,
            is_functional,
            is_trait,
            used_traits,
            uses_shared_fixture,
            setup_builds_fixture,
            has_tx_disqualifier,
            setup_hoist_candidates,
            setup_ctx_scopes,
            mutated_props,
        });
    }
    Ok(())
}

/// Returns true if the class declaration has the named modifier (e.g. "abstract", "final").
/// Tree-sitter-php represents modifiers as plain `name`-like children of the class_declaration.
fn class_has_modifier(class_decl: Node, bytes: &[u8], wanted: &str) -> bool {
    let mut cursor = class_decl.walk();
    for child in class_decl.children(&mut cursor) {
        let kind = child.kind();
        if kind == "abstract_modifier" || kind == "final_modifier" || kind == "readonly_modifier" {
            if let Ok(text) = child.utf8_text(bytes) {
                if text.eq_ignore_ascii_case(wanted) {
                    return true;
                }
            }
        }
    }
    false
}

// ── setUp-stateless helpers ────────────────────────────────────────────────

/// True if the class body does NOT declare `setUpBeforeClass` or
/// `tearDownAfterClass`. These are class-level lifecycle hooks that run once
/// per class (not once per test); splitting such a class into per-method plans
/// would either skip the hook entirely or run it once per method (wrong either way).
fn has_no_lifecycle_overrides(class_body: Node, src: &[u8]) -> bool {
    let mut cursor = class_body.walk();
    for child in class_body.children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .unwrap_or("");
        if matches!(name, "setUpBeforeClass" | "tearDownAfterClass") {
            return false;
        }
    }
    true
}

/// Returns `true` when the method body consists *entirely* of trivially-true
/// PHPUnit assertions and contains at least one of them:
///
/// - `$this->assertTrue(true)`
/// - `$this->assertFalse(false)`
/// - `$this->assertNull(null)`
/// - `$this->assertEquals(X, X)` — both args are identical PHP literals
/// - `$this->assertSame(X, X)` — both args are identical PHP literals
///
/// Any non-assertion statement (assignment, if, foreach, return, …) or a
/// method body with zero assertions causes this function to return `false`.
fn is_tautological_method(method_node: Node, src: &[u8]) -> bool {
    let body = match method_node.child_by_field_name("body") {
        Some(b) => b,
        None => return false,
    };

    let mut found_assertion = false;
    let mut cursor = body.walk();

    for child in body.children(&mut cursor) {
        match child.kind() {
            "{" | "}" | "comment" => continue,
            "expression_statement" => {
                // The inner expression must be a $this->assertXxx(...) call.
                let expr = match child.child(0) {
                    Some(e) => e,
                    None => return false,
                };
                if expr.kind() != "member_call_expression" {
                    return false;
                }
                // object must be $this
                let object = match expr.child_by_field_name("object") {
                    Some(o) => o,
                    None => return false,
                };
                let object_text = match object.utf8_text(src) {
                    Ok(t) => t,
                    Err(_) => return false,
                };
                if object_text != "$this" {
                    return false;
                }
                // method name
                let method_name_node = match expr.child_by_field_name("name") {
                    Some(n) => n,
                    None => return false,
                };
                let method_name = match method_name_node.utf8_text(src) {
                    Ok(t) => t,
                    Err(_) => return false,
                };
                // arguments node
                let args_node = match expr.child_by_field_name("arguments") {
                    Some(a) => a,
                    None => return false,
                };

                // Collect actual argument nodes (skip punctuation: `(`, `)`, `,`)
                let mut args: Vec<Node> = Vec::new();
                let mut args_cursor = args_node.walk();
                for arg in args_node.children(&mut args_cursor) {
                    match arg.kind() {
                        "(" | ")" | "," => continue,
                        "argument" => {
                            // argument node wraps the actual expression
                            if let Some(inner) = arg.child(0) {
                                args.push(inner);
                            } else {
                                args.push(arg);
                            }
                        }
                        _ => args.push(arg),
                    }
                }

                let trivial = match method_name {
                    "assertTrue" => args.len() == 1 && matches!(args[0].utf8_text(src), Ok("true")),
                    "assertFalse" => {
                        args.len() == 1 && matches!(args[0].utf8_text(src), Ok("false"))
                    }
                    "assertNull" => args.len() == 1 && matches!(args[0].utf8_text(src), Ok("null")),
                    "assertEquals" | "assertSame" => {
                        if args.len() == 2 {
                            let a = &args[0];
                            let b = &args[1];
                            // Both args must be PHP literal nodes (not just textually equal)
                            let both_literals = matches!(
                                a.kind(),
                                "integer"
                                    | "float"
                                    | "string"
                                    | "encapsed_string"
                                    | "boolean"
                                    | "true"
                                    | "false"
                                    | "null"
                            ) && matches!(
                                b.kind(),
                                "integer"
                                    | "float"
                                    | "string"
                                    | "encapsed_string"
                                    | "boolean"
                                    | "true"
                                    | "false"
                                    | "null"
                            );
                            both_literals
                                && a.utf8_text(src).unwrap_or("a")
                                    == b.utf8_text(src).unwrap_or("b")
                        } else {
                            false
                        }
                    }
                    _ => return false,
                };

                if !trivial {
                    return false;
                }
                found_assertion = true;
            }
            _ => return false,
        }
    }

    found_assertion
}

/// Walk a method body and collect every fully-qualified class name (FQCN)
/// it statically references. Used by the runner's slot-affinity dispatch
/// to route tests with similar class footprints to the same worker.
///
/// Sources of references:
///   - `new Foo()` / `new \Some\Foo()`            → object_creation_expression
///   - `Foo::class`, `Foo::CONST`, `Foo::method()` → class_constant_access_expression
///     / scoped_call_expression
///     / scoped_property_access_expression
///   - `instanceof Foo`                            → binary_expression with `instanceof`
///   - `createMock(Foo::class)` / `createStub(...)` / `getMockBuilder(...)`
///     are subsumed by `Foo::class` resolution above.
///
/// Bare names are resolved through `aliases` (the file's `use` map) and the
/// enclosing namespace. Names starting with `\` are absolute; we strip the
/// leading backslash for normalisation. Unresolved short names (e.g. PHP
/// builtins like `DateTime`, `stdClass`) are dropped since they don't
/// influence per-worker caching meaningfully.
fn extract_method_fingerprint(
    method_node: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
) -> std::collections::HashSet<String> {
    let mut result: std::collections::HashSet<String> = std::collections::HashSet::new();
    let body = match method_node.child_by_field_name("body") {
        Some(b) => b,
        None => return result,
    };
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        // Recurse into all named children. Tree-sitter PHP nests deeply so we
        // iterate in DFS using an explicit stack to avoid recursion overhead.
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            stack.push(child);
        }
        // Identify class references by node kind.
        let class_name_text: Option<&str> = match n.kind() {
            // `new Foo(...)` — the type sits in `name_node`/`type` field
            "object_creation_expression" => n
                .child_by_field_name("type")
                .or_else(|| n.named_child(0))
                .and_then(|c| c.utf8_text(bytes).ok()),
            // `Foo::class` / `Foo::CONST` — first named child is the class name
            "class_constant_access_expression"
            | "scoped_call_expression"
            | "scoped_property_access_expression" => {
                n.named_child(0).and_then(|c| c.utf8_text(bytes).ok())
            }
            // `expr instanceof Foo` — Right operand if the op is `instanceof`
            "binary_expression" => {
                let op = n
                    .child_by_field_name("operator")
                    .and_then(|c| c.utf8_text(bytes).ok())
                    .unwrap_or("");
                if op == "instanceof" {
                    n.child_by_field_name("right")
                        .and_then(|c| c.utf8_text(bytes).ok())
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(raw) = class_name_text {
            if !is_valid_class_name(raw) {
                continue;
            }
            let resolved = resolve_class_reference(raw, namespace, aliases);
            // Only retain names that look like a FQCN (contain a backslash) —
            // bare unresolved names match PHP builtins and add noise.
            if resolved.contains('\\') {
                result.insert(resolved);
            }
        }
    }
    result
}

/// Reject anything that isn't a valid PHP identifier-shaped class reference.
/// Catches:
///   * PHP pseudo-classes (`self`, `static`, `parent`) that look like names
///     to tree-sitter but get bogusly namespace-prefixed by the resolver.
///   * Tree-sitter capture of literal content from `scoped_call_expression`
///     subtrees where the "scope" was actually an array / string / call
///     expression rather than a class name (produces multi-line strings
///     starting with `(`, `[`, `'`, `"`).
///
/// A real class reference is a single line of ASCII identifier characters
/// plus optional namespace separators (`\\`).
fn is_valid_class_name(raw: &str) -> bool {
    if matches!(raw, "self" | "static" | "parent") {
        return false;
    }
    if raw.is_empty() {
        return false;
    }
    if raw.contains('\n') || raw.contains('\r') {
        return false;
    }
    // First char must be a letter, underscore, or backslash (absolute name).
    let first = raw.chars().next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_' || first == '\\') {
        return false;
    }
    // Body must be identifier chars and namespace separators only.
    raw.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\\')
}

/// Bare PHP function names that mutate process-global state without a
/// reliable per-test restore. A class that calls any of these from any
/// of its methods — test, setUp, helper, doesn't matter — gets marked
/// `is_stateful` so the runner forks a fresh worker for each of its
/// batches. The list is intentionally conservative: a false positive
/// just costs perf (extra forks), a false negative costs parity.
const STATEFUL_GLOBAL_APIS: &[&str] = &[
    // Stream wrappers (the canonical guzzle-psr7 case)
    "stream_wrapper_register",
    "stream_wrapper_unregister",
    "stream_wrapper_restore",
    "stream_register_wrapper", // pre-5.1 alias still in some codebases
    // Error / exception handlers
    "set_error_handler",
    "restore_error_handler",
    "set_exception_handler",
    "restore_exception_handler",
    // ini / env / locale
    "ini_set",
    "putenv",
    "setlocale",
    "date_default_timezone_set",
    "mb_internal_encoding",
    "mb_regex_encoding",
    // Autoload chain
    "spl_autoload_register",
    "spl_autoload_unregister",
];

/// Walk every method body in a class (test methods AND helpers like setUp,
/// tearDown, setUpBeforeClass) and return true if at least one of them
/// statically calls one of [`STATEFUL_GLOBAL_APIS`].
///
/// This is intentionally syntactic, not semantic — we don't reason about
/// whether the call is conditional, restored later, or behind an
/// `if (false)`. The runner pays for any positive match by forking a
/// fresh worker per batch; the cost is much smaller than running a
/// polluting suite with K=20 recycling and silently losing tests.
///
/// # Known blind spots
///
/// It scans the class's OWN method bodies (and, via the chain walk, its
/// ancestors'). It does **not** see state mutated through a `use`d trait
/// method, a called free function/helper, a static-property accumulator, or a
/// DI-container / global registry. A class that pollutes global state only
/// through one of those is not flagged `is_stateful`, so it can still leak
/// across a recycled worker — force isolation explicitly when that bites
/// (`PROUST_NO_ISOLATION`, or run such a suite with `--workers 1`).
fn class_has_stateful_calls(class_body: Node, bytes: &[u8]) -> bool {
    let mut cursor = class_body.walk();
    for member in class_body.children(&mut cursor) {
        if member.kind() != "method_declaration" {
            continue;
        }
        let Some(body) = member.child_by_field_name("body") else {
            continue;
        };
        let mut stack = vec![body];
        while let Some(n) = stack.pop() {
            let mut c2 = n.walk();
            for child in n.named_children(&mut c2) {
                stack.push(child);
            }
            if n.kind() == "function_call_expression" {
                let fn_node = n
                    .child_by_field_name("function")
                    .or_else(|| n.named_child(0));
                if let Some(fnn) = fn_node {
                    if let Ok(name) = fnn.utf8_text(bytes) {
                        let bare = name.trim_start_matches('\\');
                        if STATEFUL_GLOBAL_APIS.contains(&bare) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Returns true if the class declaration (its preceding docblock OR any
/// content inside the class — method docblocks, attributes, class-level
/// attributes) carries a PHPUnit "run in separate process" marker.
///
/// We over-isolate at the class level: a single method annotated with
/// `@runInSeparateProcess` promotes the whole class to isolated. The cost
/// is at worst extra force_exit cycles for a class that mixes annotated and
/// non-annotated methods (rare); the benefit is avoiding PHPUnit spawning a
/// nested `proc_open()` sub-process inside our already-forked worker — which
/// hangs on FD inheritance and was the root cause of phpunit-itself stalling.
///
/// Detection is purely textual: the markers are distinctive identifiers
/// that only appear in docblock comments (`/** @runInSeparateProcess */`)
/// or attribute names (`#[RunInSeparateProcess]`). They can't appear in
/// executable code without being a syntax error, so a `contains()` scan
/// is correct in practice. Substring overlap between
/// `RunInSeparateProcess` / `RunTestsInSeparateProcesses` /
/// `RunClassInSeparateProcess` is none — none is a substring of another.
fn class_has_run_in_separate_process(class_decl: Node, bytes: &[u8]) -> bool {
    // PHPDoc forms (PHPUnit ≤ 9 / legacy)
    const PHPDOC_MARKERS: &[&str] = &[
        "@runInSeparateProcess",
        "@runTestsInSeparateProcesses",
        "@runClassInSeparateProcess",
    ];
    // PHP-8 attribute names (PHPUnit 10+). Class-level
    // `RunClassInSeparateProcess` is the strict official name (singular).
    const ATTR_MARKERS: &[&str] = &[
        "RunInSeparateProcess",
        "RunTestsInSeparateProcesses",
        "RunClassInSeparateProcess",
    ];

    let scan = |text: &str| -> bool {
        PHPDOC_MARKERS.iter().any(|m| text.contains(m))
            || ATTR_MARKERS.iter().any(|m| text.contains(m))
    };

    // Class-level docblock lives as a preceding sibling of the
    // class_declaration node — pulled by the existing helper.
    if let Some(doc) = preceding_docblock(class_decl, bytes) {
        if scan(&doc) {
            return true;
        }
    }
    // Everything inside the class — class-level attribute groups,
    // method-level attributes, method docblock comments — is part of
    // class_decl's text. One scan covers all the in-class cases.
    if let Ok(text) = class_decl.utf8_text(bytes) {
        if scan(text) {
            return true;
        }
    }
    false
}

/// Default marker traits that signal a test needs a transactional database.
/// Configurable; threaded as `&[&str]` so the signature is stable. An in-class
/// `use RefreshDatabase;` trait-use member is the tight signal.
const DEFAULT_DB_MARKER_TRAITS: &[&str] = &["RefreshDatabase", "DatabaseTransactions"];

/// Default marker base-classes that signal a test needs a database — matched
/// against the `extends` target. EMPTY ON PURPOSE: a default base-class would
/// re-flag whole suites (e.g. doctrine-orm, where 850 files extend a common
/// ORM test base) and, under the later fail-fast policy, abort a run that does
/// not actually need a DB. Users opt in by adding their own functional-test
/// base class to this list.
const DEFAULT_DB_MARKER_BASE_CLASSES: &[&str] = &[];

/// Base classes whose presence anywhere in a test's `extends` chain marks the
/// suite as "functional": it boots a framework kernel, so each parallel worker
/// pays a high one-time fixed cost (a cold container/kernel boot ~90ms, plus a
/// per-worker DB clone when provisioned). The runner uses this — a DECLARED
/// MARKER, never type-reference inference — only to pick a more conservative
/// default worker count and to suggest `--warmup`; it has NO correctness effect,
/// so (unlike the deliberately-empty DB base-class list) a default list is safe.
const DEFAULT_FUNCTIONAL_BASE_CLASSES: &[&str] = &[
    "KernelTestCase",
    "WebTestCase",
    "ApiTestCase",
    "PantherTestCase",
];

/// Active functional base-class markers: the defaults plus any names in the
/// comma-separated `PROUST_FUNCTIONAL_BASE_CLASSES` env var, so a project with a
/// custom functional base (e.g. its own `IntegrationTestCase`) can opt in with
/// no code change. Read once per file-parse (env is process-global; tests
/// exercise the pure [`merge_functional_markers`] core instead of mutating it).
fn functional_base_markers() -> Vec<String> {
    merge_functional_markers(
        std::env::var("PROUST_FUNCTIONAL_BASE_CLASSES")
            .ok()
            .as_deref(),
    )
}

/// Pure core of [`functional_base_markers`]: defaults + comma-separated extras
/// (trimmed, blanks dropped). Order is defaults-first; duplicates are harmless
/// because matching is membership, not position.
fn merge_functional_markers(env: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = DEFAULT_FUNCTIONAL_BASE_CLASSES
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(extra) = env {
        v.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
        );
    }
    v
}

/// Whole-token / last-segment match of one identifier reference (a trait name
/// or a base-class name pulled from a tree-sitter node — never comment or
/// string text) against a configured marker list.
///
/// After stripping a leading `\`, a reference matches an entry when the entry
/// equals the whole reference OR its last `\`-segment. So `RefreshDatabase`
/// matches both `use RefreshDatabase;` and `use Illuminate\Foundation\Testing\RefreshDatabase;`,
/// while a partial identifier (e.g. `RefreshDatabaseState`) does NOT match.
/// The reference is validated as identifier-shaped by the caller, so it
/// carries no comment/string noise.
fn reference_matches_marker(reference: &str, markers: &[&str]) -> bool {
    let reference = reference.trim_start_matches('\\');
    if reference.is_empty() {
        return false;
    }
    let last_seg = reference.rsplit('\\').next().unwrap_or(reference);
    markers.iter().any(|m| reference == *m || last_seg == *m)
}

/// Returns true when a class needs a provisioned database. Detection is
/// OPT-IN MARKERS ONLY — there is deliberately no type-reference inference:
///   1. an in-class `use <MarkerTrait>;` trait-use member whose name matches
///      `marker_traits` (NOT a file-level `namespace_use_declaration` import),
///   2. an `extends <MarkerBaseClass>` whose base-class name matches
///      `marker_base_classes`.
///
/// Both are matched as whole tokens / last namespace segment (see
/// [`reference_matches_marker`]), so a partial identifier never trips
/// detection. An imported-but-unused `use Foo\RefreshDatabase;` at file scope
/// must NOT trip detection — only the in-class trait-use member counts.
///
/// Mirrors the structure of `class_has_stateful_calls` (walks `class_body`)
/// and `class_has_run_in_separate_process` (scans `class_decl`). The flag
/// ONLY ever sets `needs_db` (provision + isolate) — it must NEVER set
/// `must_force_exit`.
fn class_needs_db(
    class_decl: Node,
    class_body: Node,
    bytes: &[u8],
    marker_traits: &[&str],
    marker_base_classes: &[&str],
) -> bool {
    // 1. `extends <MarkerBaseClass>` — match the base type name as a whole
    // token from the `base_clause`, not from raw text. (Default list empty.)
    if !marker_base_classes.is_empty() {
        let mut decl_cursor = class_decl.walk();
        for child in class_decl.children(&mut decl_cursor) {
            if child.kind() == "base_clause" {
                let mut base_cursor = child.walk();
                for base in child.named_children(&mut base_cursor) {
                    if matches!(base.kind(), "name" | "qualified_name") {
                        if let Ok(raw) = base.utf8_text(bytes) {
                            if is_valid_class_name(raw)
                                && reference_matches_marker(raw, marker_base_classes)
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    // 2. `use <MarkerTrait>;` as a trait-use member inside the class body.
    // A `use_declaration` here is the in-class trait-use list; a file-scope
    // import is a `namespace_use_declaration` and never reaches this loop.
    let mut cursor = class_body.walk();
    for member in class_body.children(&mut cursor) {
        if member.kind() != "use_declaration" {
            continue;
        }
        let mut tu_cursor = member.walk();
        for used in member.named_children(&mut tu_cursor) {
            if matches!(used.kind(), "name" | "qualified_name") {
                if let Ok(raw) = used.utf8_text(bytes) {
                    if is_valid_class_name(raw) && reference_matches_marker(raw, marker_traits) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Calls in setUp signalling an expensive deterministic DB fixture. Port of the analyzer
/// eligibility list (`crates/analyzer/src/reduce/eligibility.rs`); kept in sync by the oracle
/// integration test. Matched as `<builder>(` substrings of the setUp source.
const TX_FIXTURE_BUILDERS: &[&str] = &[
    "createSchema",
    "createSchemaForModels",
    "setUpEntitySchema",
    "getEntityManager",
    "getSchemaTool",
    "createSchemaManager",
];

/// Substrings in a TEST method that a per-test rollback cannot soundly undo. Lowercased: the
/// source is lowercased before matching, so `rollBack`/`rollback` both hit `->rollback(`.
const TX_DISQUALIFIERS: &[&str] = &[
    "->commit(",
    "->begintransaction(",
    "->rollback(",
    "->createsavepoint(",
    "->createschema(",
    "->dropschema(",
    "->dropdatabase(",
    "expectexception",
    "@depends",
];

/// Does this setUp method's source call a recognised fixture builder?
fn setup_builds_fixture_src(src: &str) -> bool {
    TX_FIXTURE_BUILDERS
        .iter()
        .any(|b| src.contains(&format!("{b}(")))
}

/// The first disqualifier substring in a test method's source (lowercased match), if any.
fn method_tx_disqualifier_src(src: &str) -> Option<String> {
    let low = src.to_ascii_lowercase();
    TX_DISQUALIFIERS
        .iter()
        .find(|d| low.contains(**d))
        .map(|d| (*d).to_string())
}

/// Lifecycle method names (scanned for the fixture BUILDER, never for disqualifiers — they
/// are not tests). Mirrors the analyzer eligibility `is_lifecycle`.
fn is_tx_lifecycle(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "setup" | "setupbeforeclass" | "teardown" | "teardownafterclass"
    )
}

/// Does the class body contain an IN-CLASS `use SharedTransactionalFixture;` trait-use?
/// Mirrors `class_needs_db`'s in-class scan: a `use_declaration` inside `class_body` is the
/// trait-use list; a file-scope import is a `namespace_use_declaration` and never reaches here.
fn class_uses_shared_fixture(class_body: Node, bytes: &[u8]) -> bool {
    let mut cursor = class_body.walk();
    for member in class_body.children(&mut cursor) {
        if member.kind() != "use_declaration" {
            continue;
        }
        let mut tu_cursor = member.walk();
        for used in member.named_children(&mut tu_cursor) {
            if matches!(used.kind(), "name" | "qualified_name") {
                if let Ok(raw) = used.utf8_text(bytes) {
                    if is_valid_class_name(raw)
                        && reference_matches_marker(raw, &["SharedTransactionalFixture"])
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Resolved FQCNs of the traits a class/trait pulls in via in-class `use Trait;`
/// members (NOT file-scope `namespace_use_declaration` imports). Multi-name
/// `use A, B;` and the adaptation form `use A { B::m insteadof C; }` both yield
/// their trait names; the `insteadof`/`as` adaptations are not modelled (rare).
fn collect_used_traits(
    class_body: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = class_body.walk();
    for member in class_body.children(&mut cursor) {
        if member.kind() != "use_declaration" {
            continue;
        }
        let mut tu_cursor = member.walk();
        for used in member.named_children(&mut tu_cursor) {
            if matches!(used.kind(), "name" | "qualified_name") {
                if let Ok(raw) = used.utf8_text(bytes) {
                    if is_valid_class_name(raw) {
                        out.push(resolve_class_reference(raw, namespace, aliases));
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Way-3 setUp-hoist eligibility (read-only advisory; --report-hoistable-setup).
// Ports prototypes/test-compiler/way3/way3_setup.js: decide whether a class's
// `setUp` builds a DETERMINISTIC, IMMUTABLE fixture whose construction could be
// hoisted to run ONCE (setUpBeforeClass) instead of once-per-test. Two gates:
//   (1) determinism + context: no non-deterministic factory call, and no
//       per-test ambient-context setter (tz/now/locale) anywhere in the setUp
//       chain (a context setter at per-test scope would make a once-computed
//       value silently wrong);
//   (2) no mutation: no test/teardown method mutates the shared property.
// String-scanned (no regex dep), matching the existing src-predicate helpers.
// ---------------------------------------------------------------------------

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True if `src` calls `name` as a whole identifier (case-insensitive): `name`
/// preceded by a non-identifier byte (or start) and followed by optional
/// whitespace then `(`. Catches `name(`, `->name(`, `::name(`, `\name(`, but NOT
/// `name` embedded in a longer identifier (so `now(` does not match `setTestNow(`).
fn has_call_ci(src: &str, name: &str) -> bool {
    let s = src.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    let (sb, nb) = (s.as_bytes(), n.as_bytes());
    if nb.is_empty() {
        return false;
    }
    let mut i = 0;
    while let Some(pos) = s[i..].find(&n) {
        let at = i + pos;
        let before_ok = at == 0 || !is_ident_byte(sb[at - 1]);
        let mut j = at + nb.len();
        while j < sb.len() && matches!(sb[j], b' ' | b'\t' | b'\n' | b'\r') {
            j += 1;
        }
        if before_ok && j < sb.len() && sb[j] == b'(' {
            return true;
        }
        i = at + 1;
    }
    false
}

/// Per-test ambient-context setters; presence in a setUp body means the hoist
/// slot would run under a different context → REFUSE every candidate.
const HOIST_CONTEXT_SETTERS: &[(&str, &str)] = &[
    ("date_default_timezone_set", "tz"),
    ("setTestNow", "now"),
    ("setTestNowAndTimezone", "now"),
    ("setlocale", "locale"),
];

/// Non-deterministic factory calls: a setUp RHS calling any of these can't be
/// hoisted (its value would differ across runs/tests).
const HOIST_NONDET_CALLS: &[&str] = &[
    "rand",
    "mt_rand",
    "random_int",
    "time",
    "microtime",
    "uniqid",
    "hrtime",
    "now",
    "today",
];

/// Reader-method name prefixes: a `$this->P->reader(...)` call does NOT mutate P.
const HOIST_READER_PREFIXES: &[&str] = &[
    "get",
    "is",
    "has",
    "to",
    "as",
    "with",
    "count",
    "equals",
    "compare",
    "fingerprint",
    "tables",
    "tablecount",
    "toarray",
    "jsonserialize",
];

/// The ambient-context scopes (tz/now/locale) a setUp body establishes.
fn setup_ctx_scopes(setup_src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (needle, label) in HOIST_CONTEXT_SETTERS {
        if has_call_ci(setup_src, needle) && !out.iter().any(|x| x == label) {
            out.push((*label).to_string());
        }
    }
    out
}

/// Extract `$this->PROP = RHS;` direct assignments from a method body source,
/// returning (prop, rhs_excerpt, nondet). Skips `==`, compound assigns, and
/// indirect targets (`$this->P->x =`, `$this->P[...] =`) — only a bare
/// `$this->PROP =` is a candidate (matches the prototype's reAssign).
fn setup_hoist_candidates_src(setup_src: &str) -> Vec<(String, String, bool)> {
    let bytes = setup_src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(pos) = setup_src[i..].find("$this->") {
        let start = i + pos;
        let mut j = start + "$this->".len();
        let prop_start = j;
        while j < bytes.len() && is_ident_byte(bytes[j]) {
            j += 1;
        }
        let prop = &setup_src[prop_start..j];
        i = start + 1;
        if prop.is_empty() {
            continue;
        }
        // skip whitespace after the property
        let mut k = j;
        while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r') {
            k += 1;
        }
        // must be a bare `=` (not `==`, not `=>`, not part of `->`/`[`)
        if k >= bytes.len() || bytes[k] != b'=' {
            continue;
        }
        if k + 1 < bytes.len() && matches!(bytes[k + 1], b'=' | b'>') {
            continue;
        }
        // RHS up to the next `;`
        let rhs_start = k + 1;
        let Some(semi) = setup_src[rhs_start..].find(';') else {
            continue;
        };
        let rhs = setup_src[rhs_start..rhs_start + semi].trim();
        let nondet = HOIST_NONDET_CALLS.iter().any(|c| has_call_ci(rhs, c));
        out.push((prop.to_string(), rhs.to_string(), nondet));
    }
    out
}

/// Property names `$this->P` that a (test/teardown) method body MUTATES: a
/// non-reader call `$this->P->m(...)`, or a write/reassign (`$this->P =`,
/// `$this->P->x =`, `$this->P[...] =`). Mirrors the prototype's `mutates()`.
fn body_mutated_props_src(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let push = |p: &str, out: &mut Vec<String>| {
        if !p.is_empty() && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    };
    let mut i = 0;
    while let Some(pos) = src[i..].find("$this->") {
        let start = i + pos;
        let mut j = start + "$this->".len();
        let prop_start = j;
        while j < bytes.len() && is_ident_byte(bytes[j]) {
            j += 1;
        }
        let prop = &src[prop_start..j];
        i = start + 1;
        if prop.is_empty() {
            continue;
        }
        // what follows the property reference?
        if j + 1 < bytes.len() && bytes[j] == b'-' && bytes[j + 1] == b'>' {
            // `$this->P->IDENT` — read the method/field name
            let mut k = j + 2;
            let m_start = k;
            while k < bytes.len() && is_ident_byte(bytes[k]) {
                k += 1;
            }
            let member = src[m_start..k].to_ascii_lowercase();
            let mut w = k;
            while w < bytes.len() && matches!(bytes[w], b' ' | b'\t' | b'\n' | b'\r') {
                w += 1;
            }
            if w < bytes.len() && bytes[w] == b'(' {
                // method call: mutation unless the name has a reader prefix
                let is_reader = HOIST_READER_PREFIXES.iter().any(|p| member.starts_with(p));
                if !is_reader {
                    push(prop, &mut out);
                }
            } else if w < bytes.len()
                && bytes[w] == b'='
                && !(w + 1 < bytes.len() && matches!(bytes[w + 1], b'=' | b'>'))
            {
                // `$this->P->x = ` write
                push(prop, &mut out);
            }
            continue;
        }
        // `$this->P[...] = ` or `$this->P = ` write/reassign
        let mut k = j;
        if k < bytes.len() && bytes[k] == b'[' {
            // skip to matching ] (no nested [] expected in a simple key)
            while k < bytes.len() && bytes[k] != b']' {
                k += 1;
            }
            if k < bytes.len() {
                k += 1;
            }
        }
        while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r') {
            k += 1;
        }
        if k < bytes.len()
            && bytes[k] == b'='
            && !(k + 1 < bytes.len() && matches!(bytes[k + 1], b'=' | b'>'))
        {
            push(prop, &mut out);
        }
    }
    out
}

/// Scan a class body for the two SharedTransactionalFixture eligibility signals of THIS class
/// (folded across the inheritance chain later, in [`shared_fixture_report`]):
///   - `setup_builds_fixture`: the `setUp` method's source calls a `TX_FIXTURE_BUILDERS` call;
///   - `has_tx_disqualifier`: a non-lifecycle (test/helper) method's source hits a
///     `TX_DISQUALIFIERS` substring, OR any test method carries `#[Depends]`/`@depends`
///     (cross-test state, captured by discovery's `depends_on`).
fn scan_tx_eligibility_signals(
    body: Node,
    bytes: &[u8],
    test_methods: &[MethodInfo],
) -> (bool, bool) {
    let mut builds_fixture = false;
    let mut has_disqualifier = false;
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };
        let src = child.utf8_text(bytes).unwrap_or("");
        if name.eq_ignore_ascii_case("setUp") && setup_builds_fixture_src(src) {
            builds_fixture = true;
        }
        if !is_tx_lifecycle(name) && method_tx_disqualifier_src(src).is_some() {
            has_disqualifier = true;
        }
    }
    if test_methods.iter().any(|m| !m.depends_on.is_empty()) {
        has_disqualifier = true;
    }
    (builds_fixture, has_disqualifier)
}

/// Per-class Way-3 hoist signals (folded across the chain in `setup_hoist_report`):
///   - `candidates`: this class's `setUp` direct `$this->P = RHS` assignments (prop, rhs, nondet);
///   - `ctx_scopes`: ambient-context scopes (tz/now/locale) this class's `setUp` establishes;
///   - `mutated`: properties any per-test method (test/tearDown/helper, NOT the class-level
///     setUpBeforeClass/tearDownAfterClass) of this class mutates.
#[allow(clippy::type_complexity)]
fn scan_hoist_signals(
    body: Node,
    bytes: &[u8],
) -> (Vec<(String, String, bool)>, Vec<String>, Vec<String>) {
    let mut candidates: Vec<(String, String, bool)> = Vec::new();
    let mut ctx_scopes: Vec<String> = Vec::new();
    let mut mutated: Vec<String> = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };
        let src = child.utf8_text(bytes).unwrap_or("");
        if name.eq_ignore_ascii_case("setUp") {
            candidates = setup_hoist_candidates_src(src);
            ctx_scopes = setup_ctx_scopes(src);
        } else if !matches!(
            name.to_ascii_lowercase().as_str(),
            "setupbeforeclass" | "teardownafterclass"
        ) {
            for p in body_mutated_props_src(src) {
                if !mutated.contains(&p) {
                    mutated.push(p);
                }
            }
        }
    }
    (candidates, ctx_scopes, mutated)
}

/// One concrete test class's SharedTransactionalFixture advisory verdict (report-only; no
/// runtime effect). `uses_shared_fixture` = the class (or an in-project ancestor) opts in via
/// the trait; `tx_eligible` = a fixture builder exists in the chain AND no disqualifier does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFixtureReport {
    pub fqcn: String,
    pub file: PathBuf,
    pub uses_shared_fixture: bool,
    pub tx_eligible: bool,
    pub tx_ineligible_reason: Option<String>,
}

/// Advisory analysis: for each CONCRETE (non-abstract) test class, fold the three
/// SharedTransactionalFixture signals across the in-project inheritance chain (mirroring
/// `emit_test_cases`'s depth-bounded walk) and produce a verdict. Standalone by design — it does
/// NOT thread through the public `TestCase`/`TestClass` types, because the runtime never routes
/// on it (the build-once guard lives in the PHP trait). Consumed only by the report CLI.
fn shared_fixture_report(parsed: &[ParsedClass], graph: &ClassGraph) -> Vec<SharedFixtureReport> {
    let by_fqcn: HashMap<&str, &ParsedClass> =
        parsed.iter().map(|c| (c.fqcn.as_str(), c)).collect();
    let mut out = Vec::new();
    for class in parsed {
        if class.is_abstract || !is_test_class_via_chain(&class.fqcn, graph) {
            continue;
        }
        let (mut uses, mut builds, mut disq) = (false, false, false);
        let mut visit = class.fqcn.as_str();
        let mut d = 0;
        while d < 32 {
            if let Some(c) = by_fqcn.get(visit) {
                uses = uses || c.uses_shared_fixture;
                builds = builds || c.setup_builds_fixture;
                disq = disq || c.has_tx_disqualifier;
                match c.parent_fqcn.as_deref() {
                    Some(p) => visit = p,
                    None => break,
                }
            } else {
                break;
            }
            d += 1;
        }
        let (tx_eligible, tx_ineligible_reason) = if !builds {
            (
                false,
                Some("setUp builds no recognised fixture".to_string()),
            )
        } else if disq {
            (
                false,
                Some("a test method uses a rollback-incompatible construct".to_string()),
            )
        } else {
            (true, None)
        };
        out.push(SharedFixtureReport {
            fqcn: class.fqcn.clone(),
            file: class.file.clone(),
            uses_shared_fixture: uses,
            tx_eligible,
            tx_ineligible_reason,
        });
    }
    out
}

/// Single-file SharedTransactionalFixture advisory report (used by tests and the report CLI).
pub fn shared_fixture_report_in_file(path: &Path) -> Result<Vec<SharedFixtureReport>> {
    let parsed = parse_file_classes(path)?;
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();
    Ok(shared_fixture_report(&parsed, &graph))
}

/// Format an advisory report: one tab-separated line per concrete test class
/// (`<fqcn>\tuses=<yes|no>\teligible=<yes|no>\t<reason>`), a `WARN` line for any class that
/// `use`s the trait but is ineligible (likely misuse), and a trailing `eligible: N/total`.
pub fn format_shared_fixture_report(report: &[SharedFixtureReport]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut eligible = 0usize;
    for c in report {
        let uses = if c.uses_shared_fixture { "yes" } else { "no" };
        let el = if c.tx_eligible {
            eligible += 1;
            "yes"
        } else {
            "no"
        };
        let reason = c.tx_ineligible_reason.as_deref().unwrap_or("");
        let _ = writeln!(
            out,
            "{}\tuses={}\teligible={}\t{}",
            c.fqcn, uses, el, reason
        );
        if c.uses_shared_fixture && !c.tx_eligible {
            let _ = writeln!(
                out,
                "WARN {} uses SharedTransactionalFixture but is ineligible: {}",
                c.fqcn, reason
            );
        }
    }
    let _ = writeln!(out, "eligible: {}/{}", eligible, report.len());
    out
}

/// Project-level SharedTransactionalFixture advisory report: parse every `*Test*.php` under
/// `root`, build the in-project inheritance graph, and verdict each concrete test class.
/// Self-contained gather (advisory, not parity-critical) so it never touches the hot
/// `discover_in_dirs` path.
pub fn shared_fixture_report_in_dir(root: &Path) -> Result<Vec<SharedFixtureReport>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("php")
            && p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .contains("Test")
        {
            files.push(p.to_path_buf());
        }
    }
    files.sort(); // deterministic, machine-order-independent
    let parsed: Vec<ParsedClass> = files
        .iter()
        .map(|p| parse_file_classes(p))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();
    Ok(shared_fixture_report(&parsed, &graph))
}

/// One setUp `$this->P = RHS` candidate's Way-3 hoist verdict (report-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoistCandidateVerdict {
    pub prop: String,
    pub rhs: String,
    pub hoistable: bool,
    pub reason: String,
}

/// One concrete test class's setUp-hoist advisory (`--report-hoistable-setup`).
/// `test_count` is the hoist multiplicity (setUp currently runs once per test).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupHoistReport {
    pub fqcn: String,
    pub file: PathBuf,
    pub candidates: Vec<HoistCandidateVerdict>,
    pub test_count: usize,
}

/// Advisory: for each concrete test class, fold the Way-3 hoist signals across the
/// in-project inheritance chain — a setUp candidate, an ambient-context setter, or a
/// property mutation in ANY ancestor counts — and verdict each setUp candidate. A
/// candidate HOISTs iff its RHS is deterministic, no per-test context is established
/// anywhere in the chain, and no per-test method mutates the property. Classes with no
/// setUp candidate are omitted (nothing to advise). Standalone like
/// [`shared_fixture_report`]; consumed only by the report CLI.
fn setup_hoist_report(parsed: &[ParsedClass], graph: &ClassGraph) -> Vec<SetupHoistReport> {
    let by_fqcn: HashMap<&str, &ParsedClass> =
        parsed.iter().map(|c| (c.fqcn.as_str(), c)).collect();
    let mut out = Vec::new();
    for class in parsed {
        if class.is_abstract || !is_test_class_via_chain(&class.fqcn, graph) {
            continue;
        }
        let mut candidates: Vec<(String, String, bool)> = Vec::new();
        let mut ctx: Vec<String> = Vec::new();
        let mut mutated: Vec<String> = Vec::new();
        let mut test_count = 0usize;
        let mut seen_props: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut seen_methods: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut visit = class.fqcn.as_str();
        let mut d = 0;
        while d < 32 {
            if let Some(c) = by_fqcn.get(visit) {
                for cand in &c.setup_hoist_candidates {
                    // First-seen (most-derived) setUp assignment wins per property.
                    if seen_props.insert(cand.0.clone()) {
                        candidates.push(cand.clone());
                    }
                }
                for s in &c.setup_ctx_scopes {
                    if !ctx.iter().any(|x| x == s) {
                        ctx.push(s.clone());
                    }
                }
                for m in &c.mutated_props {
                    if !mutated.iter().any(|x| x == m) {
                        mutated.push(m.clone());
                    }
                }
                for mi in &c.test_methods {
                    if seen_methods.insert(mi.name.to_ascii_lowercase()) {
                        test_count += 1;
                    }
                }
                match c.parent_fqcn.as_deref() {
                    Some(p) => visit = p,
                    None => break,
                }
            } else {
                break;
            }
            d += 1;
        }
        if candidates.is_empty() {
            continue;
        }
        let any_ctx = !ctx.is_empty();
        let verdicts: Vec<HoistCandidateVerdict> = candidates
            .into_iter()
            .map(|(prop, rhs, nondet)| {
                let (hoistable, reason) = if nondet {
                    (false, "REFUSE: non-deterministic RHS".to_string())
                } else if any_ctx {
                    (
                        false,
                        format!("REFUSE: per-test ambient context ({})", ctx.join(",")),
                    )
                } else if mutated.contains(&prop) {
                    (false, format!("REFUSE: '{prop}' mutated by a test"))
                } else {
                    (
                        true,
                        "HOIST: deterministic, context-stable, never mutated".to_string(),
                    )
                };
                HoistCandidateVerdict {
                    prop,
                    rhs,
                    hoistable,
                    reason,
                }
            })
            .collect();
        out.push(SetupHoistReport {
            fqcn: class.fqcn.clone(),
            file: class.file.clone(),
            candidates: verdicts,
            test_count,
        });
    }
    out
}

/// Single-file setUp-hoist advisory report (used by tests and the report CLI).
pub fn setup_hoist_report_in_file(path: &Path) -> Result<Vec<SetupHoistReport>> {
    let parsed = parse_file_classes(path)?;
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();
    Ok(setup_hoist_report(&parsed, &graph))
}

/// Project-level setUp-hoist advisory: parse every `*Test*.php` under `root`, build
/// the in-project inheritance graph, verdict each concrete test class. Self-contained
/// (advisory, not parity-critical); never touches the hot `discover_in_dirs` path.
pub fn setup_hoist_report_in_dir(root: &Path) -> Result<Vec<SetupHoistReport>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("php")
            && p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .contains("Test")
        {
            files.push(p.to_path_buf());
        }
    }
    files.sort();
    let parsed: Vec<ParsedClass> = files
        .iter()
        .map(|p| parse_file_classes(p))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();
    Ok(setup_hoist_report(&parsed, &graph))
}

/// Format the setUp-hoist advisory: per class, a header line then one indented line
/// per candidate (`HOIST`/`REFUSE`, `$this->prop = rhs`, multiplicity, reason); a
/// trailing `hoistable: H/total candidate(s) across C class(es)` summary.
pub fn format_setup_hoist_report(report: &[SetupHoistReport]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let (mut hoistable, mut total) = (0usize, 0usize);
    for c in report {
        let _ = writeln!(out, "{} (×{} tests)", c.fqcn, c.test_count);
        for v in &c.candidates {
            total += 1;
            let tag = if v.hoistable {
                hoistable += 1;
                "HOIST "
            } else {
                "REFUSE"
            };
            let rhs: String = v.rhs.chars().take(48).collect();
            let _ = writeln!(
                out,
                "  {}  $this->{} = {}\t:: {}",
                tag, v.prop, rhs, v.reason
            );
        }
    }
    let _ = writeln!(
        out,
        "hoistable: {}/{} candidate(s) across {} class(es)",
        hoistable,
        total,
        report.len()
    );
    out
}

fn collect_test_methods(
    body: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
) -> Vec<MethodInfo> {
    let mut methods = Vec::new();
    let mut cursor = body.walk();
    let mut prev_comment: Option<String> = None;

    for child in body.children(&mut cursor) {
        if child.kind() == "comment" {
            prev_comment = child.utf8_text(bytes).ok().map(String::from);
            continue;
        }
        if child.kind() != "method_declaration" {
            prev_comment = None;
            continue;
        }
        let is_public = method_is_public(child, bytes);
        let Some(name_node) = child.child_by_field_name("name") else {
            prev_comment = None;
            continue;
        };
        let Ok(name) = name_node.utf8_text(bytes) else {
            prev_comment = None;
            continue;
        };

        // PHPUnit recognises a method as a test if:
        //   - its name starts with "test"
        //   - it has a /** @test */ PHPDoc annotation
        //   - it has a #[Test] or #[PHPUnit\Framework\Attributes\Test] attribute
        let has_annotation = prev_comment
            .as_deref()
            .map(|c| c.contains("@test"))
            .unwrap_or(false);
        let has_attr = method_has_test_attribute(child, bytes);

        if is_public && (name.starts_with("test") || has_annotation || has_attr) {
            let dp = prev_comment
                .as_deref()
                .and_then(phpdoc_data_provider)
                .or_else(|| method_data_provider_attr(child, bytes));
            let mut groups = extract_groups_attr(child, bytes);
            if let Some(c) = prev_comment.as_deref() {
                extract_groups_phpdoc(c, &mut groups);
            }
            let external_providers =
                extract_external_provider_attrs(child, bytes, namespace, aliases);
            let is_tautological = is_tautological_method(child, bytes);
            let mut depends_on = prev_comment
                .as_deref()
                .map(phpdoc_depends)
                .unwrap_or_default();
            depends_on.extend(method_depends_attr(child, bytes));
            let fingerprint = extract_method_fingerprint(child, bytes, namespace, aliases);
            methods.push(MethodInfo {
                name: name.to_string(),
                data_provider: dp,
                groups,
                external_providers,
                is_tautological,
                depends_on,
                fingerprint,
            });
        }
        prev_comment = None;
    }
    methods
}

/// Extract all `@depends methodName` targets from a PHPDoc comment.
fn phpdoc_depends(comment: &str) -> Vec<String> {
    let needle = "@depends ";
    let mut result = Vec::new();
    let mut search = comment;
    while let Some(pos) = search.find(needle) {
        let after = &search[pos + needle.len()..];
        let name = after
            .split(|c: char| c.is_whitespace() || c == '*')
            .next()
            .unwrap_or("");
        if !name.is_empty() {
            result.push(name.to_string());
        }
        search = &search[pos + needle.len()..];
    }
    result
}

/// Extract all `#[Depends('methodName')]` targets from a method node's attributes.
fn method_depends_attr(method: Node, bytes: &[u8]) -> Vec<String> {
    let mut result = Vec::new();
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        let Ok(text) = child.utf8_text(bytes) else {
            continue;
        };
        if !text.starts_with("#[") || !text.contains("Depends") {
            continue;
        };
        // Walk all occurrences of Depends( in this attribute block
        let mut search = text;
        while let Some(start) = search.find("Depends(") {
            let after = &search[start + "Depends(".len()..];
            let trimmed = after.trim_start();
            let name = if let Some(r) = trimmed.strip_prefix('\'') {
                r.split('\'').next().unwrap_or("")
            } else if let Some(r) = trimmed.strip_prefix('"') {
                r.split('"').next().unwrap_or("")
            } else {
                ""
            };
            if !name.is_empty() {
                result.push(name.to_string());
            }
            search = &search[start + "Depends(".len()..];
        }
    }
    result
}

/// Extract `name` from a `@dataProvider name` annotation in a PHPDoc block.
/// Handles single-line (`/** @dataProvider foo */`) and multi-line forms
/// alike by searching the whole comment text. Returns the first match.
fn phpdoc_data_provider(comment: &str) -> Option<String> {
    let needle = "@dataProvider ";
    let start = comment.find(needle)?;
    let after = &comment[start + needle.len()..];
    let name = after
        .split(|c: char| c.is_whitespace() || c == '*')
        .next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Extract the provider name from `#[DataProvider("name")]` (also matches
/// the fully-qualified `#[PHPUnit\Framework\Attributes\DataProvider(...)]`).
/// Single or double quotes accepted.
fn method_data_provider_attr(method: Node, bytes: &[u8]) -> Option<String> {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        let Ok(text) = child.utf8_text(bytes) else {
            continue;
        };
        if !text.starts_with("#[") {
            continue;
        }
        if let Some(name) = extract_data_provider_arg(text) {
            return Some(name);
        }
    }
    None
}

fn extract_data_provider_arg(attr_text: &str) -> Option<String> {
    let start = attr_text.find("DataProvider(")?;
    // Guard against #[DataProviderExternal(...)] or similar overlap.
    let after_ident_start = start + "DataProvider".len();
    if attr_text.as_bytes().get(after_ident_start) != Some(&b'(') {
        return None;
    }
    let inside = &attr_text[after_ident_start + 1..];
    let trimmed = inside.trim_start();
    let (quote, rest) = if let Some(r) = trimmed.strip_prefix('\'') {
        ('\'', r)
    } else if let Some(r) = trimmed.strip_prefix('"') {
        ('"', r)
    } else {
        return None;
    };
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Parse `DataProviderExternal(ClassName::class, 'method')` occurrences from
/// an attribute list text string (e.g. `"#[DataProviderExternal(Foo::class, 'bar')]"`).
/// Returns `(resolved_fqcn, method_name)` for every match found.
fn parse_external_provider_attr_text(
    text: &str,
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let needle = "DataProviderExternal(";
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(idx) = text[search..].find(needle) {
        let abs = search + idx;
        // Reject false matches — require a boundary character before `DataProviderExternal`.
        let before_ok = abs == 0
            || matches!(
                text.as_bytes()[abs - 1],
                b'[' | b',' | b'\\' | b' ' | b'\t' | b'\n' | b'\r'
            );
        let inside_start = abs + needle.len();
        if before_ok {
            let inside = &text[inside_start..];
            if let Some(class_end) = inside.find("::class") {
                // `::class` is only a valid PHP expression as a standalone keyword —
                // any continuation (e.g. `::classMap`) would cause the comma-quote scan
                // below to find no quote and produce `method_name = None`, silently
                // discarding the entry rather than emitting a false match.
                let raw_class = inside[..class_end].trim();
                let fqcn = resolve_class_reference(raw_class, namespace, aliases);
                let after_class = &inside[class_end + "::class".len()..];
                let after_comma =
                    after_class.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
                let method_name = if let Some(r) = after_comma.strip_prefix('\'') {
                    r.find('\'').map(|e| r[..e].to_string())
                } else if let Some(r) = after_comma.strip_prefix('"') {
                    r.find('"').map(|e| r[..e].to_string())
                } else {
                    None
                };
                if let Some(m) = method_name {
                    out.push((fqcn, m));
                }
            }
            search = inside_start;
        } else {
            search = abs + 1;
        }
    }
    out
}

fn extract_external_provider_attrs(
    method: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        let Ok(text) = child.utf8_text(bytes) else {
            continue;
        };
        if !text.starts_with("#[") {
            continue;
        }
        out.extend(parse_external_provider_attr_text(text, namespace, aliases));
    }
    out
}

fn method_has_test_attribute(method: Node, bytes: &[u8]) -> bool {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        let Ok(text) = child.utf8_text(bytes) else {
            continue;
        };
        if !text.starts_with("#[") {
            continue;
        }
        if has_attribute_name(text, "Test") {
            return true;
        }
    }
    false
}

/// Extract every group name declared on a node via:
///   #[Group('name')]   or  #[PHPUnit\Framework\Attributes\Group('name')]
///   #[Ticket('name')]  or  #[PHPUnit\Framework\Attributes\Ticket('name')]
///
/// `Ticket` is PHPUnit's documented alias for `Group` — they share a
/// namespace and PHPUnit's runner treats them identically. `node` is
/// typically the method_declaration or class_declaration.
fn extract_groups_attr(node: Node, bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let Ok(text) = child.utf8_text(bytes) else {
            continue;
        };
        if !text.starts_with("#[") {
            continue;
        }
        for needle in ["Group(", "Ticket("] {
            scan_string_arg(text, needle, &mut out);
        }
    }
    out
}

/// Scan `text` for every occurrence of `needle` (an identifier followed
/// by `(`), check the preceding char is an attribute-list boundary, then
/// extract the first quoted string argument. Pushes into `out`.
fn scan_string_arg(text: &str, needle: &str, out: &mut Vec<String>) {
    let mut search_start = 0;
    while let Some(idx) = text[search_start..].find(needle) {
        let abs = search_start + idx;
        let before_ok = abs == 0
            || matches!(
                text.as_bytes()[abs - 1],
                b'[' | b',' | b'\\' | b' ' | b'\t' | b'\n' | b'\r'
            );
        if before_ok {
            let inside = &text[abs + needle.len()..];
            let trimmed = inside.trim_start();
            let parsed = if let Some(r) = trimmed.strip_prefix('\'') {
                r.find('\'').map(|end| &r[..end])
            } else if let Some(r) = trimmed.strip_prefix('"') {
                r.find('"').map(|end| &r[..end])
            } else {
                None
            };
            if let Some(name) = parsed {
                out.push(name.to_string());
            }
        }
        search_start = abs + needle.len();
    }
}

/// Return the docblock comment immediately preceding `node`, if any.
/// PHPDoc lives in the file as a regular `comment` node positioned right
/// before the declaration; we walk siblings backwards from `node`'s parent.
fn preceding_docblock(node: Node, bytes: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    let mut prev: Option<Node> = None;
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.id() == node.id() {
            break;
        }
        prev = Some(child);
    }
    let prev = prev?;
    if prev.kind() != "comment" {
        return None;
    }
    let text = prev.utf8_text(bytes).ok()?;
    if !text.contains("/**") {
        return None;
    }
    Some(text.to_string())
}

/// Extract group names from PHPDoc lines. PHPUnit recognises both
/// `@group name` and `@ticket name` (which is just an alias).
fn extract_groups_phpdoc(comment: &str, into: &mut Vec<String>) {
    for line in comment.lines() {
        let trimmed = line.trim_start_matches(|c: char| c == '*' || c == '/' || c.is_whitespace());
        for prefix in ["@group ", "@ticket "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name = rest.split_whitespace().next().unwrap_or("");
                if !name.is_empty() {
                    into.push(name.to_string());
                }
            }
        }
    }
}

/// Tree-sitter-php groups consecutive `#[Foo] #[Bar]` decorations into one
/// attribute_list node whose text is a concatenation. So `text == "#[Test]"`
/// fails for the common doctrine-orm pattern:
///
/// ```text
/// #[Test]
/// #[Group('GH7266')]
/// public function corruptedDataDoesNotLeakIntoApplication(): void
/// ```
///
/// We instead scan the attribute text for `name` as a properly delimited
/// identifier — preceded by `[`, `,`, `\`, or whitespace, and followed by
/// `]`, `,`, `(`, or whitespace. This rejects `TestWith`, `TestDox`,
/// `TestDoxName`, etc., while accepting `#[Test]`, `#[Test, Other]`, and
/// fully-qualified `#[PHPUnit\Framework\Attributes\Test]`.
fn has_attribute_name(attr_text: &str, name: &str) -> bool {
    let haystack = attr_text.as_bytes();
    let needle = name.as_bytes();
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    let is_boundary_before =
        |b: u8| matches!(b, b'[' | b',' | b'\\' | b' ' | b'\t' | b'\n' | b'\r');
    let is_boundary_after = |b: u8| matches!(b, b']' | b',' | b'(' | b' ' | b'\t' | b'\n' | b'\r');
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            let before_ok = i == 0 || is_boundary_before(haystack[i - 1]);
            let after_idx = i + needle.len();
            let after_ok = after_idx == haystack.len() || is_boundary_after(haystack[after_idx]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn method_is_public(method: Node, bytes: &[u8]) -> bool {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child
                .utf8_text(bytes)
                .map(|t| t == "public")
                .unwrap_or(false);
        }
    }
    // PHP defaults to public when no visibility modifier is present.
    true
}

/// Convenience wrapper: discover tests in a single PHP file. Equivalent to
/// `discover_in_dir` on a directory containing only this file — useful for
/// unit tests and for cases where inheritance chains live entirely in one
/// file. Cross-file inheritance won't resolve (use `discover_in_dir`).
pub fn discover_in_file(path: &Path) -> Result<Vec<TestCase>> {
    let parsed = parse_file_classes(path)?;
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();
    emit_test_cases(&parsed, &graph)
}

/// Pass 3: walk the parsed classes, filter by the BFS, emit TestCases.
/// Shared between `discover_in_file` and `discover_in_dir`.
///
/// For each non-abstract class reaching TestCase, we union test methods
/// from the class itself plus every parsed ancestor in the chain. This
/// catches the doctrine/collections-style pattern where a `final` concrete
/// test class extends an abstract base that defines most/all of its tests.
/// Without this, the concrete class would emit zero TestCase entries (the
/// inherited methods were on the abstract parent), and the runner would
/// silently skip them.
/// Append `c`'s used-trait sources to `sources` — pre-order, transitive (a
/// trait that `use`s another trait), guarded against cycles/diamonds by
/// `visited`. Only entries that resolve to a parsed TRAIT are followed; a `use`d
/// name resolving to a class (or to nothing) is ignored.
fn collect_trait_sources<'a>(
    c: &'a ParsedClass,
    by_fqcn: &HashMap<&'a str, &'a ParsedClass>,
    sources: &mut Vec<&'a ParsedClass>,
    visited: &mut std::collections::HashSet<&'a str>,
) {
    for t in &c.used_traits {
        if !visited.insert(t.as_str()) {
            continue;
        }
        if let Some(tc) = by_fqcn.get(t.as_str()).copied() {
            if tc.is_trait {
                sources.push(tc);
                collect_trait_sources(tc, by_fqcn, sources, visited);
            }
        }
    }
}

fn emit_test_cases(parsed: &[ParsedClass], graph: &ClassGraph) -> Result<Vec<TestCase>> {
    // Index by FQCN for chain-walking.
    let by_fqcn: HashMap<&str, &ParsedClass> =
        parsed.iter().map(|c| (c.fqcn.as_str(), c)).collect();

    let mut cases = Vec::new();
    for class in parsed {
        // A trait is parsed for its methods but never run as a test class itself.
        if class.is_trait || class.is_abstract {
            continue;
        }
        if !is_test_class_via_chain(&class.fqcn, graph) {
            continue;
        }
        // Ordered method/flag sources: walk the inheritance chain, and at each
        // level take the class itself THEN its used traits (transitively). PHP
        // precedence is own-class > traits > parent — this order encodes it, and
        // the lowercased-name `seen` dedup below makes the first occurrence win
        // (PHP method names are case-insensitive at call time; PHPUnit's
        // reflection discovery collapses overrides — we mirror that).
        let mut sources: Vec<&ParsedClass> = Vec::new();
        {
            let mut visited_traits: std::collections::HashSet<&str> =
                std::collections::HashSet::new();
            let mut visit = class.fqcn.as_str();
            let mut depth = 0;
            while depth < 32 {
                let Some(c) = by_fqcn.get(visit).copied() else {
                    // Parent is outside our parsed set (e.g. PHPUnit's TestCase).
                    break;
                };
                sources.push(c);
                collect_trait_sources(c, &by_fqcn, &mut sources, &mut visited_traits);
                match c.parent_fqcn.as_deref() {
                    Some(p) => visit = p,
                    None => break,
                }
                depth += 1;
            }
        }
        // OR the class-nature flags across every source (chain + traits): a
        // setUp/marker inherited from a parent OR pulled in via a trait flags
        // the concrete class the same way.
        let chain_is_stateful = sources.iter().any(|c| c.is_stateful);
        let chain_is_isolated = sources.iter().any(|c| c.is_isolated);
        let chain_needs_db = sources.iter().any(|c| c.needs_db);
        let chain_is_functional = sources.iter().any(|c| c.is_functional);

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &sources {
            for mi in &c.test_methods {
                if seen.insert(mi.name.to_ascii_lowercase()) {
                    // Effective groups = the concrete subclass's class-level
                    // groups + the source's class-level groups + the method's own.
                    let mut groups: std::collections::BTreeSet<String> =
                        class.class_groups.iter().cloned().collect();
                    groups.extend(c.class_groups.iter().cloned());
                    groups.extend(mi.groups.iter().cloned());
                    cases.push(TestCase {
                        file: class.file.clone(),
                        class: class.fqcn.clone(),
                        method: mi.name.clone(),
                        data_provider: mi.data_provider.clone(),
                        groups: groups.into_iter().collect(),
                        external_providers: mi.external_providers.clone(),
                        is_tautological: mi.is_tautological,
                        has_lifecycle_overrides: class.has_lifecycle_overrides,
                        depends_on: mi.depends_on.clone(),
                        is_dispatch_safe: mi.depends_on.is_empty(),
                        fingerprint: mi.fingerprint.clone(),
                        is_stateful: chain_is_stateful,
                        is_isolated: chain_is_isolated,
                        needs_db: chain_needs_db,
                        is_functional: chain_is_functional,
                    });
                }
            }
        }
    }
    Ok(cases)
}

/// Walk a directory, returning all discovered test cases. Convenience wrapper
/// around `discover_in_dirs` for the single-root + no-excludes case.
pub fn discover_in_dir(root: &Path) -> Result<Vec<TestCase>> {
    let roots = [root.to_path_buf()];
    discover_in_dirs(&roots, &[], &[])
}

/// Walk multiple roots and union their tests, honoring an exclude list.
///
/// Three-pass algorithm (extended to multi-root):
///   1. Parse every `*Test*.php` file under each root, skipping anything whose
///      path begins with one of the excludes. Build a flat `Vec<ParsedClass>`.
///   2. Build a `ClassGraph`: FQCN → parent FQCN. Inheritance resolves across
///      roots — an abstract base in `tests/` and a concrete subclass in
///      `vendor/somepkg/tests/` are linked correctly.
///   3. For each non-abstract class reaching `TestCase` via the chain, emit
///      one `TestCase` per inherited test method.
///
/// `excludes` are checked as path prefixes (canonicalized) — a path under an
/// excluded directory is skipped. Matches phpunit.xml's `<testsuite>` /
/// `<exclude>` semantics.
/// Walk multiple roots and union their tests, honoring an exclude list.
///
/// `graph_supplement_dirs` are additional directories (e.g. from composer.json
/// `autoload-dev`) scanned to build a complete class graph — they contribute
/// abstract base classes to the inheritance chain but never emit test cases
/// themselves. Pass an empty slice when not needed.
/// Like [`discover_in_dirs`] but also returns a FQCN→file index for EVERY
/// class parsed from the test files (roots + supplement `*Test*.php`),
/// including co-located non-test helper classes defined inside a `*Test*.php`
/// file (e.g. `TestableLorem` in `LoremTest.php`). Such helpers are absent
/// from a cases-derived index and would otherwise end up in the "unresolvable"
/// set, preventing the PSR-4 fast path from triggering.
///
/// This index is a SUBSET of [`discover_with_index`]'s full index (it omits
/// only non-`*Test*` files), so the runner's PSR-4-sufficiency gate remains
/// sound: a genuinely non-PSR-4 class in a non-test file still lands in `U`
/// and triggers the full fallback.
pub fn discover_cases_and_test_index(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    graph_supplement_dirs: &[PathBuf],
) -> Result<(Vec<TestCase>, HashMap<String, PathBuf>)> {
    // Canonicalize excludes once so prefix checks are robust.
    let canon_excludes: Vec<PathBuf> = excludes
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();

    // Pass 1: collect matching paths, then parse in parallel. A single `seen`
    // set dedups across overlapping roots (e.g. phpunit.xml declaring both
    // `tests/` and a nested `tests/unit/`) AND across the supplement walk
    // below — mirroring `discover_with_index` so the fast path's emission
    // count matches the full parse (a duplicate would double-count `emit_count`).
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut root_files: Vec<PathBuf> = Vec::new();
    for root in roots {
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("php") {
                continue;
            }
            if !p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .contains("Test")
            {
                continue;
            }
            if let Ok(canon) = p.canonicalize() {
                if canon_excludes.iter().any(|ex| canon.starts_with(ex)) {
                    continue;
                }
            }
            let buf = p.to_path_buf();
            if !seen.insert(buf.clone()) {
                continue;
            }
            root_files.push(buf);
        }
    }
    // Sort for deterministic, filesystem-order-independent discovery. WalkDir
    // yields entries in the OS readdir order, which differs across machines
    // (e.g. a CI runner vs a dev box, even inside the same container image —
    // tmpfs readdir order is host/kernel-dependent). Downstream FQCN handling
    // (the inheritance `graph` HashMap below is last-insert-wins on duplicate
    // FQCNs, and emission keys off parse order) is order-sensitive, so an
    // unsorted walk made the emitted test count MACHINE-DEPENDENT — the daily
    // bench's CI-vs-local parity drift (carbon/doctrine/php-parser). A stable
    // sort makes discovery reproducible everywhere.
    root_files.sort();
    let mut parsed: Vec<ParsedClass> = root_files
        .par_iter()
        .map(|p| parse_file_classes(p))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let emit_count = parsed.len(); // classes from roots — those we'll emit TestCases for

    // Supplement: parse *Test*.php files from autoload-dev dirs to enrich the
    // class graph with abstract base classes that live outside the testsuite
    // directories (e.g. Carbon's tests/AbstractTestCase.php). The shared `seen`
    // set already holds every root file, so a supplement file already visited in
    // the roots is skipped — and overlapping supplement dirs dedup against each
    // other too.
    let mut supp_files: Vec<PathBuf> = Vec::new();
    for dir in graph_supplement_dirs {
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("php") {
                continue;
            }
            if !p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .contains("Test")
            {
                continue;
            }
            let buf = p.to_path_buf();
            if !seen.insert(buf.clone()) {
                continue;
            }
            supp_files.push(buf);
        }
    }
    supp_files.sort(); // same determinism rationale as root_files above
    let supp: Vec<ParsedClass> = supp_files
        .par_iter()
        .map(|p| parse_file_classes(p))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    parsed.extend(supp);

    // Pass 2: build the inheritance graph (FQCN -> parent FQCN or None).
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();

    // FQCN -> file for EVERY class parsed from the test files (roots + supplement),
    // including non-test helper classes co-located inside a *Test*.php file (e.g. a
    // `TestableLorem` defined in `LoremTest.php`). First occurrence wins, matching the
    // index semantics of `discover_with_index`. This is the fast-path fallback index;
    // it is a subset of `discover_with_index`'s full index (it omits only non-*Test*
    // files), so the runner's PSR-4-sufficiency gate stays sound.
    let index: HashMap<String, PathBuf> = {
        let mut m = HashMap::with_capacity(parsed.len());
        for c in &parsed {
            m.entry(c.fqcn.clone()).or_insert_with(|| c.file.clone());
        }
        m
    };

    // Pass 3: emit test methods only for classes from the testsuite roots.
    let cases = emit_test_cases(&parsed[..emit_count], &graph)?;
    Ok((cases, index))
}

pub fn discover_in_dirs(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    graph_supplement_dirs: &[PathBuf],
) -> Result<Vec<TestCase>> {
    Ok(discover_cases_and_test_index(roots, excludes, graph_supplement_dirs)?.0)
}

/// Single-pass discovery + FQCN index. Replaces a sequential
/// `discover_in_dirs(...)` + `discover_class_file_index(...)` pair: every
/// `.php` file in `roots ∪ supplement_dirs` is parsed once, then the
/// classes are split into the two outputs based on their origin bucket.
///
/// Semantics are preserved vs the two separate calls:
///   * Only `*Test*.php` files in `roots` produce `TestCase` entries.
///   * The inheritance graph includes `*Test*.php` from roots AND from
///     supplement dirs (matching pre-merge behaviour).
///   * The FQCN → file index includes ALL parsed files in both walks.
///   * `excludes` skip emission AND index for matching root files
///     (slightly stricter than the legacy split — `discover_class_file_index`
///     used to include excluded files in the index. In practice, excluded
///     dirs are explicit `_files/` fixtures whose classes the runner never
///     needs to autoload, so dropping them is a tightening that costs
///     nothing and saves spurious entries).
pub fn discover_with_index(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    supplement_dirs: &[PathBuf],
) -> Result<(Vec<TestCase>, HashMap<String, PathBuf>)> {
    #[derive(Copy, Clone)]
    enum Bucket {
        /// `*Test*.php` in roots — eligible for TestCase emission AND
        /// contributes to graph + index.
        TestRoot,
        /// `*Test*.php` in supplement — graph + index only (no emission).
        TestSupp,
        /// Any other `.php` — index only.
        IndexOnly,
    }

    let canon_excludes: Vec<PathBuf> = excludes
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();

    // Single union walk: dedupe paths across `roots` and `supplement_dirs`
    // (a file under both is seen once, with the bucket determined by its
    // first occurrence).
    let mut files: Vec<(PathBuf, Bucket)> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let is_test_name = |p: &Path| -> bool {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.contains("Test"))
            .unwrap_or(false)
    };

    for root in roots {
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("php") {
                continue;
            }
            if let Ok(canon) = p.canonicalize() {
                if canon_excludes.iter().any(|ex| canon.starts_with(ex)) {
                    continue;
                }
            }
            let buf = p.to_path_buf();
            if !seen.insert(buf.clone()) {
                continue;
            }
            let bucket = if is_test_name(p) {
                Bucket::TestRoot
            } else {
                Bucket::IndexOnly
            };
            files.push((buf, bucket));
        }
    }

    for dir in supplement_dirs {
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("php") {
                continue;
            }
            let buf = p.to_path_buf();
            if !seen.insert(buf.clone()) {
                continue;
            }
            let bucket = if is_test_name(p) {
                Bucket::TestSupp
            } else {
                Bucket::IndexOnly
            };
            files.push((buf, bucket));
        }
    }

    // Deterministic, filesystem-order-independent discovery. WalkDir yields OS
    // readdir order, which differs across machines (CI runner vs dev box, even
    // inside the same container image — tmpfs readdir order is host-dependent).
    // Downstream FQCN handling is order-sensitive (last-insert-wins graph,
    // parse-order emission), so an unsorted walk made the emitted test COUNT
    // machine-dependent — the daily-bench CI-vs-local parity drift. Sort by
    // path so the count is reproducible everywhere. (NOTE: this is the path the
    // runner actually uses; the sibling discover_in_dirs is sorted too.)
    files.sort_by(|a, b| a.0.cmp(&b.0));

    // Parse every file ONCE in parallel. Failures (`parse_file_classes`
    // returning Err) are silently skipped — the file likely isn't valid
    // PHP and would have been ignored by the split path too.
    let parsed: Vec<(ParsedClass, Bucket)> = files
        .par_iter()
        .flat_map(|(p, b)| {
            parse_file_classes(p)
                .unwrap_or_default()
                .into_iter()
                .map(move |c| (c, *b))
                .collect::<Vec<_>>()
        })
        .collect();

    // Index: every parsed class, first occurrence wins.
    let mut index: HashMap<String, PathBuf> = HashMap::with_capacity(parsed.len());
    for (c, _) in &parsed {
        index
            .entry(c.fqcn.clone())
            .or_insert_with(|| c.file.clone());
    }

    // Graph: only Test classes from roots + supplement (matches legacy).
    let graph: ClassGraph = parsed
        .iter()
        .filter(|(_, b)| matches!(b, Bucket::TestRoot | Bucket::TestSupp))
        .map(|(c, _)| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();

    // Emission set: only TestRoot classes are eligible.
    let root_test_classes: Vec<ParsedClass> = parsed
        .into_iter()
        .filter(|(_, b)| matches!(b, Bucket::TestRoot))
        .map(|(c, _)| c)
        .collect();

    let cases = emit_test_cases(&root_test_classes, &graph)?;
    Ok((cases, index))
}

/// Scan `dirs` for ALL `.php` files (not just `*Test*.php`) and return a
/// map of FQCN → file path. Used by the runner to locate files for
/// `#[DataProviderExternal]` provider classes that are not in the PSR-4
/// autoloader.
///
/// Only the first file seen for each FQCN is kept (stable, depth-first).
/// Like [`discover_class_file_index`] but parses ONLY `.php` files whose
/// stem (basename without extension) is in `candidate_stems`. Lets the
/// caller pre-narrow the parse set when it knows the exact class names it
/// is looking for — e.g. derived from the test cases' fingerprints after
/// PSR-4-resolvable FQCNs have been filtered out via a cheap lookup.
///
/// The returned map is post-filtered to only keep entries whose FQCN is
/// in `keep_fqcns`, mirroring the typical caller pattern.
pub fn discover_class_file_index_targeted(
    dirs: &[PathBuf],
    candidate_stems: &HashSet<String>,
    keep_fqcns: &HashSet<String>,
) -> HashMap<String, PathBuf> {
    if candidate_stems.is_empty() || keep_fqcns.is_empty() {
        return HashMap::new();
    }
    let files: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|dir| WalkDir::new(dir).into_iter().filter_map(|e| e.ok()))
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("php"))
        .filter_map(|e| {
            let p = e.path();
            let stem = p.file_stem()?.to_str()?;
            if candidate_stems.contains(stem) {
                Some(p.to_path_buf())
            } else {
                None
            }
        })
        .collect();

    let pairs: Vec<(String, PathBuf)> = files
        .par_iter()
        .flat_map(|p| {
            parse_file_classes(p)
                .unwrap_or_default()
                .into_iter()
                .filter(|c| keep_fqcns.contains(&c.fqcn))
                .map(|c| (c.fqcn, c.file))
                .collect::<Vec<_>>()
        })
        .collect();

    let mut index = HashMap::with_capacity(pairs.len());
    for (fqcn, file) in pairs {
        index.entry(fqcn).or_insert(file);
    }
    index
}

/// FQCN→file index over the NON-`*Test*.php` files in `dirs` (the "IndexOnly" bucket),
/// honoring `excludes`, with the same path-sorted, first-occurrence-wins semantics as
/// [`discover_with_index`]. Lets the runner's fallback build the full index by merging this
/// with the already-parsed test-file index, WITHOUT re-parsing the test files. Equivalent to
/// the non-test portion of `discover_with_index`'s index for any suite without a cross-file
/// FQCN redeclaration (which would be a PHP fatal anyway).
pub fn discover_nontest_class_index(
    dirs: &[PathBuf],
    excludes: &[PathBuf],
) -> HashMap<String, PathBuf> {
    let canon_excludes: Vec<PathBuf> = excludes
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for dir in dirs {
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("php") {
                continue;
            }
            if p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.contains("Test"))
                .unwrap_or(false)
            {
                continue;
            }
            if let Ok(canon) = p.canonicalize() {
                if canon_excludes.iter().any(|ex| canon.starts_with(ex)) {
                    continue;
                }
            }
            let buf = p.to_path_buf();
            if !seen.insert(buf.clone()) {
                continue;
            }
            files.push(buf);
        }
    }
    files.sort();
    let pairs: Vec<(String, PathBuf)> = files
        .par_iter()
        .flat_map(|p| {
            parse_file_classes(p)
                .unwrap_or_default()
                .into_iter()
                .map(|c| (c.fqcn, c.file))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut index = HashMap::with_capacity(pairs.len());
    for (fqcn, file) in pairs {
        index.entry(fqcn).or_insert(file);
    }
    index
}

pub fn discover_class_file_index(dirs: &[PathBuf]) -> HashMap<String, PathBuf> {
    let files: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|dir| WalkDir::new(dir).into_iter().filter_map(|e| e.ok()))
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("php"))
        .map(|e| e.path().to_path_buf())
        .collect();

    let pairs: Vec<(String, PathBuf)> = files
        .par_iter()
        .flat_map(|p| {
            parse_file_classes(p)
                .unwrap_or_default()
                .into_iter()
                .map(|c| (c.fqcn, c.file))
                .collect::<Vec<_>>()
        })
        .collect();

    let mut index = HashMap::with_capacity(pairs.len());
    for (fqcn, file) in pairs {
        index.entry(fqcn).or_insert(file);
    }
    index
}

/// Returns true if `start_fqcn` is (transitively) a subclass of PHPUnit's
/// `TestCase`.
///
/// **Inputs:**
/// - `start_fqcn`: the class to check (already a key in `graph`).
/// - `graph`: maps every discovered FQCN → its resolved parent FQCN (or `None`
///   when the class has no `extends` clause).
///
/// **Terminal rules** (return `true`):
/// - Any parent FQCN we visit equals `"PHPUnit\\Framework\\TestCase"`.
/// - Any parent FQCN we visit ends with `"\\TestCase"` (catches re-exports
///   or projects using a custom namespace for what is actually PHPUnit's class
///   — false positives are tolerable; the worker rejects non-TestCase classes
///   at runtime).
///
/// **Termination (return `false`):**
/// - The chain ends at a class with no parent (`None`).
///
/// **Safety:**
/// - Guard against cycles in the graph (malformed but defensible input).
///   Either bound iteration depth (e.g. 32) or maintain a visited set.
///
/// **Out of graph:**
/// - If a parent FQCN is *not* a key in `graph` (e.g. PHPUnit's own `TestCase`
///   defined in vendor/, outside our scanned tests/ directory), apply the
///   terminal name check to that string and stop. Returning `false` when the
///   parent is unknown AND doesn't match terminal patterns is correct: we
///   simply don't have enough information to claim it's a TestCase.
///
/// Last-segment heuristic for "this class looks like a PHPUnit TestCase".
///
/// Accepts: `TestCase` (bare), `PHPUnit\Framework\TestCase`, `My\Custom\TestCase`,
/// `PHPStan\Testing\PHPStanTestCase`, `Symfony\.../KernelTestCase`,
/// `Symfony\.../WebTestCase`. Rejects: `Foo\TestCases` (plural),
/// `Foo\NotTested`, `Foo\TestCaseDescription` (suffix only).
///
/// False positives are tolerable here: the PHP worker rejects non-TestCase
/// classes at runtime, and the misses (custom frameworks naming their base
/// class `BaseSpec` or whatever) are still handled by walking the graph
/// further up.
fn looks_like_test_case(fqcn: &str) -> bool {
    let last = fqcn.rsplit('\\').next().unwrap_or(fqcn);
    last.ends_with("TestCase")
}

fn is_test_class_via_chain(start_fqcn: &str, graph: &ClassGraph) -> bool {
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut current = start_fqcn;
    while visited.insert(current) {
        let parent = match graph.get(current) {
            Some(Some(p)) => p.as_str(),
            // No parent (no `extends` clause) OR class not in graph — stop walking.
            _ => return false,
        };
        if looks_like_test_case(parent) {
            return true;
        }
        current = parent;
    }
    // visited.insert returned false → we've already seen `current` → cycle.
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    #[test]
    fn setup_with_fixture_builder_is_detected() {
        assert!(setup_builds_fixture_src(
            "protected function setUp(): void { $this->createSchema(); }"
        ));
        assert!(!setup_builds_fixture_src(
            "protected function setUp(): void { $this->seed(); }"
        ));
    }

    #[test]
    fn disqualifier_in_method_is_detected() {
        assert_eq!(
            method_tx_disqualifier_src("function t(){ $this->conn->commit(); }"),
            Some("->commit(".to_string())
        );
        assert_eq!(
            method_tx_disqualifier_src("function t(){ $this->expectException(X::class); }"),
            Some("expectexception".to_string())
        );
        assert_eq!(
            method_tx_disqualifier_src("function t(){ self::assertTrue(true); }"),
            None
        );
    }

    #[test]
    fn shared_fixture_use_marker_reported_file_scope_not() {
        // In-class `use SharedTransactionalFixture;` is the opt-in marker.
        let (_d1, p1) = write_tmp(
            "<?php\nuse Proust\\SharedTransactionalFixture;\n\
             class FooTest extends \\PHPUnit\\Framework\\TestCase {\n\
                 use SharedTransactionalFixture;\n\
                 public function testA() { $this->assertTrue(true); }\n\
             }\n",
        );
        let r1 = shared_fixture_report_in_file(&p1).unwrap();
        assert!(
            r1.iter()
                .any(|c| c.fqcn.ends_with("FooTest") && c.uses_shared_fixture),
            "in-class trait use is reported as uses_shared_fixture"
        );

        // A file-scope import WITHOUT an in-class use must NOT be reported.
        let (_d2, p2) = write_tmp(
            "<?php\nuse Proust\\SharedTransactionalFixture;\n\
             class BarTest extends \\PHPUnit\\Framework\\TestCase {\n\
                 public function testA() { $this->assertTrue(true); }\n\
             }\n",
        );
        let r2 = shared_fixture_report_in_file(&p2).unwrap();
        assert!(
            r2.iter().all(|c| !c.uses_shared_fixture),
            "file-scope import alone is not uses_shared_fixture"
        );
    }

    #[test]
    fn tx_eligible_folds_builder_and_disqualifier_across_chain() {
        // Abstract base builds the fixture in setUp; a clean concrete child is eligible
        // (the builder is folded down the in-project inheritance chain).
        let (_d, p) = write_tmp(
            "<?php\n\
             abstract class BaseTest extends \\PHPUnit\\Framework\\TestCase {\n\
                 protected function setUp(): void { $this->createSchema(); }\n\
             }\n\
             class ChildTest extends BaseTest {\n\
                 public function testReads() { self::assertTrue(true); }\n\
             }\n",
        );
        let r = shared_fixture_report_in_file(&p).unwrap();
        let child = r
            .iter()
            .find(|c| c.fqcn.ends_with("ChildTest"))
            .expect("ChildTest reported");
        assert!(
            child.tx_eligible,
            "parent builder + clean child = eligible: {:?}",
            child.tx_ineligible_reason
        );

        // A committing test disqualifies even with a builder.
        let (_d2, p2) = write_tmp(
            "<?php\n\
             class CommitTest extends \\PHPUnit\\Framework\\TestCase {\n\
                 protected function setUp(): void { $this->createSchema(); }\n\
                 public function testWrites() { $this->conn->commit(); }\n\
             }\n",
        );
        let r2 = shared_fixture_report_in_file(&p2).unwrap();
        let t = r2
            .iter()
            .find(|c| c.fqcn.ends_with("CommitTest"))
            .expect("CommitTest reported");
        assert!(!t.tx_eligible, "a committing test is ineligible");
    }

    #[test]
    fn shared_fixture_report_formats_lines_and_summary() {
        let report = vec![
            SharedFixtureReport {
                fqcn: "A".into(),
                file: PathBuf::from("/p/A.php"),
                uses_shared_fixture: true,
                tx_eligible: true,
                tx_ineligible_reason: None,
            },
            SharedFixtureReport {
                fqcn: "B".into(),
                file: PathBuf::from("/p/B.php"),
                uses_shared_fixture: true,
                tx_eligible: false,
                tx_ineligible_reason: Some("setUp builds no recognised fixture".into()),
            },
        ];
        let out = format_shared_fixture_report(&report);
        assert!(out.contains("A\tuses=yes\teligible=yes"), "{out}");
        assert!(
            out.contains("B\tuses=yes\teligible=no\tsetUp builds no recognised fixture"),
            "{out}"
        );
        assert!(
            out.contains("WARN B uses SharedTransactionalFixture but is ineligible"),
            "{out}"
        );
        assert!(out.contains("eligible: 1/2"), "{out}");
    }

    fn write_tmp(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SomeTest.php");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn group_by_class_or_folds_needs_db_across_methods() {
        let mk = |class: &str, method: &str, needs_db: bool| TestCase {
            file: PathBuf::from("/p/A.php"),
            class: class.into(),
            method: method.into(),
            data_provider: None,
            groups: vec![],
            external_providers: vec![],
            is_tautological: false,
            has_lifecycle_overrides: false,
            depends_on: vec![],
            is_dispatch_safe: true,
            fingerprint: std::collections::HashSet::new(),
            is_stateful: false,
            is_isolated: false,
            needs_db,
            is_functional: false,
        };
        let cases = vec![
            mk("A", "testOne", false),
            mk("A", "testTwo", true),
            mk("B", "testThree", false),
        ];
        let grouped = group_by_class(cases);
        let a = grouped.iter().find(|g| g.class == "A").unwrap();
        let b = grouped.iter().find(|g| g.class == "B").unwrap();
        assert!(
            a.needs_db,
            "any DB-needing method makes the class DB-needing"
        );
        assert!(
            !b.needs_db,
            "a class with no DB method stays needs_db=false"
        );
    }

    #[test]
    fn multi_semicolon_namespace_attributes_classes_to_their_own_namespace() {
        // Regression: a file with TWO semicolon-form `namespace X;` declarations
        // (PHP's sequential form) must attribute each class to the namespace that
        // precedes it in source order — not the first namespace in the file.
        // Real trigger: monolog's IntrospectionProcessorTest.php, which co-locates
        // an `Acme` helper with the real test under `Monolog\Processor`. The bug
        // filed the test class under `Acme\…`, a phantom FQCN that fails to
        // autoload (5 spurious Errors) while the real class was never discovered.
        let src = r#"<?php
namespace Acme;
class Tester {
    public function whoAmI() {}
}
namespace Foo\Bar;
use PHPUnit\Framework\TestCase;
class WidgetTest extends TestCase {
    public function testItWorks() {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let parsed = parse_file_classes(&path).unwrap();
        let fqcns: Vec<&str> = parsed.iter().map(|c| c.fqcn.as_str()).collect();
        assert!(
            fqcns.contains(&"Foo\\Bar\\WidgetTest"),
            "test class must be attributed to its own (second) namespace; got {fqcns:?}"
        );
        assert!(
            !fqcns.contains(&"Acme\\WidgetTest"),
            "test class must NOT inherit the first namespace; got {fqcns:?}"
        );
        assert!(
            fqcns.contains(&"Acme\\Tester"),
            "the helper class stays under the first namespace; got {fqcns:?}"
        );
    }

    #[test]
    fn needs_db_propagates_down_inheritance_chain() {
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use Illuminate\Foundation\Testing\RefreshDatabase;

abstract class DbBaseTest extends TestCase {
    use RefreshDatabase;
}

class ConcreteDbTest extends DbBaseTest {
    public function testOne(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(
            cases.len(),
            1,
            "abstract base emits nothing; concrete subclass emits one"
        );
        assert_eq!(cases[0].class, "App\\ConcreteDbTest");
        assert!(
            cases[0].needs_db,
            "needs_db must OR-fold up the inheritance chain like is_stateful"
        );
    }

    #[test]
    fn is_functional_detects_symfony_base_and_folds_chain() {
        // Direct `extends WebTestCase` is flagged; the chain fold also catches a
        // concrete test reaching a functional base through a non-functional-named
        // intermediate; a plain PHPUnit `TestCase` must NOT be flagged.
        let src = r#"<?php
namespace App\Tests;
use Symfony\Bundle\FrameworkBundle\Test\WebTestCase;
use Symfony\Bundle\FrameworkBundle\Test\KernelTestCase;
use PHPUnit\Framework\TestCase;

class HomeControllerTest extends WebTestCase {
    public function testIndex(): void {}
}

abstract class AbstractKernelTest extends KernelTestCase {}
class ServiceTest extends AbstractKernelTest {
    public function testService(): void {}
}

class PlainUnitTest extends TestCase {
    public function testPure(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let func = |c: &str| cases.iter().find(|x| x.class == c).unwrap().is_functional;
        assert!(
            func("App\\Tests\\HomeControllerTest"),
            "direct extends WebTestCase → functional"
        );
        assert!(
            func("App\\Tests\\ServiceTest"),
            "KernelTestCase via a non-functional-named intermediate → functional (chain fold)"
        );
        assert!(
            !func("App\\Tests\\PlainUnitTest"),
            "plain PHPUnit TestCase → NOT functional"
        );
    }

    #[test]
    fn merge_functional_markers_appends_trimmed_env_extras() {
        let base = merge_functional_markers(None);
        assert!(base.iter().any(|m| m == "WebTestCase"));
        assert!(base.iter().any(|m| m == "KernelTestCase"));
        let extended = merge_functional_markers(Some("IntegrationTestCase, , MyBaseTest "));
        assert!(
            extended.iter().any(|m| m == "IntegrationTestCase"),
            "env extra appended"
        );
        assert!(
            extended.iter().any(|m| m == "MyBaseTest"),
            "whitespace-trimmed env extra appended"
        );
        assert!(
            !extended.iter().any(|m| m.is_empty()),
            "blank entries dropped"
        );
        assert_eq!(
            extended.len(),
            base.len() + 2,
            "only the two non-blank extras are appended"
        );
    }

    #[test]
    fn discovers_test_methods_provided_by_a_trait() {
        // PHPUnit runs test methods a class pulls in via `use SomeTrait;` as
        // tests of that class. Discovery must fold them in (the trait itself is
        // never run as a test class). Covers transitive traits + own-method
        // precedence.
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;

trait NestedTraitTests {
    public function testFromNested(): void {}
}

trait SharedApiTests {
    use NestedTraitTests;
    public function testFromTrait(): void {}
}

class WidgetTest extends TestCase {
    use SharedApiTests;
    public function testOwn(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let mut methods: Vec<&str> = cases
            .iter()
            .filter(|c| c.class == "App\\WidgetTest")
            .map(|c| c.method.as_str())
            .collect();
        methods.sort_unstable();
        assert_eq!(
            methods,
            ["testFromNested", "testFromTrait", "testOwn"],
            "own + direct-trait + transitive-trait methods all discovered under the class"
        );
        // The traits themselves must NOT be emitted as test classes.
        assert!(
            cases.iter().all(|c| c.class == "App\\WidgetTest"),
            "a trait is never a test class on its own"
        );
    }

    #[test]
    fn class_method_overrides_a_trait_method_of_the_same_name() {
        // PHP: a class's own method wins over a trait method of the same name.
        // Discovery must emit it ONCE (no duplicate), under the class.
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;

trait T {
    public function testShared(): void {}
    public function testOnlyInTrait(): void {}
}

class OverrideTest extends TestCase {
    use T;
    public function testShared(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let shared = cases.iter().filter(|c| c.method == "testShared").count();
        assert_eq!(
            shared, 1,
            "the class/trait same-named method is emitted exactly once"
        );
        assert!(
            cases.iter().any(|c| c.method == "testOnlyInTrait"),
            "the trait's other method is still discovered"
        );
        assert_eq!(cases.len(), 2);
    }

    #[test]
    fn has_call_ci_respects_identifier_boundaries() {
        assert!(has_call_ci("$d = Carbon::now();", "now"));
        assert!(has_call_ci("foo->now ();", "now")); // ws before paren
                                                     // `now` embedded in `setTestNow` must NOT match the bare `now` call.
        assert!(!has_call_ci("$this->setTestNow($t);", "now"));
        assert!(has_call_ci("$this->setTestNow($t);", "setTestNow"));
        assert!(has_call_ci(
            "date_default_timezone_set('UTC');",
            "date_default_timezone_set"
        ));
        assert!(!has_call_ci("$x = $rand;", "rand")); // no call (no paren)
    }

    #[test]
    fn setup_ctx_scopes_classifies_context_setters() {
        assert_eq!(setup_ctx_scopes("$this->x = 1;"), Vec::<String>::new());
        assert_eq!(
            setup_ctx_scopes("date_default_timezone_set('America/Toronto');"),
            vec!["tz".to_string()]
        );
        assert_eq!(
            setup_ctx_scopes("Carbon::setTestNow(Carbon::create(2024));"),
            vec!["now".to_string()]
        );
    }

    #[test]
    fn setup_hoist_candidates_extracts_direct_assignments() {
        let c = setup_hoist_candidates_src(
            "$this->schema = HeavySchema::compile(); $this->n = rand(); \
             $this->q->x = 1; $x == 5;",
        );
        // schema: deterministic candidate; n: candidate but nondet; q->x: NOT a candidate.
        assert_eq!(c.len(), 2, "two direct $this->P = assignments");
        assert_eq!(c[0].0, "schema");
        assert_eq!(c[0].1, "HeavySchema::compile()");
        assert!(!c[0].2, "compile() is deterministic");
        assert_eq!(c[1].0, "n");
        assert!(c[1].2, "rand() is non-deterministic");
    }

    #[test]
    fn body_mutated_props_detects_mutation_not_reads() {
        assert_eq!(
            body_mutated_props_src("$this->schema->addTable('x');"),
            vec!["schema".to_string()],
            "non-reader method call mutates"
        );
        assert!(
            body_mutated_props_src("$n = $this->schema->tableCount();").is_empty(),
            "reader-prefixed call does not mutate"
        );
        assert_eq!(
            body_mutated_props_src("$this->schema = null;"),
            vec!["schema".to_string()],
            "reassignment mutates"
        );
        assert!(
            body_mutated_props_src("$x = $this->schema->getName();").is_empty(),
            "getter read does not mutate"
        );
    }

    #[test]
    fn setup_hoist_report_decides_hoist_refuse_context() {
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;

class SchemaReadTest extends TestCase {
    private $schema;
    protected function setUp(): void { $this->schema = HeavySchema::compile(); }
    public function testA(): void { $this->assertSame(4, $this->schema->tableCount()); }
    public function testB(): void { $this->assertTrue($this->schema->hasTable('x')); }
}

class SchemaMutateTest extends TestCase {
    private $schema;
    protected function setUp(): void { $this->schema = HeavySchema::compile(); }
    public function testA(): void { $this->schema->addTable('y'); $this->assertSame(5, $this->schema->tableCount()); }
}

class TzTest extends TestCase {
    private $schema;
    protected function setUp(): void { date_default_timezone_set('America/Toronto'); $this->schema = HeavySchema::compile(); }
    public function testA(): void { $this->assertSame(4, $this->schema->tableCount()); }
}
"#;
        let (_dir, path) = write_tmp(src);
        let report = setup_hoist_report_in_file(&path).unwrap();
        let find = |c: &str| {
            report
                .iter()
                .find(|r| r.fqcn == format!("App\\{c}"))
                .unwrap_or_else(|| panic!("no report for {c}"))
        };
        let read = find("SchemaReadTest");
        assert!(
            read.candidates
                .iter()
                .any(|v| v.prop == "schema" && v.hoistable),
            "read-only deterministic fixture → HOIST"
        );
        assert_eq!(read.test_count, 2);
        let mutate = find("SchemaMutateTest");
        assert!(
            mutate.candidates.iter().all(|v| !v.hoistable),
            "mutated fixture → REFUSE"
        );
        assert!(mutate.candidates[0].reason.contains("mutated"));
        let tz = find("TzTest");
        assert!(
            tz.candidates.iter().all(|v| !v.hoistable),
            "per-test ambient tz → REFUSE"
        );
        assert!(tz.candidates[0].reason.contains("ambient context"));
    }

    #[test]
    fn db_type_reference_alone_does_not_flag_needs_db() {
        // Regression guard (doctrine-orm lesson): detection is opt-in markers
        // ONLY. A class that constructs `new \PDO(...)` and references
        // `Doctrine\ORM\EntityManager` but uses NO marker trait/base-class
        // must NOT be flagged — otherwise the later fail-fast policy would
        // abort large no-DB suites (850 doctrine-orm files reference the ORM).
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use Doctrine\ORM\EntityManager;

class RawDbReferenceTest extends TestCase {
    public function testOne(): void {
        $db = new \PDO('pgsql:host=localhost');
        $em = new EntityManager($conn, $config);
    }
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(cases.len(), 1, "one test method");
        assert!(
            !cases[0].needs_db,
            "raw PDO / Doctrine references without a marker must NOT flag needs_db"
        );
    }

    #[test]
    fn needs_db_detects_configured_base_class() {
        // A class extending a configured marker base-class is needs_db.
        // The DEFAULT base-class list is empty on purpose, so we exercise
        // class_needs_db directly with an explicit configured list.
        let src = r#"<?php
namespace App;

class WidgetTest extends MyFunctionalTestCase {
    public function testOne(): void {}
}
"#;
        let (dir, path) = write_tmp(src);
        let _ = &dir;
        let bytes = std::fs::read(&path).unwrap();
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_php::language_php())
            .unwrap();
        let tree = parser.parse(&bytes, None).unwrap();
        let root = tree.root_node();
        // Find the single class_declaration and its body.
        let mut stack = vec![root];
        let mut flagged = false;
        let mut seen_class = false;
        while let Some(n) = stack.pop() {
            let mut c = n.walk();
            for ch in n.named_children(&mut c) {
                stack.push(ch);
            }
            if n.kind() == "class_declaration" {
                seen_class = true;
                let body = n.child_by_field_name("body").unwrap();
                flagged = class_needs_db(
                    n,
                    body,
                    &bytes,
                    DEFAULT_DB_MARKER_TRAITS,
                    &["MyFunctionalTestCase"],
                );
            }
        }
        assert!(seen_class, "fixture must contain a class declaration");
        assert!(
            flagged,
            "extends a configured marker base-class must flag needs_db"
        );
    }

    #[test]
    fn detects_needs_db_from_marker_trait() {
        let variants = [
            (
                "marker_trait",
                r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use Illuminate\Foundation\Testing\RefreshDatabase;

class TraitDbTest extends TestCase {
    use RefreshDatabase;
    public function testOne(): void {}
}
"#,
                true,
            ),
            (
                "file_scope_use_does_not_trip",
                r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use Illuminate\Foundation\Testing\RefreshDatabase;

class NoTraitUseTest extends TestCase {
    public function testOne(): void {}
}
"#,
                false,
            ),
        ];
        for (label, src, expected) in &variants {
            let (_dir, path) = write_tmp(src);
            let cases = discover_in_file(&path).unwrap();
            assert_eq!(cases.len(), 1, "{label}: expected 1 test case");
            assert_eq!(cases[0].needs_db, *expected, "{label}: needs_db mismatch");
        }
    }

    #[test]
    fn discovered_plain_test_does_not_need_db() {
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
class PlainTest extends TestCase {
    public function testOk(): void { $this->assertTrue(true); }
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(cases.len(), 1, "one test method");
        assert!(
            cases.iter().all(|c| !c.needs_db),
            "plain tests must not need a DB by default"
        );
    }

    #[test]
    fn discovers_a_namespaced_test_class() {
        let src = r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
final class FooTest extends TestCase {
    public function testOne(): void {}
    public function testTwo(): void {}
    public function helper(): void {}
    private function testIsPrivateSoSkipped(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let methods: Vec<_> = cases.iter().map(|c| c.method.as_str()).collect();
        assert_eq!(methods, vec!["testOne", "testTwo"]);
        assert_eq!(cases[0].class, "App\\Tests\\FooTest");
    }

    #[test]
    fn skips_classes_not_extending_testcase() {
        let src = r#"<?php
namespace App;
final class NotATest {
    public function testNothing(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert!(cases.is_empty());
    }

    #[test]
    fn ticket_attribute_and_phpdoc_become_groups() {
        // PHPUnit's #[Ticket] is documented as an alias for #[Group],
        // and @ticket is the PHPDoc equivalent of @group.
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\Ticket;
use PHPUnit\Framework\Attributes\Group;

#[Ticket('GH-1234')]
class WithClassTicket extends TestCase {
    public function testStuff(): void {}
}

class WithMethodTicket extends TestCase {
    #[Ticket('GH-9999')]
    #[Group('regression')]
    public function testMethodTicket(): void {}

    /** @ticket GH-1111 */
    public function testPhpdocTicket(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let by_method: std::collections::HashMap<&str, &TestCase> =
            cases.iter().map(|c| (c.method.as_str(), c)).collect();
        assert!(
            by_method["testStuff"]
                .groups
                .contains(&"GH-1234".to_string()),
            "class-level #[Ticket] becomes a group"
        );
        let mt = &by_method["testMethodTicket"].groups;
        assert!(
            mt.contains(&"GH-9999".to_string()) && mt.contains(&"regression".to_string()),
            "#[Ticket] and #[Group] on the same method both land in groups"
        );
        assert!(
            by_method["testPhpdocTicket"]
                .groups
                .contains(&"GH-1111".to_string()),
            "@ticket PHPDoc becomes a group"
        );
    }

    #[test]
    fn captures_data_provider_from_phpdoc_and_attribute() {
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;

class DpTest extends TestCase {
    /** @dataProvider provideOne */
    public function testWithPhpdoc(int $n): void {}

    #[DataProvider('provideTwo')]
    public function testWithAttribute(int $n): void {}

    #[DataProvider("provideThree")]
    public function testWithAttributeDoubleQuotes(int $n): void {}

    public function testPlain(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let by_method: HashMap<&str, &TestCase> =
            cases.iter().map(|c| (c.method.as_str(), c)).collect();
        assert_eq!(
            by_method["testWithPhpdoc"].data_provider.as_deref(),
            Some("provideOne")
        );
        assert_eq!(
            by_method["testWithAttribute"].data_provider.as_deref(),
            Some("provideTwo")
        );
        assert_eq!(
            by_method["testWithAttributeDoubleQuotes"]
                .data_provider
                .as_deref(),
            Some("provideThree")
        );
        assert_eq!(by_method["testPlain"].data_provider, None);
    }

    #[test]
    fn detects_run_in_separate_process_in_all_four_forms() {
        // Each variant should mark the whole class isolated. We over-isolate
        // (a single annotated method promotes the class) — same trade as
        // is_stateful — because the runner only needs a class-level bit.
        let variants = [
            (
                "phpdoc_class_level",
                r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;

/**
 * @runTestsInSeparateProcesses
 */
class IsolatedClassPhpdoc extends TestCase {
    public function testOne(): void {}
}
"#,
            ),
            (
                "phpdoc_method_level",
                r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;

class IsolatedMethodPhpdoc extends TestCase {
    /** @runInSeparateProcess */
    public function testOne(): void {}
}
"#,
            ),
            (
                "attribute_class_level",
                r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\RunClassInSeparateProcess;

#[RunClassInSeparateProcess]
class IsolatedClassAttr extends TestCase {
    public function testOne(): void {}
}
"#,
            ),
            (
                "attribute_method_level",
                r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\RunInSeparateProcess;

class IsolatedMethodAttr extends TestCase {
    #[RunInSeparateProcess]
    public function testOne(): void {}
}
"#,
            ),
        ];
        for (label, src) in variants {
            let (_dir, path) = write_tmp(src);
            let cases = discover_in_file(&path).unwrap();
            assert_eq!(cases.len(), 1, "{label}: expected one test case");
            assert!(cases[0].is_isolated, "{label}: expected is_isolated=true");
        }
    }

    #[test]
    fn non_isolated_class_is_not_marked_isolated() {
        // A vanilla class with no separate-process marker stays
        // is_isolated=false. Importantly, merely importing the attribute
        // (use statement) must NOT trip the detection.
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\RunInSeparateProcess;

class PlainTest extends TestCase {
    public function testOne(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        assert!(
            !cases[0].is_isolated,
            "merely importing the attribute without applying it must not promote the class"
        );
    }

    #[test]
    fn looks_like_test_case_accepts_known_frameworks() {
        assert!(looks_like_test_case("TestCase"));
        assert!(looks_like_test_case("PHPUnit\\Framework\\TestCase"));
        assert!(looks_like_test_case("My\\Custom\\TestCase"));
        assert!(looks_like_test_case("PHPStan\\Testing\\PHPStanTestCase"));
        assert!(looks_like_test_case(
            "Symfony\\Bundle\\FrameworkBundle\\Test\\KernelTestCase"
        ));
        assert!(looks_like_test_case(
            "Symfony\\Bundle\\FrameworkBundle\\Test\\WebTestCase"
        ));

        assert!(
            !looks_like_test_case("Foo\\TestCases"),
            "plural form rejected"
        );
        assert!(
            !looks_like_test_case("Foo\\TestCaseDescription"),
            "suffix-only rejected"
        );
        assert!(!looks_like_test_case("Foo\\NotTested"));
        assert!(!looks_like_test_case("App\\Service\\OrderService"));
    }

    #[test]
    fn detects_test_attribute_when_stacked_with_other_attributes() {
        // The doctrine-orm pattern: #[Test] immediately followed by
        // #[Group('xyz')] on a method whose name doesn't start with "test".
        // tree-sitter-php groups stacked attributes into one attribute_list
        // node, so an exact text match on "#[Test]" fails.
        let src = r#"<?php
namespace App;
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\Test;
use PHPUnit\Framework\Attributes\Group;

class StackedTest extends TestCase {
    #[Test]
    #[Group('GH7266')]
    public function thisIsActuallyATestDespiteTheName(): void {}

    #[Group('other')]
    #[Test]
    public function reversedOrderAlsoCounts(): void {}

    #[Group('not-a-test')]
    public function plainGroupNoTest(): void {}

    #[TestDox('description')]
    public function testDoxIsNotTheTestAttribute(): void {}

    #[TestWith([1, 2])]
    public function testWithIsAlsoNotTest(int $a, int $b): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let methods: std::collections::BTreeSet<&str> =
            cases.iter().map(|c| c.method.as_str()).collect();
        assert!(
            methods.contains("thisIsActuallyATestDespiteTheName"),
            "missed #[Test] before #[Group]"
        );
        assert!(
            methods.contains("reversedOrderAlsoCounts"),
            "missed #[Test] after #[Group]"
        );
        assert!(
            !methods.contains("plainGroupNoTest"),
            "false positive on plain non-test method"
        );
        // testDoxIsNotTheTestAttribute / testWithIsAlsoNotTest start with
        // "test" so they're picked up by the name rule, not the attribute.
        // That's expected — we just want #[Test]-only methods to be found.
    }

    #[test]
    fn handles_file_without_namespace() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class BareTest extends TestCase {
    public function testStuff(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].class, "BareTest");
    }

    #[test]
    fn discovers_fixture_project_tests() {
        // Fixture lives in the runner crate (where it's used by integration
        // tests too); we walk up to the workspace root and over.
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../runner/fixtures/sample_project/tests");
        let cases = discover_in_dir(&fixture).unwrap();
        let methods: Vec<_> = cases
            .iter()
            .map(|c| (c.class.as_str(), c.method.as_str()))
            .collect();
        assert!(methods.contains(&(
            "Sample\\Tests\\CalculatorTest",
            "testAddsTwoPositiveIntegers"
        )));
        assert!(methods.contains(&("Sample\\Tests\\FailingTest", "testThisDeliberatelyFails")));
        assert_eq!(cases.len(), 12);
    }

    /// The brick/math-style case: a project-local abstract base class extends
    /// PHPUnit\Framework\TestCase, and concrete tests extend that base. The
    /// old single-pass discovery missed these. After fix #4, `discover_in_dir`
    /// must walk the chain and discover the concrete subclass.
    #[test]
    fn discovers_class_extending_intermediate_base_class() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AbstractBaseTest.php"),
            r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
abstract class AbstractBaseTest extends TestCase {
    protected function helper(): void {}
}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ConcreteTest.php"),
            r#"<?php
namespace App\Tests;
final class ConcreteTest extends AbstractBaseTest {
    public function testActual(): void {}
    public function testAnother(): void {}
}
"#,
        )
        .unwrap();

        let cases = discover_in_dir(dir.path()).unwrap();
        let methods: Vec<_> = cases
            .iter()
            .map(|c| (c.class.as_str(), c.method.as_str()))
            .collect();
        // The abstract base class itself must NOT contribute tests.
        assert!(
            !methods
                .iter()
                .any(|(c, _)| *c == "App\\Tests\\AbstractBaseTest"),
            "abstract base class leaked into discovery: {methods:?}"
        );
        // The concrete subclass's two tests must appear.
        assert!(methods.contains(&("App\\Tests\\ConcreteTest", "testActual")));
        assert!(methods.contains(&("App\\Tests\\ConcreteTest", "testAnother")));
        assert_eq!(cases.len(), 2, "unexpected outcomes: {methods:?}");
    }

    /// Defense against malformed input: a class graph with a cycle must not
    /// cause `is_test_class_via_chain` to infinite-loop. Hand-build a graph
    /// where A → B → A and assert the call returns (false) within reasonable
    /// time. (If your BFS is unguarded, this test hangs.)
    #[test]
    fn bfs_handles_cyclic_graph_without_hanging() {
        let mut graph: ClassGraph = HashMap::new();
        graph.insert("App\\A".into(), Some("App\\B".into()));
        graph.insert("App\\B".into(), Some("App\\A".into()));
        // Neither class is a TestCase; the chain cycles back without reaching one.
        assert!(!is_test_class_via_chain("App\\A", &graph));
    }

    #[test]
    fn group_by_class_collapses_per_method_cases() {
        let cases = vec![
            TestCase {
                file: PathBuf::from("/p/A.php"),
                class: "A".into(),
                method: "testOne".into(),
                data_provider: None,
                groups: vec![],
                external_providers: vec![],
                is_tautological: false,
                has_lifecycle_overrides: false,
                depends_on: vec![],
                is_dispatch_safe: true,
                fingerprint: std::collections::HashSet::new(),
                is_stateful: false,
                is_isolated: false,
                needs_db: false,
                is_functional: false,
            },
            TestCase {
                file: PathBuf::from("/p/A.php"),
                class: "A".into(),
                method: "testTwo".into(),
                data_provider: None,
                groups: vec![],
                external_providers: vec![],
                is_tautological: false,
                has_lifecycle_overrides: false,
                depends_on: vec![],
                is_dispatch_safe: true,
                fingerprint: std::collections::HashSet::new(),
                is_stateful: false,
                is_isolated: false,
                needs_db: false,
                is_functional: false,
            },
            TestCase {
                file: PathBuf::from("/p/B.php"),
                class: "B".into(),
                method: "testThree".into(),
                data_provider: None,
                groups: vec![],
                external_providers: vec![],
                is_tautological: false,
                has_lifecycle_overrides: false,
                depends_on: vec![],
                is_dispatch_safe: true,
                fingerprint: std::collections::HashSet::new(),
                is_stateful: false,
                is_isolated: false,
                needs_db: false,
                is_functional: false,
            },
        ];
        let grouped = group_by_class(cases);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].class, "A");
        let names_a: Vec<&str> = grouped[0].methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names_a, vec!["testOne", "testTwo"]);
        assert_eq!(grouped[1].class, "B");
        let names_b: Vec<&str> = grouped[1].methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names_b, vec!["testThree"]);
    }

    #[test]
    fn group_by_class_keeps_same_fqcn_from_different_files_separate() {
        // PHPUnit's own end-to-end fixtures declare the same FQCN in multiple
        // sub-directories (each with its own phpunit.xml). grouping by (file,
        // class) prevents method names from one file bleeding into the batch
        // for another file, which previously caused ReflectionException crashes
        // when --workers 1 serialised all batches through one PHP process.
        let cases = vec![
            TestCase {
                file: PathBuf::from("/fix/IssueTriggerResolverTest.php"),
                class: "Ns\\Foo".into(),
                method: "testDeprecation".into(),
                data_provider: None,
                groups: vec![],
                external_providers: vec![],
                is_tautological: false,
                has_lifecycle_overrides: false,
                depends_on: vec![],
                is_dispatch_safe: true,
                fingerprint: std::collections::HashSet::new(),
                is_stateful: false,
                is_isolated: false,
                needs_db: false,
                is_functional: false,
            },
            TestCase {
                file: PathBuf::from("/invalid-class/IssueTriggerResolverTest.php"),
                class: "Ns\\Foo".into(),
                method: "testSomething".into(),
                data_provider: None,
                groups: vec![],
                external_providers: vec![],
                is_tautological: false,
                has_lifecycle_overrides: false,
                depends_on: vec![],
                is_dispatch_safe: true,
                fingerprint: std::collections::HashSet::new(),
                is_stateful: false,
                is_isolated: false,
                needs_db: false,
                is_functional: false,
            },
            TestCase {
                file: PathBuf::from("/nonexistent-class/IssueTriggerResolverTest.php"),
                class: "Ns\\Foo".into(),
                method: "testSomething".into(),
                data_provider: None,
                groups: vec![],
                external_providers: vec![],
                is_tautological: false,
                has_lifecycle_overrides: false,
                depends_on: vec![],
                is_dispatch_safe: true,
                fingerprint: std::collections::HashSet::new(),
                is_stateful: false,
                is_isolated: false,
                needs_db: false,
                is_functional: false,
            },
        ];
        let grouped = group_by_class(cases);
        assert_eq!(
            grouped.len(),
            3,
            "each (file, class) pair must be its own TestClass"
        );
        assert_eq!(grouped[0].methods[0].name, "testDeprecation");
        assert_eq!(grouped[1].methods[0].name, "testSomething");
        assert_eq!(grouped[2].methods[0].name, "testSomething");
    }

    #[test]
    fn parse_external_provider_attr_text_short_class() {
        let aliases: HashMap<String, String> = [(
            "AssertSize".to_string(),
            "PHPUnit\\Tests\\AssertSize".to_string(),
        )]
        .into_iter()
        .collect();
        let text = "#[DataProviderExternal(AssertSize::class, 'providerMethod')]";
        let got = parse_external_provider_attr_text(text, Some("PHPUnit\\Tests"), &aliases);
        assert_eq!(
            got,
            vec![(
                "PHPUnit\\Tests\\AssertSize".to_string(),
                "providerMethod".to_string()
            )]
        );
    }

    #[test]
    fn parse_external_provider_attr_text_fqcn() {
        let aliases = HashMap::new();
        let text =
            "#[DataProviderExternal(PHPUnit\\Framework\\ProviderClass::class, \"myProvider\")]";
        let got = parse_external_provider_attr_text(text, None, &aliases);
        assert_eq!(
            got,
            vec![(
                "PHPUnit\\Framework\\ProviderClass".to_string(),
                "myProvider".to_string()
            )]
        );
    }

    #[test]
    fn parse_external_provider_does_not_match_regular_data_provider() {
        let aliases = HashMap::new();
        let text = "#[DataProvider('localProvider')]";
        let got = parse_external_provider_attr_text(text, None, &aliases);
        assert!(got.is_empty());
    }

    #[test]
    fn discovers_external_provider_on_method() {
        let src = r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
use App\Data\Provider as DataProv;
class FooTest extends TestCase {
    #[DataProviderExternal(DataProv::class, 'rows')]
    public function testWithExternal(int $x): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(
            cases[0].external_providers,
            vec![("App\\Data\\Provider".to_string(), "rows".to_string())]
        );
    }

    #[test]
    fn parse_external_provider_attr_text_absolute_fqcn() {
        // #[\DataProviderExternal(...)] — leading backslash (absolute FQCN attribute form)
        let aliases = HashMap::new();
        let text = "#[\\DataProviderExternal(Foo\\Bar::class, 'myMethod')]";
        let got = parse_external_provider_attr_text(text, None, &aliases);
        assert_eq!(got, vec![("Foo\\Bar".to_string(), "myMethod".to_string())]);
    }

    #[test]
    fn parse_external_provider_attr_text_multiple_providers() {
        let aliases = HashMap::new();
        let text = "#[DataProviderExternal(ClassA::class, 'p1'), DataProviderExternal(ClassB::class, 'p2')]";
        let mut got = parse_external_provider_attr_text(text, None, &aliases);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("ClassA".to_string(), "p1".to_string()),
                ("ClassB".to_string(), "p2".to_string()),
            ]
        );
    }

    #[test]
    fn discovers_external_provider_inherited_method() {
        // Method with DataProviderExternal lives on abstract parent; discovered via concrete subclass.
        let src = r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
use App\Data\Provider as DataProv;
abstract class BaseTest extends TestCase {
    #[DataProviderExternal(DataProv::class, 'rows')]
    public function testWithExternal(int $x): void {}
}
class ConcreteTest extends BaseTest {}
"#;
        // write_tmp creates a single file; discover_in_file handles same-file inheritance.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ConcreteTest.php");
        std::fs::write(&path, src).unwrap();
        let cases = discover_in_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].class, "App\\Tests\\ConcreteTest");
        assert_eq!(
            cases[0].external_providers,
            vec![("App\\Data\\Provider".to_string(), "rows".to_string())]
        );
    }

    #[test]
    fn discover_nontest_class_index_indexes_only_nontest_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/Helper.php"),
            "<?php\nnamespace App;\nclass Helper {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/FooTest.php"),
            "<?php\nnamespace App;\nclass FooTest {}\n",
        )
        .unwrap();
        let idx = discover_nontest_class_index(&[dir.join("src")], &[]);
        assert!(
            idx.contains_key("App\\Helper"),
            "non-test class must be indexed"
        );
        assert!(
            !idx.contains_key("App\\FooTest"),
            "*Test* file must be skipped"
        );
    }

    #[test]
    fn discover_class_file_index_finds_classes() {
        let src = r#"<?php
namespace App\Data;
class Provider {
    public static function rows(): array { return []; }
}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Provider.php");
        std::fs::write(&path, src).unwrap();
        let idx = discover_class_file_index(&[dir.path().to_path_buf()]);
        assert_eq!(idx.get("App\\Data\\Provider"), Some(&path));
    }

    #[test]
    fn lifecycle_overrides_detected_correctly() {
        // has_lifecycle_overrides is true ONLY when setUpBeforeClass or
        // tearDownAfterClass is overridden. setUp body content is irrelevant.
        let src = r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;

class PlainTest extends TestCase {
    public function setUp(): void {
        $this->db = new Database();
        $this->db->connect();   // complex setUp — no longer blocks dispatch
    }
    public function testA(): void {}
}

class NoSetUpTest extends TestCase {
    public function testB(): void {}
}

class HasBeforeClassTest extends TestCase {
    public static function setUpBeforeClass(): void { /* shared state */ }
    public function testC(): void {}
}

class HasAfterClassTest extends TestCase {
    public static function tearDownAfterClass(): void { /* shared cleanup */ }
    public function testD(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let grouped = group_by_class(cases);
        let by_class: std::collections::HashMap<&str, &TestClass> =
            grouped.iter().map(|g| (g.class.as_str(), g)).collect();

        assert!(
            !by_class["App\\Tests\\PlainTest"].has_lifecycle_overrides,
            "complex setUp body without lifecycle hooks must not block dispatch"
        );
        assert!(
            !by_class["App\\Tests\\NoSetUpTest"].has_lifecycle_overrides,
            "no setUp at all must not block dispatch"
        );
        assert!(
            by_class["App\\Tests\\HasBeforeClassTest"].has_lifecycle_overrides,
            "setUpBeforeClass must set has_lifecycle_overrides"
        );
        assert!(
            by_class["App\\Tests\\HasAfterClassTest"].has_lifecycle_overrides,
            "tearDownAfterClass must set has_lifecycle_overrides"
        );
    }

    #[test]
    fn depends_detection_marks_methods_not_dispatch_safe() {
        let src = r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\Depends;

class DependsTest extends TestCase {
    public function testFirst(): void {}

    #[Depends('testFirst')]
    public function testSecond(): void {}

    /** @depends testFirst */
    public function testThirdPhpdoc(): void {}

    public function testIndependent(): void {}
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let grouped = group_by_class(cases);
        let methods: std::collections::HashMap<&str, bool> = grouped[0]
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m.is_dispatch_safe))
            .collect();

        assert!(methods["testFirst"], "no depends → dispatch safe");
        assert!(!methods["testSecond"], "#[Depends] → not dispatch safe");
        assert!(!methods["testThirdPhpdoc"], "@depends → not dispatch safe");
        assert!(methods["testIndependent"], "no depends → dispatch safe");
    }

    #[test]
    fn tautology_detection_marks_trivially_true_methods() {
        let src = r#"<?php
namespace App\Tests;
use PHPUnit\Framework\TestCase;
class TautologyTest extends TestCase {
    public function testTrivialTrue(): void {
        $this->assertTrue(true);
    }
    public function testTrivialFalse(): void {
        $this->assertFalse(false);
    }
    public function testTrivialNull(): void {
        $this->assertNull(null);
    }
    public function testTrivialEquals(): void {
        $this->assertEquals(42, 42);
    }
    public function testTrivialSame(): void {
        $this->assertSame('hello', 'hello');
    }
    public function testRealAssert(): void {
        $value = 1 + 1;
        $this->assertEquals(2, $value);
    }
    public function testAssertsTrue(): void {
        $this->assertTrue(false);
    }
    public function testNoAssertions(): void {
        // intentionally empty
    }
    public function testVarEquals(): void {
        $x = compute();
        $this->assertEquals($x, $x);
    }
}
"#;
        let (_dir, path) = write_tmp(src);
        let cases = discover_in_file(&path).unwrap();
        let by_method: std::collections::HashMap<&str, &TestCase> =
            cases.iter().map(|c| (c.method.as_str(), c)).collect();

        assert!(
            by_method["testTrivialTrue"].is_tautological,
            "assertTrue(true) must be tautological"
        );
        assert!(
            by_method["testTrivialFalse"].is_tautological,
            "assertFalse(false) must be tautological"
        );
        assert!(
            by_method["testTrivialNull"].is_tautological,
            "assertNull(null) must be tautological"
        );
        assert!(
            by_method["testTrivialEquals"].is_tautological,
            "assertEquals(42,42) must be tautological"
        );
        assert!(
            by_method["testTrivialSame"].is_tautological,
            "assertSame('hello','hello') must be tautological"
        );

        assert!(
            !by_method["testRealAssert"].is_tautological,
            "body with assignment must NOT be tautological"
        );
        assert!(
            !by_method["testAssertsTrue"].is_tautological,
            "assertTrue(false) must NOT be tautological"
        );
        assert!(
            !by_method["testNoAssertions"].is_tautological,
            "zero assertions must NOT be tautological"
        );
        assert!(
            !by_method["testVarEquals"].is_tautological,
            "assertEquals with variables must NOT be tautological"
        );
    }

    /// tree-sitter is error-recovering: a broken file still yields a partial
    /// tree, so classes/methods can be silently dropped. `has_syntax_errors`
    /// is the predicate that lets us surface that to the user. It must be true
    /// for broken input and false for valid input.
    #[test]
    fn detects_syntax_errors_in_broken_php() {
        // Unclosed method body — tree-sitter parses with error recovery but
        // flags the tree as containing an ERROR node.
        let broken = "<?php class Foo { public function bar() { ";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_php::language_php())
            .unwrap();
        let tree = parser.parse(broken, None).unwrap();
        assert!(
            tree.root_node().has_error(),
            "tree-sitter should flag the broken snippet as containing an ERROR node",
        );
        assert!(
            has_syntax_errors(&tree),
            "helper must report broken input as having errors"
        );

        let valid = "<?php class Foo { public function bar(): void {} }";
        let tree_ok = parser.parse(valid, None).unwrap();
        assert!(
            !has_syntax_errors(&tree_ok),
            "helper must report valid input as clean"
        );
    }

    /// M10: a PHP file with a non-UTF-8 byte (latin-1 text, a binary heredoc)
    /// must still be parsed lossily and its tests discovered — `read_to_string`
    /// would error on it and the caller's `unwrap_or_default()` would silently
    /// drop every test in the file (a test-count parity violation).
    #[test]
    fn discovers_tests_in_non_utf8_php() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("LatinTest.php");
        // 0xE9 is latin-1 'é' — invalid UTF-8, which `read_to_string` rejects.
        let content: &[u8] = b"<?php\nclass LatinTest extends TestCase {\n    // caf\xe9 latin-1\n    public function testBar(): void {}\n}\n";
        std::fs::write(&path, content).unwrap();
        let classes =
            parse_file_classes(&path).expect("non-UTF-8 PHP must parse lossily, not error out");
        assert!(
            classes.iter().any(|c| c.fqcn == "LatinTest"),
            "LatinTest must be discovered despite the non-UTF-8 byte; got {:?}",
            classes.iter().map(|c| &c.fqcn).collect::<Vec<_>>()
        );
    }

    #[test]
    fn discover_cases_and_test_index_includes_colocated_helper_classes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        // A *Test* file defining BOTH a test class AND a co-located non-test helper.
        std::fs::write(
            dir.join("tests/FooTest.php"),
            "<?php\nnamespace App\\Tests;\nuse PHPUnit\\Framework\\TestCase;\nclass FooHelper {}\nclass FooTest extends TestCase { public function testBar() { $this->assertTrue(true); } }\n",
        )
        .unwrap();
        let roots = vec![dir.join("tests")];
        let (cases, index) = discover_cases_and_test_index(&roots, &[], &[]).unwrap();
        // Only the test class is emitted...
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].class, "App\\Tests\\FooTest");
        // ...but the index captures BOTH the test class and the co-located helper.
        assert!(index.contains_key("App\\Tests\\FooTest"));
        assert!(
            index.contains_key("App\\Tests\\FooHelper"),
            "co-located helper class must be in the test-file index"
        );
    }

    /// Overlapping roots (a phpunit.xml declaring both `tests/` and a nested
    /// `tests/unit/`) must not double-count: `discover_in_dirs` is the runner's
    /// fast path and `discover_with_index` is the full path — if the former
    /// re-visits files under the nested dir while the latter dedups across walks,
    /// the two emit different test counts → rust count > vanilla (parity failure).
    #[test]
    fn discover_in_dirs_dedups_overlapping_roots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(
            dir.path().join("a/b/FooTest.php"),
            "<?php\nclass FooTest extends \\PHPUnit\\Framework\\TestCase {\n    public function testX() {}\n}\n",
        )
        .unwrap();
        let roots = vec![dir.path().join("a"), dir.path().join("a/b")];

        let cases_in_dirs = discover_in_dirs(&roots, &[], &[]).unwrap();
        let (cases_with_index, _) = discover_with_index(&roots, &[], &[]).unwrap();

        assert_eq!(
            cases_in_dirs.len(),
            cases_with_index.len(),
            "fast-path emission must match full parse"
        );
        assert_eq!(cases_in_dirs.len(), 1);
    }
}
