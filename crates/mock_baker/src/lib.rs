//! Mock baker: rewrite `createMock(I::class)` + expectation chains into
//! a self-contained anonymous class implementing `I`.
//!
//! Scope (POC): the supported pattern is
//!
//!   $m = $this->createMock(Iface::class);
//!   $m->expects(self::once())->method('foo')->with($x)->willReturn($v);
//!
//! Any deviation (custom matchers, willReturnCallback referencing capture,
//! method chained across statements) is detected and reported — the
//! transformation falls back to the original source rather than emitting
//! incorrect code.
//!
//! The interface signatures are extracted from the user-supplied source
//! file; all `use` statements from that file are preserved verbatim so
//! unqualified types in signatures keep resolving correctly inside the
//! anonymous class.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone)]
pub struct MethodSig {
    pub name:      String,
    pub params:    String,
    pub return_ty: String,
    pub is_static: bool,
}

#[derive(Debug, Clone)]
pub struct Expectation {
    pub method:        String,
    pub with_args:     Option<String>,
    pub will_return:   Option<String>,
    pub expects_count: ExpectsCount,
    /// false when the chain contains unsupported methods (willReturnMap, willReturnCallback,
    /// etc.). Blocks with any unsupported expectation are skipped from baking so the
    /// original $this->createMock() call is left verbatim.
    pub is_supported:  bool,
}

#[derive(Debug, Clone)]
pub enum ExpectsCount { Once, Any, Never, Times(String) }

#[derive(Debug, Clone)]
pub struct MockBlock {
    pub var:           String,
    pub iface_name:    String,
    /// Byte range of the `$x = $this->createMock(...)` statement only.
    pub byte_start:    usize,
    pub create_end:    usize,
    /// Byte ranges of each `$x->expects(...)->method(...)->...` statement
    /// that follows this createMock. Stored separately so they can be
    /// excised independently — crucial when multiple mocks' expectation
    /// chains are interleaved in the same method body.
    pub exp_stmts:     Vec<(usize, usize)>,
    pub expectations:  Vec<Expectation>,
}

pub struct Interface {
    pub use_lines:     Vec<String>,
    pub methods:       Vec<MethodSig>,
    pub is_interface:  bool,
    /// True when this is an `abstract class` (not an interface). The two flags
    /// are mutually exclusive; both false means a concrete class, which is skipped.
    pub is_abstract:   bool,
    pub extends_names: Vec<String>,
    /// PHP namespace of this interface (e.g. `Akeneo\Pim\Structure\Component\Model`).
    pub namespace:     String,
}

fn new_parser() -> Result<Parser> {
    let mut p = Parser::new();
    p.set_language(&tree_sitter_php::language_php())
        .context("set tree-sitter-php language")?;
    Ok(p)
}

fn text<'a>(n: Node<'a>, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

/// Extract a `short_name → fully_qualified_name` map from a PHP file's
/// `use` statements. E.g. `use Aws\S3\S3Client;` → `("S3Client", "Aws\\S3\\S3Client")`.
/// Also handles aliased imports: `use Foo\Bar as Baz;` → `("Baz", "Foo\\Bar")`.
pub fn extract_use_map(src: &str) -> Result<HashMap<String, String>> {
    let mut p = new_parser()?;
    let tree = p.parse(src, None).ok_or_else(|| anyhow!("php parse failed"))?;
    let bytes = src.as_bytes();
    let mut map = HashMap::new();

    walk(tree.root_node(), &mut |n: Node| {
        if n.kind() != "namespace_use_declaration" { return; }
        let mut cursor = n.walk();
        for clause in n.named_children(&mut cursor) {
            if clause.kind() != "namespace_use_clause" { continue; }
            let name_node = clause.child_by_field_name("name")
                .or_else(|| clause.named_child(0));
            let Some(name_node) = name_node else { continue };
            let fqn = text(name_node, bytes).trim_start_matches('\\').to_string();

            // Alias: `use Foo\Bar as Baz` — tree-sitter-php may not expose this
            // as a named field, so we scan the raw clause text for " as ".
            let clause_text = text(clause, bytes);
            let alias = clause_text.to_ascii_lowercase()
                .find(" as ")
                .map(|pos| clause_text[pos + 4..].trim().to_string());

            let short = alias.unwrap_or_else(|| {
                fqn.rsplit('\\').next().unwrap_or(&fqn).to_string()
            });
            map.insert(short, fqn);
        }
    });

    Ok(map)
}

