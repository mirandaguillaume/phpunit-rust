//! Native mutation generation over the mago AST (V1 mutator set).
//!
//! `mutators` maps individual AST operator/literal nodes to their byte-range
//! replacement; higher tasks walk a file's AST and join the result with per-test
//! coverage. The byte offsets come straight from mago spans, so a mutation is a
//! splice of `source[start..end]` — no pretty-printer, no re-emission.
use std::path::{Path, PathBuf};

pub mod coverage;
pub mod mutators;
pub mod plan;

use bumpalo::Bump;
use mago_database::file::FileId;
use mago_span::HasSpan;
use mago_syntax::ast::argument::{Argument, ArgumentList};
use mago_syntax::ast::binary::BinaryOperator;
use mago_syntax::ast::call::{Call, FunctionCall};
use mago_syntax::ast::expression::Expression;
use mago_syntax::ast::function_like::r#return::FunctionLikeReturnTypeHint;
use mago_syntax::ast::identifier::Identifier;
use mago_syntax::ast::literal::Literal;
use mago_syntax::ast::node::Node;
use mago_syntax::ast::r#yield::Yield;
use mago_syntax::ast::type_hint::Hint;
use mago_syntax::ast::unary::{UnaryPostfixOperator, UnaryPrefixOperator};
use mago_syntax::ast::variable::Variable;
use mago_syntax::ast::Access;
use mago_syntax::ast::ArrayElement;
use mago_syntax::ast::MatchArm;
use mago_syntax::ast::Method;
use mago_syntax::ast::MethodBody;
use mago_syntax::ast::Modifier;
use mago_syntax::parser::parse_file_content;

/// The lower-cased, namespace-stripped name of an identifier-callee (e.g. `\StrToLower`
/// → `strtolower`); `None` for dynamic callees (`$f(...)`, closures, method calls).
fn callee_name_lower(func: &Expression) -> Option<Vec<u8>> {
    let Expression::Identifier(id) = func else {
        return None;
    };
    let raw: &[u8] = match id {
        Identifier::Local(l) => l.value,
        Identifier::Qualified(q) => q.value,
        Identifier::FullyQualified(f) => f.value,
    };
    let last = raw.rsplit(|&b| b == b'\\').next().unwrap_or(raw);
    Some(last.to_ascii_lowercase())
}

/// Byte span of the `n`-th positional (non-spread) argument's value, if any.
fn nth_arg_span(args: &ArgumentList, n: usize) -> Option<(usize, usize)> {
    let Argument::Positional(p) = args.arguments.iter().nth(n)? else {
        return None;
    };
    if p.ellipsis.is_some() {
        return None;
    }
    let s = p.value.span();
    Some((s.start.offset as usize, s.end.offset as usize))
}

/// Infection `Unwrap*`: `f(…, a, …)` → `a`. Replace the whole call with the kept arg
/// (arg 0 for most; a few keep a later arg, e.g. `str_replace`/`array_reduce` → arg 2).
fn record_unwrap(out: &mut Vec<Mutant>, file: &Path, source: &[u8], fc: &FunctionCall) {
    let Some(name) = callee_name_lower(fc.function) else {
        return;
    };
    let call = fc.span();
    let (cstart, cend) = (call.start.offset as usize, call.end.offset as usize);
    // Single-index unwrap: keep one fixed arg (arg 0 for most; `str_replace` → arg 2…).
    if let Some((mutator, index)) = mutators::unwrap_arg(&name) {
        let Some((astart, aend)) = nth_arg_span(&fc.argument_list, index) else {
            return;
        };
        if astart >= aend || aend > source.len() {
            return;
        }
        record_owned(
            out,
            file,
            source,
            cstart,
            cend,
            source[astart..aend].to_vec(),
            mutator,
        );
        return;
    }
    // Range unwrap: emit one mutant per kept arg in `start..len-drop_last` (e.g. `array_map`
    // drops the leading callback; `array_uintersect` drops the trailing comparator).
    if let Some((mutator, start_idx, drop_last)) = mutators::unwrap_range(&name) {
        let end = fc.argument_list.arguments.len().saturating_sub(drop_last);
        for i in start_idx..end {
            let Some((astart, aend)) = nth_arg_span(&fc.argument_list, i) else {
                continue;
            };
            if astart >= aend || aend > source.len() {
                continue;
            }
            record_owned(
                out,
                file,
                source,
                cstart,
                cend,
                source[astart..aend].to_vec(),
                mutator,
            );
        }
    }
}

