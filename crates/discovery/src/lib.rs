//! Shared PHPUnit test discovery.
//!
//! Walks PHP source directories with tree-sitter, builds the class
//! inheritance graph, and emits one [`TestCase`] per discovered test method.
//! Recognises the `testXxx` naming convention, `/** @test */` PHPDoc,
//! `#[Test]` attribute, plus `#[DataProvider]` / `@dataProvider` markers
//! per method.
//!
//! Used by both the phpunit-rust runner (for dispatch) and the analyzer
//! (for coverage tracing entry points).

use anyhow::{anyhow, Context, Result};
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
    pub file:               PathBuf,
    pub class:              String,
    pub method:             String,
    pub data_provider:      Option<String>,
    pub groups:             Vec<String>,
    pub external_providers: Vec<(String, String)>,
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
}

/// Per-method discovery info collected during the tree-sitter walk.
#[derive(Debug, Clone)]
struct MethodInfo {
    name:               String,
    data_provider:      Option<String>,
    groups:             Vec<String>,
    external_providers: Vec<(String, String)>,
}

/// Maps every discovered class FQCN to its resolved parent FQCN (or None).
/// Used by the BFS to decide whether a class reaches TestCase.
type ClassGraph = HashMap<String, Option<String>>;

/// A test method within a discovered class, with its optional data-provider
/// reference. The runner uses `data_provider` to look up row counts and
/// schedule heavy methods first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedMethod {
    pub name:               String,
    pub data_provider:      Option<String>,
    pub groups:             Vec<String>,
    pub external_providers: Vec<(String, String)>,
}

/// A discovered test class with all of its methods, grouped for batched
/// dispatch (one request per class to the worker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestClass {
    pub file:    PathBuf,
    pub class:   String,
    pub methods: Vec<GroupedMethod>,
}

/// Group a flat list of TestCases by class. Preserves discovery order
/// and per-method data-provider attribution.
pub fn group_by_class(cases: Vec<TestCase>) -> Vec<TestClass> {
    let mut groups: Vec<TestClass> = Vec::new();
    for case in cases {
        let gm = GroupedMethod {
            name:               case.method,
            data_provider:      case.data_provider,
            groups:             case.groups,
            external_providers: case.external_providers,
        };
        if let Some(existing) = groups.iter_mut().find(|g| g.class == case.class) {
            existing.methods.push(gm);
        } else {
            groups.push(TestClass {
                file:    case.file,
                class:   case.class,
                methods: vec![gm],
            });
        }
    }
    groups
}