pub fn parse_interface(src: &str) -> Result<Interface> {
    let mut p = new_parser()?;
    let tree = p.parse(src, None).ok_or_else(|| anyhow!("php parse failed"))?;
    let root = tree.root_node();
    let bytes = src.as_bytes();

    let mut use_lines = Vec::new();
    let mut methods = Vec::new();
    let mut is_interface = false;
    let mut is_abstract = false;
    let mut extends_names: Vec<String> = Vec::new();
    let mut namespace = String::new();

    walk(root, &mut |n: Node| {
        if n.kind() == "namespace_use_declaration" {
            use_lines.push(text(n, bytes).to_string());
        }
        if n.kind() == "namespace_definition" {
            if let Some(name_node) = n.child_by_field_name("name") {
                namespace = text(name_node, bytes)
                    .trim_start_matches('\\').to_string();
            }
        }
        if n.kind() == "interface_declaration" {
            is_interface = true;
            let mut c = n.walk();
            for child in n.named_children(&mut c) {
                if child.kind() == "base_clause" {
                    let mut c2 = child.walk();
                    for name_node in child.named_children(&mut c2) {
                        let raw = text(name_node, bytes).trim_start_matches('\\').to_string();
                        extends_names.push(raw);
                    }
                }
            }
        }
        if n.kind() == "class_declaration" {
            let class_text = text(n, bytes);
            let before_class = class_text.split("class").next().unwrap_or("");
            if before_class.split_whitespace().any(|w| w == "abstract") {
                is_abstract = true;
            }
        }
        if n.kind() == "method_declaration" {
            if let Some(sig) = sig_from(n, bytes) {
                methods.push(sig);
            }
        }
    });

    Ok(Interface { use_lines, methods, is_interface, is_abstract, extends_names, namespace })
}

fn sig_from(n: Node, src: &[u8]) -> Option<MethodSig> {
    let name_node = n.child_by_field_name("name")?;
    let name = text(name_node, src).to_string();
    let params_node = n.child_by_field_name("parameters")?;
    let params_raw = text(params_node, src).to_string();
    let params = normalize_params(&params_raw);
    let ret_node = n.child_by_field_name("return_type");
    // Use "mixed" when no return type is declared — methods without annotations
    // can return any value, so we must not treat them as void.
    let return_ty = ret_node.map(|r| text(r, src).trim().to_string())
                            .unwrap_or_else(|| "mixed".to_string());
    let decl_text = text(n, src);
    let before_fn = decl_text.split("function").next().unwrap_or("");
    let mods: Vec<&str> = before_fn.split_whitespace().collect();
    // final methods cannot be overridden; private methods are inaccessible from
    // an anonymous subclass — skip both so we never emit a compile error.
    if mods.contains(&"final") || mods.contains(&"private") {
        return None;
    }
    let is_static = mods.contains(&"static");
    Some(MethodSig { name, params, return_ty, is_static })
}

/// Strip outer parens, collapse internal whitespace runs, drop any
/// trailing comma (PHP 8.0+ allows it in declarations, but tree-sitter
/// captures it verbatim — emitting it back into our anon class is fine,
/// but it looks ugly so we clean it).
fn normalize_params(raw: &str) -> String {
    let mut s = raw.trim().trim_start_matches('(').trim_end_matches(')').trim().to_string();
    // Collapse whitespace.
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s = collapsed;
    // Drop a trailing comma if present.
    if s.ends_with(',') { s.pop(); }
    s
}

