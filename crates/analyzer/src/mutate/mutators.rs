//! The V1 mutator set: pure functions mapping a mago AST operator/literal node
//! to its byte-range replacement, keyed by Infection's exact mutator names.

use mago_span::HasSpan;
use mago_syntax::ast::binary::BinaryOperator;

/// Map a binary operator to its Infection-compatible mutation:
/// `(start_offset, end_offset, replacement_bytes, mutator_name)`.
///
/// The byte offsets come straight from the operator token's span, so the caller
/// patches `source[start..end] = replacement`. Returns `None` for operators the
/// V1 set does not mutate (bitwise, concat, coalesce, spaceship, `**`, …).
pub fn mutate_binary(op: &BinaryOperator) -> Option<(usize, usize, &'static [u8], &'static str)> {
    let s = op.span();
    let (start, end) = (s.start.offset as usize, s.end.offset as usize);
    let (repl, name): (&'static [u8], &'static str) = match op {
        BinaryOperator::Addition(_) => (b"-", "Plus"),
        BinaryOperator::Subtraction(_) => (b"+", "Minus"),
        BinaryOperator::Multiplication(_) => (b"/", "Multiplication"),
        BinaryOperator::Division(_) => (b"*", "Division"),
        BinaryOperator::Modulo(_) => (b"*", "Modulus"),
        BinaryOperator::LessThan(_) => (b"<=", "LessThan"),
        BinaryOperator::LessThanOrEqual(_) => (b"<", "LessThanOrEqualTo"),
        BinaryOperator::GreaterThan(_) => (b">=", "GreaterThan"),
        BinaryOperator::GreaterThanOrEqual(_) => (b">", "GreaterThanOrEqualTo"),
        BinaryOperator::Equal(_) => (b"!=", "Equal"),
        BinaryOperator::NotEqual(_) => (b"==", "NotEqual"),
        BinaryOperator::Identical(_) => (b"!==", "Identical"),
        BinaryOperator::NotIdentical(_) => (b"===", "NotIdentical"),
        BinaryOperator::And(_) => (b"||", "LogicalAnd"),
        BinaryOperator::Or(_) => (b"&&", "LogicalOr"),
        _ => return None,
    };
    Some((start, end, repl, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mago_database::file::FileId;
    use mago_span::{Position, Span};

    /// A one-file span over `[a, b)`; the offsets are what `mutate_binary` echoes back.
    fn span(a: u32, b: u32) -> Span {
        Span::new(FileId::zero(), Position::new(a), Position::new(b))
    }

    #[test]
    fn plus_becomes_minus() {
        use mago_syntax::ast::binary::BinaryOperator;
        let op = BinaryOperator::Addition(span(10, 11));
        let (start, end, repl, name) = mutate_binary(&op).unwrap();
        assert_eq!((start, end), (10, 11));
        assert_eq!(repl, b"-");
        assert_eq!(name, "Plus");
    }

    #[test]
    fn greater_than_becomes_greater_or_equal() {
        use mago_syntax::ast::binary::BinaryOperator;
        let op = BinaryOperator::GreaterThan(span(4, 5));
        let (start, end, repl, name) = mutate_binary(&op).unwrap();
        assert_eq!((start, end), (4, 5));
        assert_eq!(repl, b">=");
        assert_eq!(name, "GreaterThan");
    }

    #[test]
    fn bitwise_and_is_not_mutated() {
        use mago_syntax::ast::binary::BinaryOperator;
        assert!(mutate_binary(&BinaryOperator::BitwiseAnd(span(0, 1))).is_none());
    }
}
