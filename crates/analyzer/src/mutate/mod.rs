//! Native mutation generation over the mago AST (V1 mutator set).
//!
//! `mutators` maps individual AST operator/literal nodes to their byte-range
//! replacement; higher tasks walk a file's AST and join the result with per-test
//! coverage. The byte offsets come straight from mago spans, so a mutation is a
//! splice of `source[start..end]` — no pretty-printer, no re-emission.
use std::path::{Path, PathBuf};

pub mod mutators;

use bumpalo::Bump;
use mago_database::file::FileId;
use mago_syntax::ast::node::Node;
use mago_syntax::ast::unary::{UnaryPostfixOperator, UnaryPrefixOperator};
use mago_syntax::parser::parse_file_content;

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
    out.push(Mutant {
        file: file.to_path_buf(),
        start,
        end,
        replacement: repl.to_vec(),
        mutator: name,
        line: line_at(source, start),
    });
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
                if let Some(t) = mutators::mutate_binary(&b.operator) {
                    record(&mut out, path, source, t);
                }
            }
            Node::Literal(l) => {
                if let Some(t) = mutators::mutate_literal(l) {
                    record(&mut out, path, source, t);
                }
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
                _ => {}
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
    pub line: u32,
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