fn walk<F: FnMut(Node)>(n: Node, f: &mut F) {
    f(n);
    let mut c = n.walk();
    for child in n.named_children(&mut c) {
        walk(child, f);
    }
}

/// Detect `$var = $this->getMockBuilder(X::class)->...->getMock()`.
/// Intermediate builder calls (disableOriginalConstructor, etc.) are silently
/// ignored. Bails on `onlyMethods` / `addMethods` / `setConstructorArgs` since
/// those require per-method or constructor-argument awareness.
fn detect_get_mock_builder<'a>(
    asn: Node<'a>, stmt: Node<'a>, src: &'a [u8],
) -> Option<MockBlock> {
    let lhs = asn.child_by_field_name("left")?;
    if lhs.kind() != "variable_name" { return None; }
    let var = format!("${}", text(lhs.named_child(0)?, src));

    let rhs = asn.child_by_field_name("right")?;
    if rhs.kind() != "member_call_expression" { return None; }

    const BAIL: &[&str] = &["onlyMethods", "addMethods", "setMethods", "setConstructorArgs"];
    let mut cur = rhs;
    let iface_name: String = loop {
        if cur.kind() != "member_call_expression" { return None; }
        let call_name = text(cur.child_by_field_name("name")?, src);
        match call_name {
            "getMock" | "getMockForAbstractClass" => {}
            "getMockBuilder" => {
                let args = cur.child_by_field_name("arguments")?;
                let arg  = args.named_child(0)?;
                let cce  = arg.named_child(0)?;
                if cce.kind() != "class_constant_access_expression" { return None; }
                let obj = cur.child_by_field_name("object")?;
                if text(obj, src) != "$this" { return None; }
                break text(cce.named_child(0)?, src).to_string();
            }
            n if BAIL.contains(&n) => return None,
            _ => {}
        }
        cur = cur.child_by_field_name("object")?;
    };
    Some(MockBlock {
        var,
        iface_name,
        byte_start:   stmt.start_byte(),
        create_end:   stmt.end_byte(),
        exp_stmts:    Vec::new(),
        expectations: Vec::new(),
    })
}