/// Whole-call rewrites keyed by callee name: `Nullify` (`array_find(…)` → `null`) and
/// `MBString` (`mb_strlen($s, …)` → `strlen($s)` — vanilla name, first N args kept).
fn record_call_rewrites(out: &mut Vec<Mutant>, file: &Path, source: &[u8], fc: &FunctionCall) {
    let Some(name) = callee_name_lower(fc.function) else {
        return;
    };
    let call = fc.span();
    let (cstart, cend) = (call.start.offset as usize, call.end.offset as usize);
    if cstart >= cend || cend > source.len() {
        return;
    }
    // Nullify: the whole call becomes `null`.
    if let Some(mutator) = mutators::nullify_name(&name) {
        record_owned(out, file, source, cstart, cend, b"null".to_vec(), mutator);
        return;
    }
    // Boolean predicate: `array_all(…)`/`array_any(…)` becomes `true`.
    if let Some(mutator) = mutators::array_predicate_true(&name) {
        record_owned(out, file, source, cstart, cend, b"true".to_vec(), mutator);
        return;
    }
    if name == b"preg_match" {
        // PregMatchMatches: with the `$matches` out-param -> `(int)($m = [])`.
        if fc.argument_list.arguments.len() >= 3 {
            if let Some((m0, m1)) = nth_arg_span(&fc.argument_list, 2) {
                if m1 <= source.len() {
                    let mut r = b"(int)(".to_vec();
                    r.extend_from_slice(&source[m0..m1]);
                    r.extend_from_slice(b" = [])");
                    record_owned(out, file, source, cstart, cend, r, "PregMatchMatches");
                }
            }
        }
        // Regex-content mutators: drop `^` / `$` / a flag from a string-literal pattern.
        if let Some(Argument::Positional(p)) = fc.argument_list.arguments.iter().next() {
            if let Expression::Literal(Literal::String(ls)) = p.value {
                if let Some(content) = ls.value {
                    let a0 = p.value.span();
                    let (a0s, a0e) = (a0.start.offset as usize, a0.end.offset as usize);
                    if a0e <= source.len() {
                        for (mutator, mutated) in mutators::regex_variants(content) {
                            let mut lit = vec![b'\''];
                            for &b in &mutated {
                                if b == b'\\' || b == b'\'' {
                                    lit.push(b'\\');
                                }
                                lit.push(b);
                            }
                            lit.push(b'\'');
                            record_owned(out, file, source, a0s, a0e, lit, mutator);
                        }
                    }
                }
            }
        }
        return;
    }
    // RoundingFamily: `round($x)` -> `floor($x)` AND `ceil($x)` (2 mutants, arg 0 only).
    if let Some(targets) = mutators::rounding_family(&name) {
        if let Some((a0, a1)) = nth_arg_span(&fc.argument_list, 0) {
            if a1 <= source.len() {
                for target in targets {
                    let mut r = target.to_vec();
                    r.push(b'(');
                    r.extend_from_slice(&source[a0..a1]);
                    r.push(b')');
                    record_owned(out, file, source, cstart, cend, r, "RoundingFamily");
                }
            }
        }
        return;
    }
    // BCMath: `bcadd($a, $b, …)` -> `(string)($a + $b)`; `bcsqrt($a, …)` -> `(string)sqrt($a)`.
    if let Some(kind) = mutators::bcmath_op(&name) {
        let seg = |i: usize| nth_arg_span(&fc.argument_list, i);
        let repl: Option<Vec<u8>> = match kind {
            mutators::BcMath::Binary(op) => match (seg(0), seg(1)) {
                (Some((a0, a1)), Some((b0, b1))) if a1 <= source.len() && b1 <= source.len() => {
                    let mut r = b"(string)(".to_vec();
                    r.extend_from_slice(&source[a0..a1]);
                    r.push(b' ');
                    r.extend_from_slice(op);
                    r.push(b' ');
                    r.extend_from_slice(&source[b0..b1]);
                    r.push(b')');
                    Some(r)
                }
                _ => None,
            },
            mutators::BcMath::Sqrt => {
                seg(0)
                    .filter(|&(_, a1)| a1 <= source.len())
                    .map(|(a0, a1)| {
                        let mut r = b"(string)sqrt(".to_vec();
                        r.extend_from_slice(&source[a0..a1]);
                        r.push(b')');
                        r
                    })
            }
            // bcpowmod($a, $b, $c) -> (string)(pow($a, $b) % $c).
            mutators::BcMath::PowMod => match (seg(0), seg(1), seg(2)) {
                (Some((a0, a1)), Some((b0, b1)), Some((c0, c1)))
                    if a1 <= source.len() && b1 <= source.len() && c1 <= source.len() =>
                {
                    let mut r = b"(string)(pow(".to_vec();
                    r.extend_from_slice(&source[a0..a1]);
                    r.extend_from_slice(b", ");
                    r.extend_from_slice(&source[b0..b1]);
                    r.extend_from_slice(b") % ");
                    r.extend_from_slice(&source[c0..c1]);
                    r.push(b')');
                    Some(r)
                }
                _ => None,
            },
        };
        if let Some(repl) = repl {
            record_owned(out, file, source, cstart, cend, repl, "BCMath");
        }
        return;
    }
    // MBString: rebuild the call as `<vanilla>(<first N positional args>)`.
    if let Some((vanilla, at_most)) = mutators::mbstring_vanilla(&name) {
        let mut repl = vanilla.to_vec();
        repl.push(b'(');
        let mut n = 0;
        for arg in fc.argument_list.arguments.iter() {
            if n >= at_most {
                break;
            }
            let Argument::Positional(p) = arg else {
                continue;
            };
            if p.ellipsis.is_some() {
                return; // spread — don't attempt a byte-exact rebuild
            }
            let s = p.value.span();
            let (vs, ve) = (s.start.offset as usize, s.end.offset as usize);
            if ve > source.len() {
                return;
            }
            if n > 0 {
                repl.extend_from_slice(b", ");
            }
            repl.extend_from_slice(&source[vs..ve]);
            n += 1;
        }
        repl.push(b')');
        record_owned(out, file, source, cstart, cend, repl, "MBString");
    }
}

/// Infection condition-negation (`IfNegation`/`ElseIfNegation`): wrap `cond` in
/// `!(...)` by re-emitting the original bytes around a `!(` / `)`.
fn negate_condition(
    out: &mut Vec<Mutant>,
    file: &Path,
    source: &[u8],
    cond: &Expression,
    mutator: &'static str,
) {
    wrap_not_span(out, file, source, cond.span(), mutator);
}

/// Wrap the bytes at `span` in `!(...)` — shared by `IfNegation`/`ElseIfNegation` and
/// `LogicalAndNegation`/`LogicalOrNegation`.
fn wrap_not_span(
    out: &mut Vec<Mutant>,
    file: &Path,
    source: &[u8],
    span: mago_span::Span,
    mutator: &'static str,
) {
    let (cs, ce) = (span.start.offset as usize, span.end.offset as usize);
    if cs >= ce || ce > source.len() {
        return;
    }
    let mut repl = Vec::with_capacity(ce - cs + 3);
    repl.extend_from_slice(b"!(");
    repl.extend_from_slice(&source[cs..ce]);
    repl.push(b')');
    record_owned(out, file, source, cs, ce, repl, mutator);
}

/// Replace a span with a fixed token (loop conditions -> `false`, iterated exprs -> `[]`).
fn replace_span(
    out: &mut Vec<Mutant>,
    file: &Path,
    source: &[u8],
    span: mago_span::Span,
    repl: &'static [u8],
    mutator: &'static str,
) {
    let (start, end) = (span.start.offset as usize, span.end.offset as usize);
    if start >= end || end > source.len() {
        return;
    }
    record_owned(out, file, source, start, end, repl.to_vec(), mutator);
}

/// 1-based line number of byte `offset` (count the newlines before it).
fn line_at(source: &[u8], offset: usize) -> u32 {
    source[..offset.min(source.len())]
        .iter()
        .filter(|&&c| c == b'\n')
        .count() as u32
        + 1
}

/// Record one mutation from a mutator's `(start, end, replacement, name)` tuple.
fn record(
    out: &mut Vec<Mutant>,
    file: &Path,
    source: &[u8],
    t: (usize, usize, &'static [u8], &'static str),
) {
    let (start, end, repl, name) = t;
    record_owned(out, file, source, start, end, repl.to_vec(), name);
}

/// Record a mutation whose replacement bytes are computed (e.g. an integer literal
/// N → N±1), not a `&'static` token.
fn record_owned(
    out: &mut Vec<Mutant>,
    file: &Path,
    source: &[u8],
    start: usize,
    end: usize,
    replacement: Vec<u8>,
    name: &'static str,
) {
    out.push(Mutant {
        file: file.to_path_buf(),
        start,
        end,
        replacement,
        mutator: name,
        line: line_at(source, start),
        report_line: line_at(source, start),
    });
}