/// Pass 1 (per file): parse a PHP source file and return every class
/// declaration in it, with its resolved parent FQCN and test methods.
///
/// "Resolved parent FQCN" applies the file's namespace + use-alias context so
/// the BFS in pass 3 can compare on FQCN strings alone.
fn parse_file_classes(path: &Path) -> Result<Vec<ParsedClass>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_php::language_php())
        .context("setting tree-sitter-php language")?;
    let tree = parser
        .parse(&src, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {}", path.display()))?;

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
                        imported = cc.utf8_text(bytes).ok().map(|s| s.trim_start_matches('\\').to_string());
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
                let local = alias.unwrap_or_else(|| {
                    fqcn.rsplit('\\').next().unwrap_or(&fqcn).to_string()
                });
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

fn collect_parsed_classes(
    root: Node,
    bytes: &[u8],
    namespace: Option<&str>,
    aliases: &HashMap<String, String>,
    path: &Path,
    out: &mut Vec<ParsedClass>,
) -> Result<()> {
    let query_src = r#"
        (class_declaration
          name: (name) @class_name
          (base_clause [(name) (qualified_name)] @base)?
          body: (declaration_list) @body) @class
    "#;
    let lang = tree_sitter_php::language_php();
    let query = Query::new(&lang, query_src).context("compiling class query")?;
    let mut cursor = QueryCursor::new();
    let captures = query.capture_names();
    let class_idx = captures.iter().position(|n| *n == "class").unwrap();
    let class_name_idx = captures.iter().position(|n| *n == "class_name").unwrap();
    let base_idx = captures.iter().position(|n| *n == "base").unwrap();
    let body_idx = captures.iter().position(|n| *n == "body").unwrap();

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

        let (Some(name), Some(body), Some(decl)) = (class_name, body_node, class_node) else { continue };

        let fqcn = match namespace {
            Some(ns) => format!("{ns}\\{name}"),
            None => name.to_string(),
        };
        let parent_fqcn = base_name.map(|b| resolve_class_reference(b, namespace, aliases));
        let test_methods = collect_test_methods(body, bytes, namespace.as_deref(), &aliases);
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

        out.push(ParsedClass {
            file: path.to_path_buf(),
            fqcn,
            parent_fqcn,
            test_methods,
            class_groups,
            is_abstract,
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
        let has_annotation = prev_comment.as_deref()
            .map(|c| c.contains("@test"))
            .unwrap_or(false);
        let has_attr = method_has_test_attribute(child, bytes);

        if is_public && (name.starts_with("test") || has_annotation || has_attr) {
            let dp = prev_comment.as_deref()
                .and_then(phpdoc_data_provider)
                .or_else(|| method_data_provider_attr(child, bytes));
            let mut groups = extract_groups_attr(child, bytes);
            if let Some(c) = prev_comment.as_deref() {
                extract_groups_phpdoc(c, &mut groups);
            }
            let external_providers = extract_external_provider_attrs(child, bytes, namespace, aliases);
            methods.push(MethodInfo {
                name:               name.to_string(),
                data_provider:      dp,
                groups,
                external_providers,
            });
        }
        prev_comment = None;
    }
    methods
}

/// Extract `name` from a `@dataProvider name` annotation in a PHPDoc block.
/// Handles single-line (`/** @dataProvider foo */`) and multi-line forms
/// alike by searching the whole comment text. Returns the first match.
fn phpdoc_data_provider(comment: &str) -> Option<String> {
    let needle = "@dataProvider ";
    let start = comment.find(needle)?;
    let after = &comment[start + needle.len()..];
    let name = after.split(|c: char| c.is_whitespace() || c == '*').next()?;
    if name.is_empty() { None } else { Some(name.to_string()) }
}

/// Extract the provider name from `#[DataProvider("name")]` (also matches
/// the fully-qualified `#[PHPUnit\Framework\Attributes\DataProvider(...)]`).
/// Single or double quotes accepted.
fn method_data_provider_attr(method: Node, bytes: &[u8]) -> Option<String> {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        let Ok(text) = child.utf8_text(bytes) else { continue };
        if !text.starts_with("#[") { continue; }
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
        let before_ok = abs == 0 || matches!(
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
                let after_comma = after_class.trim_start_matches(|c: char| {
                    c == ',' || c.is_whitespace()
                });
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
        let Ok(text) = child.utf8_text(bytes) else { continue };
        if !text.starts_with("#[") { continue; }
        out.extend(parse_external_provider_attr_text(text, namespace, aliases));
    }
    out
}

fn method_has_test_attribute(method: Node, bytes: &[u8]) -> bool {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        let Ok(text) = child.utf8_text(bytes) else { continue };
        if !text.starts_with("#[") { continue; }
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
        let Ok(text) = child.utf8_text(bytes) else { continue };
        if !text.starts_with("#[") { continue; }
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
        let before_ok = abs == 0 || matches!(
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
            } else { None };
            if let Some(name) = parsed { out.push(name.to_string()); }
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
        if child.id() == node.id() { break; }
        prev = Some(child);
    }
    let prev = prev?;
    if prev.kind() != "comment" { return None; }
    let text = prev.utf8_text(bytes).ok()?;
    if !text.contains("/**") { return None; }
    Some(text.to_string())
}

/// Extract group names from PHPDoc lines. PHPUnit recognises both
/// `@group name` and `@ticket name` (which is just an alias).
fn extract_groups_phpdoc(comment: &str, into: &mut Vec<String>) {
    for line in comment.lines() {
        let trimmed = line.trim_start_matches(|c: char|
            c == '*' || c == '/' || c.is_whitespace()
        );
        for prefix in ["@group ", "@ticket "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name = rest.split_whitespace().next().unwrap_or("");
                if !name.is_empty() { into.push(name.to_string()); }
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
    if needle.is_empty() || haystack.len() < needle.len() { return false; }
    let is_boundary_before = |b: u8| matches!(b, b'[' | b',' | b'\\' | b' ' | b'\t' | b'\n' | b'\r');
    let is_boundary_after  = |b: u8| matches!(b, b']' | b',' | b'(' | b' ' | b'\t' | b'\n' | b'\r');
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            let before_ok = i == 0 || is_boundary_before(haystack[i - 1]);
            let after_idx = i + needle.len();
            let after_ok = after_idx == haystack.len() || is_boundary_after(haystack[after_idx]);
            if before_ok && after_ok { return true; }
        }
        i += 1;
    }
    false
}

fn method_is_public(method: Node, bytes: &[u8]) -> bool {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child.utf8_text(bytes).map(|t| t == "public").unwrap_or(false);
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
fn emit_test_cases(parsed: &[ParsedClass], graph: &ClassGraph) -> Result<Vec<TestCase>> {
    // Index by FQCN for chain-walking.
    let by_fqcn: HashMap<&str, &ParsedClass> =
        parsed.iter().map(|c| (c.fqcn.as_str(), c)).collect();

    let mut cases = Vec::new();
    for class in parsed {
        if class.is_abstract {
            continue;
        }
        if !is_test_class_via_chain(&class.fqcn, graph) {
            continue;
        }
        // Collect methods from this class + all parsed ancestors, dedup by name.
        // The PHP runtime will run inherited methods on the concrete subclass,
        // so we emit them under `class.fqcn` (not the ancestor's FQCN).
        // PHP method names are case-insensitive at call time: a subclass
        // declaring `testfoo` overrides a parent's `testFoo`. PHPUnit's
        // reflection-based discovery collapses them; we mirror that by
        // deduping on the lowercased name as we walk the inheritance chain.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut visit = class.fqcn.as_str();
        let mut depth = 0;
        while depth < 32 {
            if let Some(c) = by_fqcn.get(visit) {
                for mi in &c.test_methods {
                    if seen.insert(mi.name.to_ascii_lowercase()) {
                        // Effective groups = the concrete subclass's
                        // class-level groups + the inherited method's
                        // class-level groups + the method's own groups.
                        // Dedup with a btreeset to keep output stable.
                        let mut groups: std::collections::BTreeSet<String> =
                            class.class_groups.iter().cloned().collect();
                        groups.extend(c.class_groups.iter().cloned());
                        groups.extend(mi.groups.iter().cloned());
                        cases.push(TestCase {
                            file:               class.file.clone(),
                            class:              class.fqcn.clone(),
                            method:             mi.name.clone(),
                            data_provider:      mi.data_provider.clone(),
                            groups:             groups.into_iter().collect(),
                            external_providers: mi.external_providers.clone(),
                        });
                    }
                }
                match c.parent_fqcn.as_deref() {
                    Some(p) => visit = p,
                    None => break,
                }
            } else {
                // Parent is outside our parsed set (e.g., PHPUnit's TestCase).
                break;
            }
            depth += 1;
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
pub fn discover_in_dirs(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    graph_supplement_dirs: &[PathBuf],
) -> Result<Vec<TestCase>> {
    // Canonicalize excludes once so prefix checks are robust.
    let canon_excludes: Vec<PathBuf> = excludes
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();

    // Pass 1: parse every *Test*.php file across all testsuite roots.
    let mut parsed: Vec<ParsedClass> = Vec::new();
    for root in roots {
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("php") {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.contains("Test") {
                continue;
            }
            // Apply excludes (prefix match on canonicalized path).
            if let Ok(canon) = p.canonicalize() {
                if canon_excludes.iter().any(|ex| canon.starts_with(ex)) {
                    continue;
                }
            }
            parsed.extend(parse_file_classes(p)?);
        }
    }
    let emit_count = parsed.len(); // classes from roots — those we'll emit TestCases for

    // Supplement: parse *Test*.php files from autoload-dev dirs to enrich the
    // class graph with abstract base classes that live outside the testsuite
    // directories (e.g. Carbon's tests/AbstractTestCase.php).
    let parsed_paths: HashSet<PathBuf> = parsed.iter().map(|c| c.file.clone()).collect();
    for dir in graph_supplement_dirs {
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("php") { continue; }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.contains("Test") { continue; }
            if parsed_paths.contains(p) { continue; }
            parsed.extend(parse_file_classes(p)?);
        }
    }

    // Pass 2: build the inheritance graph (FQCN -> parent FQCN or None).
    let graph: ClassGraph = parsed
        .iter()
        .map(|c| (c.fqcn.clone(), c.parent_fqcn.clone()))
        .collect();

    // Pass 3: emit test methods only for classes from the testsuite roots.
    emit_test_cases(&parsed[..emit_count], &graph)
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

    fn write_tmp(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SomeTest.php");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, path)
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
        assert!(by_method["testStuff"].groups.contains(&"GH-1234".to_string()),
            "class-level #[Ticket] becomes a group");
        let mt = &by_method["testMethodTicket"].groups;
        assert!(mt.contains(&"GH-9999".to_string()) && mt.contains(&"regression".to_string()),
            "#[Ticket] and #[Group] on the same method both land in groups");
        assert!(by_method["testPhpdocTicket"].groups.contains(&"GH-1111".to_string()),
            "@ticket PHPDoc becomes a group");
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
        assert_eq!(by_method["testWithPhpdoc"].data_provider.as_deref(),            Some("provideOne"));
        assert_eq!(by_method["testWithAttribute"].data_provider.as_deref(),         Some("provideTwo"));
        assert_eq!(by_method["testWithAttributeDoubleQuotes"].data_provider.as_deref(), Some("provideThree"));
        assert_eq!(by_method["testPlain"].data_provider, None);
    }

    #[test]
    fn looks_like_test_case_accepts_known_frameworks() {
        assert!(looks_like_test_case("TestCase"));
        assert!(looks_like_test_case("PHPUnit\\Framework\\TestCase"));
        assert!(looks_like_test_case("My\\Custom\\TestCase"));
        assert!(looks_like_test_case("PHPStan\\Testing\\PHPStanTestCase"));
        assert!(looks_like_test_case("Symfony\\Bundle\\FrameworkBundle\\Test\\KernelTestCase"));
        assert!(looks_like_test_case("Symfony\\Bundle\\FrameworkBundle\\Test\\WebTestCase"));

        assert!(!looks_like_test_case("Foo\\TestCases"), "plural form rejected");
        assert!(!looks_like_test_case("Foo\\TestCaseDescription"), "suffix-only rejected");
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
        let methods: std::collections::BTreeSet<&str> = cases.iter()
            .map(|c| c.method.as_str()).collect();
        assert!(methods.contains("thisIsActuallyATestDespiteTheName"),
            "missed #[Test] before #[Group]");
        assert!(methods.contains("reversedOrderAlsoCounts"),
            "missed #[Test] after #[Group]");
        assert!(!methods.contains("plainGroupNoTest"),
            "false positive on plain non-test method");
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
        let methods: Vec<_> = cases.iter().map(|c| (c.class.as_str(), c.method.as_str())).collect();
        assert!(methods.contains(&("Sample\\Tests\\CalculatorTest", "testAddsTwoPositiveIntegers")));
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
        ).unwrap();
        std::fs::write(
            dir.path().join("ConcreteTest.php"),
            r#"<?php
namespace App\Tests;
final class ConcreteTest extends AbstractBaseTest {
    public function testActual(): void {}
    public function testAnother(): void {}
}
"#,
        ).unwrap();

        let cases = discover_in_dir(dir.path()).unwrap();
        let methods: Vec<_> = cases
            .iter()
            .map(|c| (c.class.as_str(), c.method.as_str()))
            .collect();
        // The abstract base class itself must NOT contribute tests.
        assert!(!methods.iter().any(|(c, _)| *c == "App\\Tests\\AbstractBaseTest"),
            "abstract base class leaked into discovery: {methods:?}");
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
            TestCase { file: PathBuf::from("/p/A.php"), class: "A".into(), method: "testOne".into(),   data_provider: None, groups: vec![], external_providers: vec![] },
            TestCase { file: PathBuf::from("/p/A.php"), class: "A".into(), method: "testTwo".into(),   data_provider: None, groups: vec![], external_providers: vec![] },
            TestCase { file: PathBuf::from("/p/B.php"), class: "B".into(), method: "testThree".into(), data_provider: None, groups: vec![], external_providers: vec![] },
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
    fn parse_external_provider_attr_text_short_class() {
        let aliases: HashMap<String, String> = [
            ("AssertSize".to_string(), "PHPUnit\\Tests\\AssertSize".to_string()),
        ].into_iter().collect();
        let text = "#[DataProviderExternal(AssertSize::class, 'providerMethod')]";
        let got = parse_external_provider_attr_text(text, Some("PHPUnit\\Tests"), &aliases);
        assert_eq!(got, vec![("PHPUnit\\Tests\\AssertSize".to_string(), "providerMethod".to_string())]);
    }

    #[test]
    fn parse_external_provider_attr_text_fqcn() {
        let aliases = HashMap::new();
        let text = "#[DataProviderExternal(PHPUnit\\Framework\\ProviderClass::class, \"myProvider\")]";
        let got = parse_external_provider_attr_text(text, None, &aliases);
        assert_eq!(got, vec![("PHPUnit\\Framework\\ProviderClass".to_string(), "myProvider".to_string())]);
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
        assert_eq!(got, vec![
            ("ClassA".to_string(), "p1".to_string()),
            ("ClassB".to_string(), "p2".to_string()),
        ]);
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
}