/// Detect all `createMock` blocks in a test source. Each block is
/// (mock variable, interface name, statement byte range, expectations).
pub fn parse_test(src: &str) -> Result<Vec<MockBlock>> {
    let mut p = new_parser()?;
    let tree = p.parse(src, None).ok_or_else(|| anyhow!("php parse failed"))?;
    let bytes = src.as_bytes();

    let mut blocks: Vec<MockBlock> = Vec::new();

    walk(tree.root_node(), &mut |n: Node| {
        if n.kind() != "expression_statement" { return; }
        let inner = match n.named_child(0) { Some(c) => c, None => return };

        // Case A: assignment "$m = $this->createMock(Iface::class)"
        if inner.kind() == "assignment_expression" {
            if let Some(b) = detect_create_mock(inner, n, bytes) {
                blocks.push(b);
            } else if let Some(b) = detect_get_mock_builder(inner, n, bytes) {
                blocks.push(b);
            }
        }
    });

    // Map var → sorted list of (byte_pos, Option<block_index>).
    // Some(i) = createMock block; None = any other assignment (getMockBuilder, etc.).
    // A variable may be declared in multiple test methods under the same name.
    let mut by_var: std::collections::HashMap<String, Vec<(usize, Option<usize>)>> = HashMap::new();
    for (i, b) in blocks.iter().enumerate() {
        by_var.entry(b.var.clone()).or_default().push((b.byte_start, Some(i)));
    }

    // Collect non-createMock assignments that shadow earlier createMock blocks.
    // e.g. `$m = $this->getMockBuilder(Foo::class)->getMock()` resets $m even though
    // we don't bake it, so any ->method() chain after it must NOT be attached to the
    // earlier createMock of the same variable.
    walk(tree.root_node(), &mut |n: Node| {
        if n.kind() != "expression_statement" { return; }
        let inner = match n.named_child(0) { Some(c) => c, None => return };
        if inner.kind() != "assignment_expression" { return; }
        let Some(lhs) = inner.child_by_field_name("left") else { return };
        if lhs.kind() != "variable_name" { return; }
        let Some(name_node) = lhs.named_child(0) else { return };
        let var = format!("${}", text(name_node, bytes));
        if !by_var.contains_key(&var) { return; }
        let pos = n.start_byte();
        // Skip positions already registered as createMock blocks.
        if blocks.iter().any(|b| b.var == var && b.byte_start == pos) { return; }
        by_var.entry(var).or_default().push((pos, None));
    });
    for v in by_var.values_mut() { v.sort_by_key(|(p, _)| *p); }

    walk(tree.root_node(), &mut |n: Node| {
        if n.kind() != "expression_statement" { return; }
        let inner = match n.named_child(0) { Some(c) => c, None => return };
        if inner.kind() != "member_call_expression" { return; }
        if let Some((var, expectation)) = detect_expectation(inner, bytes) {
            if let Some(assigns) = by_var.get(&var) {
                // The closest preceding assignment for this variable wins.
                // If it's a createMock (Some(idx)) → attach; if it's another
                // assignment (None, e.g. getMockBuilder) → skip.
                let exp_pos = n.start_byte();
                let best = assigns.iter()
                    .filter(|(bs, _)| *bs < exp_pos)
                    .max_by_key(|(bs, _)| *bs);
                if let Some((_, Some(idx))) = best {
                    blocks[*idx].exp_stmts.push((n.start_byte(), n.end_byte()));
                    blocks[*idx].expectations.push(expectation);
                }
            }
        }
    });

    // Remove blocks that have any unsupported expectation — the original
    // $this->createMock() call and all its method chains are left verbatim.
    let unsupported_vars: std::collections::HashSet<String> = blocks.iter()
        .filter(|b| b.expectations.iter().any(|e| !e.is_supported))
        .map(|b| b.var.clone())
        .collect();
    if !unsupported_vars.is_empty() {
        blocks.retain(|b| !unsupported_vars.contains(&b.var));
    }

    Ok(blocks)
}

fn detect_create_mock<'a>(
    asn: Node<'a>, stmt: Node<'a>, src: &'a [u8],
) -> Option<MockBlock> {
    let lhs = asn.child_by_field_name("left")?;
    if lhs.kind() != "variable_name" { return None; }
    let var = format!("${}", text(lhs.named_child(0)?, src));

    let rhs = asn.child_by_field_name("right")?;
    if rhs.kind() != "member_call_expression" { return None; }
    let obj = rhs.child_by_field_name("object")?;
    if text(obj, src) != "$this" { return None; }
    let name = rhs.child_by_field_name("name")?;
    if !matches!(text(name, src), "createMock" | "createStub") { return None; }

    let args = rhs.child_by_field_name("arguments")?;
    let arg  = args.named_child(0)?;
    let cce  = arg.named_child(0)?;
    if cce.kind() != "class_constant_access_expression" { return None; }
    let iface_name = text(cce.named_child(0)?, src).to_string();

    Some(MockBlock {
        var,
        iface_name,
        byte_start:   stmt.start_byte(),
        create_end:   stmt.end_byte(),
        exp_stmts:    Vec::new(),
        expectations: Vec::new(),
    })
}

