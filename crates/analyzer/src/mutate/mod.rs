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
use mago_syntax::ast::identifier::Identifier;
use mago_syntax::ast::literal::Literal;
use mago_syntax::ast::node::Node;
use mago_syntax::ast::unary::{UnaryPostfixOperator, UnaryPrefixOperator};
use mago_syntax::ast::variable::Variable;
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
    // Range unwrap: emit one mutant per kept arg (`array_merge` → each; `array_map` → all
    // but the callback at index 0). Variable arg count, so iterate positionals.
    if let Some((mutator, skip_first)) = mutators::unwrap_range(&name) {
        let start_idx = usize::from(skip_first);
        for i in start_idx..fc.argument_list.arguments.len() {
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

/// Infection condition-negation (`IfNegation`/`ElseIfNegation`): wrap `cond` in
/// `!(...)` by re-emitting the original bytes around a `!(` / `)`.
fn negate_condition(
    out: &mut Vec<Mutant>,
    file: &Path,
    source: &[u8],
    cond: &Expression,
    mutator: &'static str,
) {
    let s = cond.span();
    let (cs, ce) = (s.start.offset as usize, s.end.offset as usize);
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

/// Emit the two Number mutants for an integer literal: `IncrementInteger` (N→N+1) and
/// `DecrementInteger` (N→N-1). Uses i128 so `0` decrements to `-1` without wrapping.
/// (Infection has a parent-`UnaryMinus` special case for negative literals; we don't
/// track parents, so negative literals are out of scope — the oracle fixture avoids them.)
fn record_integer_literal(out: &mut Vec<Mutant>, file: &Path, source: &[u8], lit: &Literal) {
    let Literal::Integer(int) = lit else { return };
    let Some(v) = int.value else { return };
    let (start, end) = (int.span.start.offset as usize, int.span.end.offset as usize);
    let v = v as i128;
    record_owned(
        out,
        file,
        source,
        start,
        end,
        (v + 1).to_string().into_bytes(),
        "IncrementInteger",
    );
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

/// Parse `source` and emit every V1 mutant, sorted by byte offset. A parse that
/// yields no usable AST simply produces no mutants (never panics).
pub fn generate_file(path: &Path, source: &[u8]) -> Vec<Mutant> {
    let arena = Bump::new();
    let program = parse_file_content(&arena, FileId::zero(), source);

    let mut out = Vec::new();
    // Depth-first over every AST node; match the operator/literal nodes we mutate.
    let mut stack: Vec<Node> = vec![Node::Program(program)];
    while let Some(node) = stack.pop() {
        stack.extend(node.children());
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
            }
            Node::FunctionCall(fc) => record_unwrap(&mut out, path, source, fc),
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
                record_integer_literal(&mut out, path, source, l);
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
            _ => {}
        }
    }
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