/// Byte offset of an integer literal used as a comparison operand, unwrapping a leading
/// unary minus — Infection's Number `canMutate` recurses through `UnaryMinus` when
/// deciding whether a literal is part of a comparison.
fn int_operand_offset(e: &Expression) -> Option<usize> {
    match e {
        Expression::Literal(Literal::Integer(i)) => Some(i.span.start.offset as usize),
        Expression::UnaryPrefix(u) if matches!(u.operator, UnaryPrefixOperator::Negation(_)) => {
            int_operand_offset(u.operand)
        }
        _ => None,
    }
}

/// Parent-context needed to reproduce Infection's Number `canMutate` exclusions: an
/// integer literal's eligibility for Increment/Decrement depends on the node it sits in.
#[derive(Default)]
struct NumberContext {
    /// Operand of an equality comparison (`==`, `!=`, `===`, `!==`).
    equality: std::collections::HashSet<usize>,
    /// Operand of a relational/"size" comparison (`<`, `>`, `<=`, `>=`).
    relational: std::collections::HashSet<usize>,
    /// Direct right-hand side of a plain `=` assignment.
    assign_rhs: std::collections::HashSet<usize>,
    /// Direct index of an array access (`$a[0]`).
    array_index: std::collections::HashSet<usize>,
}

/// Record a literal's parent relationship into `ctx`. Called as each node is visited;
/// because the walk is depth-first (parent before child), every literal's context is
/// populated before the literal itself is processed.
fn populate_number_ctx(ctx: &mut NumberContext, node: Node) {
    match node {
        Node::Binary(b) => {
            let set = match b.operator {
                BinaryOperator::Equal(_)
                | BinaryOperator::NotEqual(_)
                | BinaryOperator::Identical(_)
                | BinaryOperator::NotIdentical(_) => Some(&mut ctx.equality),
                BinaryOperator::LessThan(_)
                | BinaryOperator::LessThanOrEqual(_)
                | BinaryOperator::GreaterThan(_)
                | BinaryOperator::GreaterThanOrEqual(_) => Some(&mut ctx.relational),
                _ => None,
            };
            if let Some(set) = set {
                if let Some(o) = int_operand_offset(b.lhs) {
                    set.insert(o);
                }
                if let Some(o) = int_operand_offset(b.rhs) {
                    set.insert(o);
                }
            }
        }
        Node::Assignment(a) if a.operator.is_assign() => {
            if let Expression::Literal(Literal::Integer(i)) = a.rhs {
                ctx.assign_rhs.insert(i.span.start.offset as usize);
            }
        }
        Node::ArrayAccess(aa) => {
            if let Expression::Literal(Literal::Integer(i)) = aa.index {
                if i.value == Some(0) {
                    ctx.array_index.insert(i.span.start.offset as usize);
                }
            }
        }
        _ => {}
    }
}

/// Mark `&&`/`||` sub-expressions that must NOT be negated, mirroring Infection's
/// LogicalAndNegation/LogicalOrNegation `canMutate`: a logical node is skipped when its
/// parent is the SAME logical operator (only the top of a chain is negated) or a `!`
/// (already negated). Populated at parent-visit time, checked when the node is visited.
fn populate_logic_skip(skip: &mut std::collections::HashSet<usize>, node: Node) {
    let mark =
        |skip: &mut std::collections::HashSet<usize>, e: &Expression, and: bool, or: bool| {
            if let Expression::Binary(b) = e {
                let hit = (and && matches!(b.operator, BinaryOperator::And(_)))
                    || (or && matches!(b.operator, BinaryOperator::Or(_)));
                if hit {
                    skip.insert(b.span().start.offset as usize);
                }
            }
        };
    match node {
        Node::Binary(b) => match b.operator {
            BinaryOperator::And(_) => {
                mark(skip, b.lhs, true, false);
                mark(skip, b.rhs, true, false);
            }
            BinaryOperator::Or(_) => {
                mark(skip, b.lhs, false, true);
                mark(skip, b.rhs, false, true);
            }
            _ => {}
        },
        Node::UnaryPrefix(u) if matches!(u.operator, UnaryPrefixOperator::Not(_)) => {
            mark(skip, u.operand, true, true);
        }
        _ => {}
    }
}

/// Emit the two Number mutants for an integer literal: `IncrementInteger` (N→N+1) and
/// `DecrementInteger` (N→N-1), honouring Infection's `canMutate` exclusions:
/// - relational operand → skip BOTH (a size comparison against `N±1` is trivially caught)
/// - `0` under equality/assignment → skip Increment; `1` under equality/assignment → skip Decrement
/// - `0` as an array index → skip Decrement
///   (PHP_INT_MAX and the preg_split-limit niches are not yet reproduced.)
fn record_integer_literal(
    out: &mut Vec<Mutant>,
    file: &Path,
    source: &[u8],
    lit: &Literal,
    ctx: &NumberContext,
) {
    let Literal::Integer(int) = lit else { return };
    let Some(v) = int.value else { return };
    let (start, end) = (int.span.start.offset as usize, int.span.end.offset as usize);
    let v = v as i128;

    let relational = ctx.relational.contains(&start);
    let eq_or_assign = ctx.equality.contains(&start) || ctx.assign_rhs.contains(&start);
    let skip_inc = relational || (v == 0 && eq_or_assign);
    let skip_dec =
        relational || (v == 1 && eq_or_assign) || (v == 0 && ctx.array_index.contains(&start));

    if !skip_inc {
        record_owned(
            out,
            file,
            source,
            start,
            end,
            (v + 1).to_string().into_bytes(),
            "IncrementInteger",
        );
    }
    if !skip_dec {
        record_owned(
            out,
            file,
            source,
            start,
            end,
            (v - 1).to_string().into_bytes(),
            "DecrementInteger",
        );
    }
}

/// Infection's `OneZeroFloat`: mutates ONLY the float literals `0.0`/`1.0`
/// (`1.0`→`0.0`, `0.0`→`1.0`); any other float is left alone.
fn record_float_literal(out: &mut Vec<Mutant>, file: &Path, source: &[u8], lit: &Literal) {
    let Literal::Float(f) = lit else { return };
    let repl: &[u8] = if f.value.0 == 0.0 {
        b"1.0"
    } else if f.value.0 == 1.0 {
        b"0.0"
    } else {
        return;
    };
    let (start, end) = (f.span.start.offset as usize, f.span.end.offset as usize);
    record_owned(out, file, source, start, end, repl.to_vec(), "OneZeroFloat");
}