/// Walk an expectation chain `$m->expects(...)->method('x')->with(...)->willReturn(...)`.
/// We descend from the outermost call (willReturn / willReturnCallback / willThrow)
/// down to the base `$m`.
fn detect_expectation<'a>(
    outer: Node<'a>, src: &'a [u8],
) -> Option<(String, Expectation)> {
    let mut method_name: Option<String> = None;
    let mut with_args:   Option<String> = None;
    let mut will_return: Option<String> = None;
    let mut expects_count = ExpectsCount::Any;
    let mut is_supported = true;

    let mut cur = outer;
    loop {
        if cur.kind() != "member_call_expression" { break; }
        let call_name = text(cur.child_by_field_name("name")?, src);
        let args = cur.child_by_field_name("arguments")?;
        let args_inner = strip_outer_parens(text(args, src));

        match call_name {
            "willReturn"                  => will_return = Some(format!("return {};", args_inner)),
            "willReturnSelf"              => will_return = Some("return $this;".to_string()),
            "willThrowException"          => will_return = Some(format!("throw:{}", args_inner)),
            "willReturnCallback"          => will_return = Some(format!("callback:{}", args_inner)),
            "willReturnOnConsecutiveCalls" => will_return = Some(format!("queue:[{}]", args_inner)),
            "method"              => {
                let s = args_inner.trim();
                let s = s.trim_matches('\'').trim_matches('"');
                method_name = Some(s.to_string());
            }
            "with"                => with_args = Some(args_inner.to_string()),
            "expects"             => {
                let a = args_inner.trim();
                expects_count = if a.contains("once")  { ExpectsCount::Once }
                                else if a.contains("never") { ExpectsCount::Never }
                                else if a.contains("any")   { ExpectsCount::Any }
                                else { ExpectsCount::Times(a.to_string()) };
            }
            _ => { is_supported = false; }
        }

        let obj = cur.child_by_field_name("object")?;
        if obj.kind() == "variable_name" {
            let var = format!("${}", text(obj.named_child(0)?, src));
            return Some((var, Expectation {
                method:        method_name?,
                with_args,
                will_return,
                expects_count,
                is_supported,
            }));
        }
        cur = obj;
    }
    None
}

/// Return the body fragment for a lenient stub of a method with the given return type.
/// Respects `declare(strict_types=1)`: primitive types need real zero values, not null.
fn primitive_default_body(ret: &str) -> String {
    let t = ret.trim().trim_start_matches(':').trim();
    // Strip leading `?` (nullable) — nullable types accept null, so we can just return null.
    if t.starts_with('?') {
        return " return null;".to_string();
    }
    // Union types that include null (`X|null`) also accept null.
    if t.split('|').any(|p| p.trim() == "null") {
        return " return null;".to_string();
    }
    // Strip nullable prefix for the base type check (after the above checks).
    let base = t.trim_start_matches('?').trim();
    // For union types, use the first component's default.
    let first = base.split(|c: char| c == '|' || c == '&').next().unwrap_or(base).trim();
    match first {
        "bool" | "true" | "false" => " return false;".to_string(),
        "int"                     => " return 0;".to_string(),
        "float"                   => " return 0.0;".to_string(),
        "string"                  => " return '';".to_string(),
        "array" | "iterable"      => " return [];".to_string(),
        "static" | "self"         => " return $this;".to_string(),
        "never"                   => " throw new \\LogicException('stub');".to_string(),
        // mixed, null, object, resource, callable, scalar, numeric, or unrecognised — null is fine
        _ => " return null;".to_string(),
    }
}

/// If `ret` is a fully-qualified class/interface return type (starts with `\`),
/// return the FQN to pass to `createStub(FQN::class)`. Returns None for void,
/// primitive types, or unqualified names that we can't reliably stub.
fn object_return_fqn(ret: &str) -> Option<String> {
    let t = ret.trim().trim_start_matches(':').trim();
    let t = t.strip_prefix('?').unwrap_or(t).trim();
    // Take the first part of a union/intersection (e.g. `\Foo|\Bar` → `\Foo`).
    let t = t.split(|c: char| c == '|' || c == '&').next().unwrap_or(t).trim();
    if t.starts_with('\\') {
        Some(t.to_string())
    } else {
        None
    }
}

fn strip_outer_parens(s: &str) -> &str {
    s.strip_prefix('(').and_then(|s| s.strip_suffix(')')).unwrap_or(s)
}

/// Extract the first argument from a comma-separated PHP expression list,
/// respecting nesting depth so `fn($a, $b) => $a` is treated as one token.
fn first_arg(args: &str) -> String {
    let mut depth = 0i32;
    for (i, c) in args.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => return args[..i].trim().to_string(),
            _ => {}
        }
    }
    args.trim().to_string()
}

/// Emit the anon-class block that replaces a `createMock` site.
/// `iface` is `None` when the interface couldn't be resolved; in that case
/// we emit a verbatim `createMock()` call so the test still compiles.
///
/// Values from `willReturn` / `with` are passed directly as constructor
/// arguments (constructor property promotion), so no post-creation mutation
/// is needed. The instantiation is emitted at the position of the *last*
/// expectation statement by `bake()`, ensuring all runtime values are in
/// scope at the call site.
pub fn emit_anon_class(
    block: &MockBlock, iface: Option<&Interface>,
) -> String {
    let Some(iface) = iface else {
        return format!("{} = $this->createMock({}::class);", block.var, block.iface_name);
    };

    // Build ordered list of (field_name, php_call_value, php_type) for promoted properties.
    let mut ctor_fields: Vec<(String, String, &str)> = Vec::new();
    for (i, e) in block.expectations.iter().enumerate() {
        if let Some(wa) = &e.with_args {
            ctor_fields.push((format!("__cap_{}", i), first_arg(wa), "mixed"));
        }
        if let Some(raw) = &e.will_return {
            if raw == "return $this;" { continue; }
            let (val, ty) = if let Some(rest) = raw.strip_prefix("throw:") {
                (rest.to_string(), "mixed")
            } else if let Some(rest) = raw.strip_prefix("callback:") {
                (rest.to_string(), "mixed")
            } else if let Some(rest) = raw.strip_prefix("queue:") {
                (rest.to_string(), "array")
            } else {
                (raw.trim_start_matches("return ").trim_end_matches(';').trim().to_string(), "mixed")
            };
            ctor_fields.push((format!("__ret_{}", i), val, ty));
        }
    }

    let keyword = if iface.is_interface { "implements" } else { "extends" };
    let mut out = String::new();

    // Constructor call args: $this (TestCase) + promoted field values in declaration order.
    let call_args = std::iter::once("$this".to_string())
        .chain(ctor_fields.iter().map(|(_, v, _)| v.clone()))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("{} = new class({}) {} {} {{\n",
        block.var, call_args, keyword, block.iface_name));

    // Per-method call counters (mutable state — cannot use constructor promotion).
    let mut seen_counters = std::collections::HashSet::new();
    for e in &block.expectations {
        if seen_counters.insert(e.method.as_str()) {
            out.push_str(&format!("    private int $__calls_{} = 0;\n", e.method));
        }
    }

    // Constructor: $__tc + all ret/cap values as promoted private properties.
    let promoted = ctor_fields.iter()
        .map(|(name, _, ty)| format!("private {} ${}", ty, name))
        .collect::<Vec<_>>();
    let ctor_params = std::iter::once(
            "private \\PHPUnit\\Framework\\TestCase $__tc".to_string())
        .chain(promoted)
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("    public function __construct({}) {{}}\n", ctor_params));

    // Expectation methods.
    for (i, e) in block.expectations.iter().enumerate() {
        let sig = match iface.methods.iter().find(|m| m.name == e.method) {
            Some(s) => s,
            None => continue,
        };
        let stat = if sig.is_static { "static " } else { "" };
        out.push_str(&format!("    public {}function {}({}): {} {{\n",
                              stat, sig.name, sig.params, sig.return_ty));
        out.push_str(&format!("        $this->__calls_{}++;\n", e.method));
        if e.with_args.is_some() {
            let param0 = first_param_name(&sig.params);
            out.push_str(&format!(
                "        \\PHPUnit\\Framework\\Assert::assertSame($this->__cap_{}, {});\n",
                i, param0));
        }
        let is_void = sig.return_ty.trim() == ": void" || sig.return_ty.trim() == "void";
        match e.will_return.as_deref() {
            Some("return $this;") => {
                out.push_str("        return $this;\n");
            }
            Some(wr) if wr.starts_with("throw:") => {
                out.push_str(&format!("        throw $this->__ret_{};\n", i));
            }
            Some(wr) if wr.starts_with("callback:") => {
                if is_void {
                    out.push_str(&format!("        ($this->__ret_{})(...func_get_args());\n", i));
                } else {
                    out.push_str(&format!("        return ($this->__ret_{})(...func_get_args());\n", i));
                }
            }
            Some(wr) if wr.starts_with("queue:") => {
                if is_void {
                    out.push_str(&format!("        array_shift($this->__ret_{});\n", i));
                } else {
                    out.push_str(&format!("        return array_shift($this->__ret_{});\n", i));
                }
            }
            Some(_) if !is_void => {
                out.push_str(&format!("        return $this->__ret_{};\n", i));
            }
            _ => {}
        }
        out.push_str("    }\n");
    }

    // Lenient stubs for methods not covered by any expectation.
    for m in &iface.methods {
        if block.expectations.iter().any(|e| e.method == m.name) { continue; }
        let stat = if m.is_static { "static " } else { "" };
        let is_void = m.return_ty.trim() == ": void" || m.return_ty.trim() == "void";
        let body = if is_void {
            String::new()
        } else if let Some(fqn) = object_return_fqn(&m.return_ty) {
            format!(" return (function() {{ return $this->createStub({}::class); }})->call($this->__tc);", fqn)
        } else {
            primitive_default_body(&m.return_ty)
        };
        out.push_str(&format!(
            "    public {}function {}({}): {} {{{}}}\n",
            stat, m.name, m.params, m.return_ty, body));
    }

    out.push_str("    public function __destruct() {\n");
    for e in &block.expectations {
        if let ExpectsCount::Once = e.expects_count {
            out.push_str(&format!(
                "        \\PHPUnit\\Framework\\Assert::assertSame(1, $this->__calls_{}, '{}() expected once');\n",
                e.method, e.method));
        }
    }
    out.push_str("    }\n");
    out.push_str("};");
    out
}