/// Infection's `isNullReturnValueAllowed`: null may be returned unless the enclosing
/// function declares a non-nullable named/scalar return type. A missing hint, a nullable
/// `?T`, or a union/intersection allows null; every plain type (`int`, `Foo`, `void`, …)
/// does not — matching Infection's over-conservative rule.
fn return_hint_allows_null(hint: Option<&FunctionLikeReturnTypeHint>) -> bool {
    match hint {
        None => true,
        Some(h) => matches!(
            h.hint,
            Hint::Nullable(_) | Hint::Union(_) | Hint::Intersection(_) | Hint::Parenthesized(_)
        ),
    }
}

/// Infection's `ArrayOneItem` (`return $v` with an `array` return type) checks the exact
/// `array` return type — `?array`, `iterable`, unions do not qualify.
fn return_hint_is_array(hint: Option<&FunctionLikeReturnTypeHint>) -> bool {
    matches!(hint, Some(h) if matches!(h.hint, Hint::Array(_)))
}

/// Infection `ReturnValue` mutators that need enclosing-scope context: `return new X()`
/// (NewObject) / `return foo()` (FunctionCall) become `<expr>; return null` when the
/// function permits a null return; `return $v` (ArrayOneItem) becomes a keep-first-item
/// ternary when the function returns `array`. A recursive descent carries the enclosing
/// return type down each function scope (the flat walk has no ancestor chain).
fn walk_return_values(
    node: Node,
    allows_null: bool,
    returns_array: bool,
    out: &mut Vec<Mutant>,
    path: &Path,
    source: &[u8],
) {
    // A function-like node re-scopes the return-type flags for its whole subtree.
    let (child_allows, child_array) = match node {
        Node::Function(f) => (
            return_hint_allows_null(f.return_type_hint.as_ref()),
            return_hint_is_array(f.return_type_hint.as_ref()),
        ),
        Node::Method(m) => (
            return_hint_allows_null(m.return_type_hint.as_ref()),
            return_hint_is_array(m.return_type_hint.as_ref()),
        ),
        Node::Closure(c) => (
            return_hint_allows_null(c.return_type_hint.as_ref()),
            return_hint_is_array(c.return_type_hint.as_ref()),
        ),
        Node::ArrowFunction(a) => (
            return_hint_allows_null(a.return_type_hint.as_ref()),
            return_hint_is_array(a.return_type_hint.as_ref()),
        ),
        _ => (allows_null, returns_array),
    };
    if let Node::Return(r) = node {
        if let Some(expr) = r.value {
            let e = expr.span();
            let (es, ee) = (e.start.offset as usize, e.end.offset as usize);
            // ArrayOneItem: `return $v` -> keep only the first item when returning `array`.
            if returns_array && matches!(expr, Expression::Variable(_)) && ee <= source.len() {
                let v = &source[es..ee];
                let mut repl = b"count(".to_vec();
                repl.extend_from_slice(v);
                repl.extend_from_slice(b") > 1 ? array_slice(");
                repl.extend_from_slice(v);
                repl.extend_from_slice(b", 0, 1, true) : ");
                repl.extend_from_slice(v);
                record_owned(out, path, source, es, ee, repl, "ArrayOneItem");
            }
            if allows_null {
                let mutator = match expr {
                    Expression::Instantiation(inst)
                        if matches!(inst.class, Expression::Identifier(_)) =>
                    {
                        Some("NewObject")
                    }
                    Expression::Call(Call::Function(_)) => Some("FunctionCall"),
                    _ => None,
                };
                if let Some(mutator) = mutator {
                    let ss = r.r#return.span().start.offset as usize;
                    let se = r.terminator.span().end.offset as usize;
                    if ss < se && se <= source.len() && es >= ss && ee <= se {
                        let mut repl = source[es..ee].to_vec();
                        repl.extend_from_slice(b"; return null;");
                        record_owned(out, path, source, ss, se, repl, mutator);
                    }
                }
            }
        }
    }
    for child in node.children() {
        walk_return_values(child, child_allows, child_array, out, path, source);
    }
}

/// Infection's `isNodeWithSideEffects`: a function call, method call, or property fetch.
fn is_side_effect(e: &Expression) -> bool {
    match e {
        Expression::Call(Call::Function(_)) | Expression::Call(Call::Method(_)) => true,
        Expression::Access(a) => matches!(a, Access::Property(_)),
        _ => false,
    }
}

/// Highest (last) 1-based start line among a node and all its descendants — used to find
/// a body line pcov is likely to record for a method-signature mutant.
fn max_descendant_line(node: Node, source: &[u8]) -> u32 {
    let mut m = line_at(source, node.span().start.offset as usize);
    for child in node.children() {
        m = m.max(max_descendant_line(child, source));
    }
    m
}

/// Emit a FunctionSignature visibility mutant for one method: `public`→`protected`
/// (PublicVisibility) or `protected`→`private` (ProtectedVisibility, unless `final`).
/// Reports on the method's declaration line but anchors coverage to the body's first
/// statement (pcov does not record signature lines). Magic (`__*`) and abstract methods
/// are skipped, matching Infection's `canMutate`.
fn emit_visibility_mutant(m: &Method, out: &mut Vec<Mutant>, path: &Path, source: &[u8]) {
    let MethodBody::Concrete(block) = &m.body else {
        return;
    };
    if block.statements.as_slice().is_empty() {
        return;
    }
    if m.name.value.starts_with(b"__") {
        return;
    }
    let is_final = m
        .modifiers
        .as_slice()
        .iter()
        .any(|md| matches!(md, Modifier::Final(_)));
    // Coverage anchor: the deepest (last) content line of the method — pcov does not record
    // signature lines, literal assignments (`$x = 0;`), or block keywords (`try`), but the
    // final `return`/`throw`/call almost always sits on the max content line.
    let cover = max_descendant_line(Node::Method(m), source);
    let report = line_at(source, m.span().start.offset as usize);
    for md in m.modifiers.as_slice() {
        let (mutator, repl): (&'static str, &'static [u8]) = match md {
            Modifier::Public(_) => ("PublicVisibility", b"protected"),
            Modifier::Protected(_) if !is_final => ("ProtectedVisibility", b"private"),
            _ => continue,
        };
        let kw = md.span();
        out.push(Mutant {
            file: path.to_path_buf(),
            start: kw.start.offset as usize,
            end: kw.end.offset as usize,
            replacement: repl.to_vec(),
            mutator,
            line: cover,
            report_line: report,
        });
    }
}