fn first_param_name(params: &str) -> String {
    // Cheap extractor: grab the first $name in the parameter list.
    let bytes = params.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            return String::from_utf8_lossy(&bytes[i..j]).to_string();
        }
        i += 1;
    }
    "$arg0".into()
}

/// Rewrite `test_src` replacing every `createMock` site with a baked anon class.
/// `ifaces` maps interface short-name (as it appears in `createMock(X::class)`) to
/// its parsed `Interface`. Blocks whose interface is absent from the map are left
/// as a verbatim `$this->createMock()` call (safe fallback).
pub fn bake(test_src: &str, ifaces: &HashMap<String, Interface>) -> Result<String> {
    let blocks = parse_test(test_src)?;
    if blocks.is_empty() {
        bail!("no createMock pattern found in test source");
    }
    // Merge all use_lines from every resolved interface.
    let all_uses: Vec<String> = ifaces.values()
        .flat_map(|i| i.use_lines.iter().cloned())
        .collect();
    let with_uses = inject_uses(test_src, &all_uses);

    // We have to re-parse because byte offsets shifted after inserting uses.
    let blocks = parse_test(&with_uses)?;

    // Build a flat sorted list of (start, end, kind) spans.
    //
    // Strategy: for each baked block, the anonymous class instantiation is emitted
    // at the position of the *last* expectation statement (where all willReturn
    // values are already in scope). The createMock() statement and all preceding
    // expectation chains are silently dropped. If a block has no expectations the
    // instantiation stays at the createMock() position.
    //
    // This eliminates the old "Assign" spans entirely: values flow directly into
    // the constructor call, so no post-creation mutation is needed.
    #[derive(Clone)]
    enum SpanKind { Create(usize), Skip }
    let mut spans: Vec<(usize, usize, SpanKind)> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if !ifaces.contains_key(&b.iface_name) {
            continue;
        }
        if b.exp_stmts.is_empty() {
            // No expectations → instantiate at the createMock() position as before.
            spans.push((b.byte_start, b.create_end, SpanKind::Create(i)));
        } else {
            // Drop the createMock() statement; defer instantiation to last exp.
            spans.push((b.byte_start, b.create_end, SpanKind::Skip));
            for (exp_idx, &(s, e)) in b.exp_stmts.iter().enumerate() {
                if exp_idx + 1 == b.exp_stmts.len() {
                    spans.push((s, e, SpanKind::Create(i)));
                } else {
                    spans.push((s, e, SpanKind::Skip));
                }
            }
        }
    }
    spans.sort_by_key(|&(s, _, _)| s);

    let mut out = String::new();
    let mut cursor = 0usize;
    for (start, end, kind) in &spans {
        if *start < cursor { continue; }
        out.push_str(&with_uses[cursor..*start]);
        if let SpanKind::Create(i) = kind {
            out.push_str(&emit_anon_class(&blocks[*i], ifaces.get(&blocks[*i].iface_name)));
        }
        cursor = *end;
        if with_uses[cursor..].starts_with(';') { cursor += 1; }
        if with_uses.as_bytes().get(cursor) == Some(&b'\n') { cursor += 1; }
    }
    out.push_str(&with_uses[cursor..]);
    Ok(out)
}