/// Recursive pass for the visibility mutators: `class_no_parent` is `Some(true)` inside a
/// class with no `extends`/`implements` (where Infection's hasSame*ParentMethod is
/// trivially false). Classes WITH a parent are skipped — the parent-method resolution is
/// deferred — so only standalone-class methods mutate.
fn walk_visibility(
    node: Node,
    class_no_parent: Option<bool>,
    out: &mut Vec<Mutant>,
    path: &Path,
    source: &[u8],
) {
    let child_ctx = match node {
        Node::Class(c) => Some(c.extends.is_none() && c.implements.is_none()),
        _ => class_no_parent,
    };
    if let Node::Method(m) = node {
        if class_no_parent == Some(true) {
            emit_visibility_mutant(m, out, path, source);
        }
    }
    for child in node.children() {
        walk_visibility(child, child_ctx, out, path, source);
    }
}

/// Parse `source` and emit every V1 mutant, sorted by byte offset. A parse that
/// yields no usable AST simply produces no mutants (never panics).
pub fn generate_file(path: &Path, source: &[u8]) -> Vec<Mutant> {
    let arena = Bump::new();
    let program = parse_file_content(&arena, FileId::zero(), source);

    let mut out = Vec::new();
    // Depth-first over every AST node; match the operator/literal nodes we mutate.
    let mut stack: Vec<Node> = vec![Node::Program(program)];
    let mut number_ctx = NumberContext::default();
    let mut skip_logic_neg = std::collections::HashSet::new();
    // Array literals used as an assignment target (`[$a, $b] = …`) are destructuring, not
    // values — Infection excludes them from ArrayItemRemoval. Mark them at parent-visit.
    let mut skip_array_item = std::collections::HashSet::new();
    while let Some(node) = stack.pop() {
        stack.extend(node.children());
        populate_number_ctx(&mut number_ctx, node);
        populate_logic_skip(&mut skip_logic_neg, node);
        if let Node::Assignment(a) = node {
            if let Expression::Array(arr) = a.lhs {
                skip_array_item.insert(arr.left_bracket.start.offset as usize);
            }
        }
        match node {
            Node::Binary(b) => {
                for t in mutators::mutate_binary(&b.operator) {
                    record(&mut out, path, source, t);
                }
                // Operand-swap mutators: `a <=> b` -> `b <=> a` (Spaceship); `a ?? b`
                // -> `b ?? a` (Coalesce). Rebuild the whole expression with the operands
                // reordered around the original operator text.
                let swap = match b.operator {
                    BinaryOperator::Spaceship(_) => Some("Spaceship"),
                    BinaryOperator::NullCoalesce(_) => Some("Coalesce"),
                    // Simple (non-chained) `a . b` -> `b . a`. Chained concat has an
                    // Infection special case we don't reproduce (fixture uses 2 operands).
                    BinaryOperator::StringConcat(_) if !matches!(b.lhs, Expression::Binary(inner) if matches!(inner.operator, BinaryOperator::StringConcat(_))) => {
                        Some("Concat")
                    }
                    _ => None,
                };
                if let Some(name) = swap {
                    let seg = |sp: mago_span::Span| {
                        &source[sp.start.offset as usize..sp.end.offset as usize]
                    };
                    let mut repl = Vec::new();
                    repl.extend_from_slice(seg(b.rhs.span()));
                    repl.push(b' ');
                    repl.extend_from_slice(seg(b.operator.span()));
                    repl.push(b' ');
                    repl.extend_from_slice(seg(b.lhs.span()));
                    let whole = b.span();
                    record_owned(
                        &mut out,
                        path,
                        source,
                        whole.start.offset as usize,
                        whole.end.offset as usize,
                        repl,
                        name,
                    );
                }
                // InstanceOf_: `$a instanceof B` -> `true` AND `false`.
                if matches!(b.operator, BinaryOperator::Instanceof(_)) {
                    let whole = b.span();
                    let (s, e) = (whole.start.offset as usize, whole.end.offset as usize);
                    record_owned(
                        &mut out,
                        path,
                        source,
                        s,
                        e,
                        b"true".to_vec(),
                        "InstanceOf_",
                    );
                    record_owned(
                        &mut out,
                        path,
                        source,
                        s,
                        e,
                        b"false".to_vec(),
                        "InstanceOf_",
                    );
                }
                // ConcatOperandRemoval: `$a . $b` drops one operand. Chained `($a.$b).$c`
                // yields only the left (drop the last operand); a simple `$a . $b` yields
                // the right then the left (2 mutants, matching Infection's order).
                if matches!(b.operator, BinaryOperator::StringConcat(_)) {
                    let whole = b.span();
                    let (ws, we) = (whole.start.offset as usize, whole.end.offset as usize);
                    let seg = |sp: mago_span::Span| {
                        source[sp.start.offset as usize..sp.end.offset as usize].to_vec()
                    };
                    let left_is_concat = matches!(b.lhs, Expression::Binary(inner) if matches!(inner.operator, BinaryOperator::StringConcat(_)));
                    if left_is_concat {
                        record_owned(
                            &mut out,
                            path,
                            source,
                            ws,
                            we,
                            seg(b.lhs.span()),
                            "ConcatOperandRemoval",
                        );
                    } else {
                        record_owned(
                            &mut out,
                            path,
                            source,
                            ws,
                            we,
                            seg(b.rhs.span()),
                            "ConcatOperandRemoval",
                        );
                        record_owned(
                            &mut out,
                            path,
                            source,
                            ws,
                            we,
                            seg(b.lhs.span()),
                            "ConcatOperandRemoval",
                        );
                    }
                }
                // LogicalAndNegation / LogicalOrNegation: wrap a top-of-chain `&&`/`||`
                // (parent not the same op nor `!`) in `!(...)`.
                let off = b.span().start.offset as usize;
                if !skip_logic_neg.contains(&off) {
                    match b.operator {
                        BinaryOperator::And(_) => {
                            wrap_not_span(&mut out, path, source, b.span(), "LogicalAndNegation");
                        }
                        BinaryOperator::Or(_) => {
                            wrap_not_span(&mut out, path, source, b.span(), "LogicalOrNegation");
                        }
                        _ => {}
                    }
                }
            }
            // Throw_: `throw $x` -> `$x` (remove the `throw` keyword).
            Node::Throw(t) => {
                let s = t.throw.span();
                record_owned(
                    &mut out,
                    path,
                    source,
                    s.start.offset as usize,
                    s.end.offset as usize,
                    Vec::new(),
                    "Throw_",
                );
            }
            Node::AssignmentOperator(op) => {
                if let Some(t) = mutators::mutate_assignment(op) {
                    record(&mut out, path, source, t);
                }
                // AssignCoalesce: `$a ??= $b` -> `$a = $b` (replace the `??=` token with `=`).
                let s = op.span();
                if &source[s.start.offset as usize..s.end.offset as usize] == b"??=" {
                    replace_span(&mut out, path, source, s, b"=", "AssignCoalesce");
                }
            }
            Node::FunctionCall(fc) => {
                record_unwrap(&mut out, path, source, fc);
                record_call_rewrites(&mut out, path, source, fc);
            }
            // ArrayItemRemoval: remove the FIRST element of a non-empty array literal
            // (`[a, b, c]` -> `[b, c]`), matching Infection's default `remove: first`.
            // Destructuring targets are excluded via `skip_array_item`; the rarer
            // attribute-arg and foreach-list-target exclusions are not yet reproduced.
            Node::Array(a) => {
                let els = a.elements.as_slice();
                let bracket = a.left_bracket.start.offset as usize;
                if !els.is_empty() && !skip_array_item.contains(&bracket) {
                    let de = if els.len() > 1 {
                        els[1].span().start.offset as usize
                    } else {
                        els[0].span().end.offset as usize
                    };
                    let ds = els[0].span().start.offset as usize;
                    if ds < de && de <= source.len() {
                        out.push(Mutant {
                            file: path.to_path_buf(),
                            start: ds,
                            end: de,
                            replacement: Vec::new(),
                            mutator: "ArrayItemRemoval",
                            line: line_at(source, bracket),
                            report_line: line_at(source, bracket),
                        });
                    }
                }
                // SpreadAssignment: a single-element spread array `[...$x]` -> `$x`
                // (unwrap the array entirely, unlike SpreadRemoval which keeps `[$x]`).
                if els.len() == 1 {
                    if let ArrayElement::Variadic(v) = &els[0] {
                        let arr_end = a.right_bracket.end.offset as usize;
                        let val = v.value.span();
                        let (vs, ve) = (val.start.offset as usize, val.end.offset as usize);
                        if bracket < arr_end && ve <= arr_end {
                            record_owned(
                                &mut out,
                                path,
                                source,
                                bracket,
                                arr_end,
                                source[vs..ve].to_vec(),
                                "SpreadAssignment",
                            );
                        }
                    }
                }
            }
            // MatchArmRemoval: drop one arm from a `match` with >1 arms. Delete the arm's
            // bytes plus one separating comma by cutting to the neighbour arm's edge. (Arms
            // with several conditions — `1, 2 => …` — remove one condition in Infection; that
            // multi-condition case is deferred, so it is skipped here.)
            Node::Match(m) => {
                let arms = m.arms.as_slice();
                if arms.len() > 1 {
                    // Infection anchors every arm-removal to the `match` line (the mutant
                    // inherits the Match node's attributes), so both the coverage line and
                    // the reported line use it.
                    let match_line = line_at(source, m.span().start.offset as usize);
                    let cut = |ds: usize, de: usize, out: &mut Vec<Mutant>| {
                        if ds < de && de <= source.len() {
                            out.push(Mutant {
                                file: path.to_path_buf(),
                                start: ds,
                                end: de,
                                replacement: Vec::new(),
                                mutator: "MatchArmRemoval",
                                line: match_line,
                                report_line: match_line,
                            });
                        }
                    };
                    for i in 0..arms.len() {
                        if let MatchArm::Expression(e) = &arms[i] {
                            let conds = e.conditions.as_slice();
                            if conds.len() > 1 {
                                // Multi-condition arm: remove one condition per mutant.
                                for j in 0..conds.len() {
                                    let (ds, de) = if j + 1 < conds.len() {
                                        (
                                            conds[j].span().start.offset as usize,
                                            conds[j + 1].span().start.offset as usize,
                                        )
                                    } else {
                                        (
                                            conds[j - 1].span().end.offset as usize,
                                            conds[j].span().end.offset as usize,
                                        )
                                    };
                                    cut(ds, de, &mut out);
                                }
                                continue;
                            }
                        }
                        let (ds, de) = if i + 1 < arms.len() {
                            (
                                arms[i].span().start.offset as usize,
                                arms[i + 1].span().start.offset as usize,
                            )
                        } else {
                            (
                                arms[i - 1].span().end.offset as usize,
                                arms[i].span().end.offset as usize,
                            )
                        };
                        cut(ds, de, &mut out);
                    }
                }
            }
            // CatchBlockRemoval: drop one non-empty `catch` from a try with >=2 catches.
            // Catch clauses are adjacent (no separator), so deleting the clause's span is
            // enough. Anchored to the `try` line, like Infection (TryCatch attributes).
            Node::Try(t) => {
                let catches = t.catch_clauses.as_slice();
                if catches.len() >= 2 {
                    // Report on the `try` line (Infection), but map coverage to the first
                    // statement of the try body — the `try` keyword line is not one pcov
                    // records, so anchoring coverage there would drop the mutant as uncovered.
                    let try_line = line_at(source, t.r#try.span().start.offset as usize);
                    let cover_line = t
                        .block
                        .statements
                        .as_slice()
                        .first()
                        .map(|st| line_at(source, st.span().start.offset as usize))
                        .unwrap_or(try_line);
                    for c in catches {
                        if c.block.statements.is_empty() {
                            continue;
                        }
                        let s = c.span();
                        let (ds, de) = (s.start.offset as usize, s.end.offset as usize);
                        if ds < de && de <= source.len() {
                            out.push(Mutant {
                                file: path.to_path_buf(),
                                start: ds,
                                end: de,
                                replacement: Vec::new(),
                                mutator: "CatchBlockRemoval",
                                line: cover_line,
                                report_line: try_line,
                            });
                        }
                    }
                }
            }
            // Boolean `ArrayItem`: a keyed element `$k => $v` whose key or value has a side
            // effect (a call or property fetch) becomes `$k > $v`.
            Node::KeyValueArrayElement(kv) => {
                if is_side_effect(kv.key) || is_side_effect(kv.value) {
                    let ks = kv.key.span().start.offset as usize;
                    let v = kv.value.span();
                    let (vs, ve) = (v.start.offset as usize, v.end.offset as usize);
                    if ks < ve && ve <= source.len() {
                        let mut repl = source[ks..kv.key.span().end.offset as usize].to_vec();
                        repl.extend_from_slice(b" > ");
                        repl.extend_from_slice(&source[vs..ve]);
                        record_owned(&mut out, path, source, ks, ve, repl, "ArrayItem");
                    }
                }
            }
            // SpreadRemoval: `[...$x]` -> `[$x]` (delete the `...` before the value).
            // SpreadOneItem: `...$x` -> `[...$x][0]` (keep only the spread's first element).
            Node::VariadicArrayElement(v) => {
                let e = v.ellipsis;
                let (s, en) = (e.start.offset as usize, e.end.offset as usize);
                if s < en && en <= source.len() {
                    record_owned(&mut out, path, source, s, en, Vec::new(), "SpreadRemoval");
                }
                let elem_end = v.value.span().end.offset as usize;
                if s < elem_end && elem_end <= source.len() {
                    let mut repl = vec![b'['];
                    repl.extend_from_slice(&source[s..elem_end]);
                    repl.extend_from_slice(b"][0]");
                    record_owned(&mut out, path, source, s, elem_end, repl, "SpreadOneItem");
                }
            }
            // NullSafeMethodCall: `$x?->m()` -> `$x->m()` (replace `?->` with `->`). A call
            // on null is a fatal Error (killed); the property-read variant only warns, so
            // NullSafePropertyCall is deferred until the runner honours failOnWarning.
            Node::NullSafeMethodCall(m) => {
                replace_span(
                    &mut out,
                    path,
                    source,
                    m.question_mark_arrow,
                    b"->",
                    "NullSafeMethodCall",
                );
            }
            // Loop control swap: `break` <-> `continue`.
            Node::Break(b) => {
                let s = b.r#break.span();
                record_owned(
                    &mut out,
                    path,
                    source,
                    s.start.offset as usize,
                    s.end.offset as usize,
                    b"continue".to_vec(),
                    "Break_",
                );
            }
            Node::Continue(c) => {
                let s = c.r#continue.span();
                record_owned(
                    &mut out,
                    path,
                    source,
                    s.start.offset as usize,
                    s.end.offset as usize,
                    b"break".to_vec(),
                    "Continue_",
                );
            }
            // Removal: a statement that is JUST a call becomes a no-op (removed).
            Node::ExpressionStatement(es) => {
                if let Expression::Call(call) = es.expression {
                    let name = match call {
                        Call::Function(_) => "FunctionCallRemoval",
                        Call::Method(_) | Call::NullSafeMethod(_) | Call::StaticMethod(_) => {
                            "MethodCallRemoval"
                        }
                    };
                    let sp = es.span();
                    record_owned(
                        &mut out,
                        path,
                        source,
                        sp.start.offset as usize,
                        sp.end.offset as usize,
                        Vec::new(),
                        name,
                    );
                }
            }
            // Ternary: `c ? then : else` -> `c ? else : then` (swap the branches).
            Node::Conditional(c) => {
                if let Some(then) = c.then {
                    let ts = then.span();
                    let es = c.r#else.span();
                    let (tstart, tend) = (ts.start.offset as usize, ts.end.offset as usize);
                    let (estart, eend) = (es.start.offset as usize, es.end.offset as usize);
                    let mut repl = Vec::new();
                    repl.extend_from_slice(&source[estart..eend]);
                    repl.extend_from_slice(b" : ");
                    repl.extend_from_slice(&source[tstart..tend]);
                    record_owned(&mut out, path, source, tstart, eend, repl, "Ternary");
                }
            }
            // ReturnValue mutators: `return $this`->null (This), `return N`->`-N`
            // (IntegerNegation), `return F`->`-F` (FloatNegation).
            Node::Return(r) => {
                if let Some(val) = r.value {
                    let s = val.span();
                    let (start, end) = (s.start.offset as usize, s.end.offset as usize);
                    match val {
                        Expression::Variable(Variable::Direct(v))
                            if v.name == b"$this" || v.name == b"this" =>
                        {
                            record_owned(
                                &mut out,
                                path,
                                source,
                                start,
                                end,
                                b"null".to_vec(),
                                "This",
                            );
                        }
                        Expression::Literal(Literal::Integer(_)) => {
                            let mut repl = vec![b'-'];
                            repl.extend_from_slice(&source[start..end]);
                            record_owned(
                                &mut out,
                                path,
                                source,
                                start,
                                end,
                                repl,
                                "IntegerNegation",
                            );
                        }
                        Expression::Literal(Literal::Float(_)) => {
                            let mut repl = vec![b'-'];
                            repl.extend_from_slice(&source[start..end]);
                            record_owned(&mut out, path, source, start, end, repl, "FloatNegation");
                        }
                        _ => {}
                    }
                }
            }
            Node::Literal(l) => {
                if let Some(t) = mutators::mutate_literal(l) {
                    record(&mut out, path, source, t);
                }
                record_integer_literal(&mut out, path, source, l, &number_ctx);
                record_float_literal(&mut out, path, source, l);
            }
            Node::UnaryPrefix(u) => match &u.operator {
                UnaryPrefixOperator::PreIncrement(s) => {
                    record(
                        &mut out,
                        path,
                        source,
                        mutators::mutate_unary_suffix(*s, true),
                    );
                }
                UnaryPrefixOperator::PreDecrement(s) => {
                    record(
                        &mut out,
                        path,
                        source,
                        mutators::mutate_unary_suffix(*s, false),
                    );
                }
                UnaryPrefixOperator::Not(s) => {
                    // LogicalNot: remove the `!` (unwrap). We don't handle `!!` (Infection
                    // skips it); the oracle fixture avoids doubled negation.
                    record_owned(
                        &mut out,
                        path,
                        source,
                        s.start.offset as usize,
                        s.end.offset as usize,
                        Vec::new(),
                        "LogicalNot",
                    );
                }
                UnaryPrefixOperator::BitwiseNot(s) => {
                    // BitwiseNot: `~$x` -> `$x` (remove the `~`).
                    record_owned(
                        &mut out,
                        path,
                        source,
                        s.start.offset as usize,
                        s.end.offset as usize,
                        Vec::new(),
                        "BitwiseNot",
                    );
                }
                other => {
                    if let Some(t) = mutators::mutate_cast(other) {
                        record(&mut out, path, source, t);
                    }
                }
            },
            Node::UnaryPostfix(u) => match &u.operator {
                UnaryPostfixOperator::PostIncrement(s) => {
                    record(
                        &mut out,
                        path,
                        source,
                        mutators::mutate_unary_suffix(*s, true),
                    );
                }
                UnaryPostfixOperator::PostDecrement(s) => {
                    record(
                        &mut out,
                        path,
                        source,
                        mutators::mutate_unary_suffix(*s, false),
                    );
                }
            },
            // IfNegation: `if (c)` -> `if (!(c))`. Wrap the condition in `!(...)`.
            Node::If(i) => {
                negate_condition(&mut out, path, source, i.condition, "IfNegation");
            }
            // ElseIfNegation: `elseif (c)` -> `elseif (!(c))` (statement-form elseif).
            Node::IfStatementBodyElseIfClause(ei) => {
                negate_condition(&mut out, path, source, ei.condition, "ElseIfNegation");
            }
            // While_: `while (c)` -> `while (false)` (loop body never runs).
            Node::While(w) => {
                let s = w.condition.span();
                replace_span(&mut out, path, source, s, b"false", "While_");
            }
            // DoWhile: `do {…} while (c)` -> `… while (false)` (body runs exactly once).
            // Splice at the condition, but anchor the reported line to the `do` keyword —
            // that is Infection's `originalStartLine` for a do-while (the statement start).
            Node::DoWhile(d) => {
                let cs = d.condition.span();
                let (start, end) = (cs.start.offset as usize, cs.end.offset as usize);
                if start < end && end <= source.len() {
                    out.push(Mutant {
                        file: path.to_path_buf(),
                        start,
                        end,
                        replacement: b"false".to_vec(),
                        mutator: "DoWhile",
                        // Coverage anchor: the condition line (executable, pcov records it).
                        line: line_at(source, start),
                        // Report anchor: the `do` keyword line (Infection's originalStartLine).
                        report_line: line_at(source, d.r#do.span().start.offset as usize),
                    });
                }
            }
            // For_: `for (init; conds; loop)` -> the whole condition list becomes `false`.
            Node::For(f) => {
                if let (Some(first), Some(last)) = (f.conditions.first(), f.conditions.last()) {
                    let (cs, ce) = (
                        first.span().start.offset as usize,
                        last.span().end.offset as usize,
                    );
                    if cs < ce && ce <= source.len() {
                        record_owned(&mut out, path, source, cs, ce, b"false".to_vec(), "For_");
                    }
                }
            }
            // Foreach_: `foreach ($xs as …)` -> `foreach ([] as …)` (iterates nothing).
            Node::Foreach(fe) => {
                let s = fe.expression.span();
                replace_span(&mut out, path, source, s, b"[]", "Foreach_");
            }
            // CloneRemoval: `clone $x` -> `$x` (re-emit just the cloned expression).
            Node::Clone(c) => {
                let whole = c.clone.span().join(c.object.span());
                let o = c.object.span();
                let (start, end) = (whole.start.offset as usize, whole.end.offset as usize);
                let (os, oe) = (o.start.offset as usize, o.end.offset as usize);
                if start < end && oe <= source.len() {
                    record_owned(
                        &mut out,
                        path,
                        source,
                        start,
                        end,
                        source[os..oe].to_vec(),
                        "CloneRemoval",
                    );
                }
            }
            // YieldValue: `yield $k => $v` -> `yield $v` (drop the key).
            Node::Yield(Yield::Pair(p)) => {
                let ks = p.key.span().start.offset as usize;
                let v = p.value.span();
                let (vs, ve) = (v.start.offset as usize, v.end.offset as usize);
                if ks < ve && ve <= source.len() {
                    record_owned(
                        &mut out,
                        path,
                        source,
                        ks,
                        ve,
                        source[vs..ve].to_vec(),
                        "YieldValue",
                    );
                }
            }
            _ => {}
        }
    }
    // Second, scope-aware pass for the ReturnValue mutators (need the enclosing return type).
    walk_return_values(Node::Program(program), true, false, &mut out, path, source);
    // Visibility mutators need the enclosing class (parent status) — another recursive pass.
    walk_visibility(Node::Program(program), None, &mut out, path, source);
    out.sort_by_key(|m| m.start);
    out
}

/// One mutation: replace `file[start..end]` with `replacement`.
///
/// `start`/`end` are byte offsets into the file's source; `line` is 1-based (the
/// line the mutated token starts on); `mutator` is the Infection-compatible name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutant {
    pub file: PathBuf,
    pub start: usize,
    pub end: usize,
    pub replacement: Vec<u8>,
    pub mutator: &'static str,
    /// 1-based line used to look up covering tests — must be an executable line pcov
    /// records (i.e. where the mutated bytes sit).
    pub line: u32,
    /// 1-based line reported to the user / oracle. Equals `line` for every mutator
    /// except do-while, where Infection anchors to the `do` keyword (a line pcov may
    /// not record), so coverage and reporting need different anchors.
    pub report_line: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_arithmetic_and_comparison_mutants() {
        let src = b"<?php\nfunction f($a, $b) { return $a + $b > 3; }\n";
        let mut got: Vec<(&str, Vec<u8>)> = generate_file(std::path::Path::new("f.php"), src)
            .into_iter()
            .map(|m| (m.mutator, m.replacement))
            .collect();
        got.sort();
        assert!(got.contains(&("Plus", b"-".to_vec())), "got {got:?}");
        assert!(
            got.contains(&("GreaterThan", b">=".to_vec())),
            "got {got:?}"
        );
    }

    #[test]
    fn line_is_one_based() {
        let src = b"<?php\n\n$x = 1 + 2;\n";
        let m = generate_file(std::path::Path::new("f.php"), src)
            .into_iter()
            .find(|m| m.mutator == "Plus")
            .unwrap();
        assert_eq!(m.line, 3, "the `+` is on source line 3");
    }

    #[test]
    fn generates_cast_unwrap_mutant() {
        let src = b"<?php\nfunction f($s) { return (int) $s; }\n";
        let m = generate_file(std::path::Path::new("f.php"), src)
            .into_iter()
            .find(|m| m.mutator == "CastInt")
            .expect("CastInt mutant");
        assert_eq!(&src[m.start..m.end], b"(int)");
        assert_eq!(m.replacement, b"", "unwrap removes the cast");
    }

    #[test]
    fn unwraps_string_function_to_first_arg() {
        let src = b"<?php\nfunction f($s) { return strtolower($s); }\n";
        let m = generate_file(std::path::Path::new("f.php"), src)
            .into_iter()
            .find(|m| m.mutator == "UnwrapStrToLower")
            .expect("UnwrapStrToLower mutant");
        assert_eq!(&src[m.start..m.end], b"strtolower($s)");
        assert_eq!(m.replacement, b"$s");
    }

    #[test]
    fn patching_the_span_edits_the_operator() {
        // The reported [start,end) must isolate exactly the `+` byte so a splice works.
        let src = b"<?php\n$x = 1 + 2;\n";
        let m = generate_file(std::path::Path::new("f.php"), src)
            .into_iter()
            .find(|m| m.mutator == "Plus")
            .unwrap();
        assert_eq!(&src[m.start..m.end], b"+");
    }
}