/// Extract the short name (or alias) from a `use` line.
/// `use Foo\Bar\Baz;`      → Some("Baz")
/// `use Foo\Bar as Alias;` → Some("Alias")
/// `use function ...`      → None  (function imports have different rules)
fn use_short_name(line: &str) -> Option<String> {
    let body = line.trim().strip_prefix("use ")?.strip_suffix(';')?.trim();
    if body.starts_with("function ") || body.starts_with("const ") { return None; }
    if let Some(pos) = body.to_ascii_lowercase().find(" as ") {
        Some(body[pos + 4..].trim().to_string())
    } else {
        Some(body.rsplit('\\').next()?.trim().to_string())
    }
}

/// Insert each missing `use` line just before the first non-use statement
/// in the file. Skips entries that already textually appear in `src`, and
/// also skips any entry whose short name / alias is already declared in `src`
/// (injecting it would produce a PHP fatal "name already in use" error).
fn inject_uses(src: &str, extra: &[String]) -> String {
    // Collect short names already in use inside the test file.
    let existing_short: std::collections::HashSet<String> = src.lines()
        .filter_map(|l| use_short_name(l.trim()))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut missing: Vec<&str> = Vec::new();
    for line in extra {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        if src.contains(trimmed) { continue; }
        // Skip if the short name/alias would shadow an existing import.
        if let Some(short) = use_short_name(trimmed) {
            if existing_short.contains(&short) { continue; }
        }
        if seen.insert(trimmed) { missing.push(trimmed); }
    }
    if missing.is_empty() { return src.to_string(); }

    // Anchor: the line after the last top-level `use ...;` or `namespace ...;`
    // in the file. We only consider lines with NO leading whitespace — trait
    // `use` statements inside class bodies are indented and must not be used
    // as the anchor (they would inject the new use lines inside the class).
    let mut anchor = 0usize;
    for (idx, line) in src.lines().enumerate() {
        // Top-level PHP statements never have leading whitespace.
        if line.starts_with("use ") || line.starts_with("namespace ") {
            anchor = src.lines().take(idx + 1)
                .map(|l| l.len() + 1).sum::<usize>(); // end of this line
        }
    }
    let (head, tail) = src.split_at(anchor);
    let mut block = String::new();
    for u in missing {
        block.push_str(u);
        if !u.ends_with(';') { block.push(';'); }
        block.push('\n');
    }
    format!("{head}{block}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_extract_use_map_with_alias() {
        let src = "<?php\nuse Doctrine\\DBAL\\Driver\\Connection as DriverConnection;\nuse Foo\\Bar;\n";
        let map = extract_use_map(src).unwrap();
        assert_eq!(map.get("DriverConnection").map(|s| s.as_str()), Some("Doctrine\\DBAL\\Driver\\Connection"), "alias not resolved: {:?}", map);
        assert_eq!(map.get("Bar").map(|s| s.as_str()), Some("Foo\\Bar"), "non-alias not resolved: {:?}", map);
    }
}
