//! The V1 mutator set: pure functions mapping a mago AST operator/literal node
//! to its byte-range replacement, keyed by Infection's exact mutator names.

use mago_span::HasSpan;
use mago_syntax::ast::binary::BinaryOperator;
use mago_syntax::ast::literal::Literal;

/// Map a binary operator to its Infection-compatible mutation:
/// `(start_offset, end_offset, replacement_bytes, mutator_name)`.
///
/// The byte offsets come straight from the operator token's span, so the caller
/// patches `source[start..end] = replacement`. Returns `None` for operators not in
/// the set (string concat `.`, coalesce `??`, spaceship `<=>`, `**`, …).
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
        // Bitwise — Infection swaps &<->| and ^->& , and reverses the shifts.
        // (mago's shift variants are Left/RightShift; Infection names them Shift{Left,Right}.)
        BinaryOperator::BitwiseAnd(_) => (b"|", "BitwiseAnd"),
        BinaryOperator::BitwiseOr(_) => (b"&", "BitwiseOr"),
        BinaryOperator::BitwiseXor(_) => (b"&", "BitwiseXor"),
        BinaryOperator::LeftShift(_) => (b">>", "ShiftLeft"),
        BinaryOperator::RightShift(_) => (b"<<", "ShiftRight"),
        _ => return None,
    };
    Some((start, end, repl, name))
}

/// `true`→`false` (`TrueValue`) / `false`→`true` (`FalseValue`). Only boolean
/// literals mutate; every other literal (null, int, float, string) returns `None`.
pub fn mutate_literal(lit: &Literal) -> Option<(usize, usize, &'static [u8], &'static str)> {
    let (span, repl, name): (mago_span::Span, &'static [u8], &'static str) = match lit {
        Literal::True(k) => (k.span(), b"false", "TrueValue"),
        Literal::False(k) => (k.span(), b"true", "FalseValue"),
        _ => return None,
    };
    Some((
        span.start.offset as usize,
        span.end.offset as usize,
        repl,
        name,
    ))
}

/// `++`↔`--`. `is_increment` picks the direction and the Infection name; the caller
/// passes the operator token's span (from a pre- or post-fix increment/decrement).
pub fn mutate_unary_suffix(
    op_span: mago_span::Span,
    is_increment: bool,
) -> (usize, usize, &'static [u8], &'static str) {
    let (repl, name): (&'static [u8], &'static str) = if is_increment {
        (b"--", "IncrementInteger")
    } else {
        (b"++", "DecrementInteger")
    };
    (
        op_span.start.offset as usize,
        op_span.end.offset as usize,
        repl,
        name,
    )
}

/// Infection's cast mutators UNWRAP the cast (`(int)$x` → `$x`), so we remove the
/// cast operator's token span (replace with nothing). mago spells some casts several
/// ways (int/integer, float/double/real, string/binary, bool/boolean); all map to the
/// one Infection name. `(unset)`/`(void)` have no Infection mutator → `None`.
pub fn mutate_cast(
    op: &mago_syntax::ast::unary::UnaryPrefixOperator,
) -> Option<(usize, usize, &'static [u8], &'static str)> {
    use mago_syntax::ast::unary::UnaryPrefixOperator as U;
    let (span, name): (mago_span::Span, &'static str) = match op {
        U::IntCast(s, _) | U::IntegerCast(s, _) => (*s, "CastInt"),
        U::FloatCast(s, _) | U::DoubleCast(s, _) | U::RealCast(s, _) => (*s, "CastFloat"),
        U::StringCast(s, _) | U::BinaryCast(s, _) => (*s, "CastString"),
        U::BoolCast(s, _) | U::BooleanCast(s, _) => (*s, "CastBool"),
        U::ArrayCast(s, _) => (*s, "CastArray"),
        U::ObjectCast(s, _) => (*s, "CastObject"),
        _ => return None,
    };
    Some((
        span.start.offset as usize,
        span.end.offset as usize,
        b"",
        name,
    ))
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
    fn bitwise_and_becomes_or() {
        use mago_syntax::ast::binary::BinaryOperator;
        let (start, end, repl, name) =
            mutate_binary(&BinaryOperator::BitwiseAnd(span(0, 1))).unwrap();
        assert_eq!((start, end), (0, 1));
        assert_eq!(repl, b"|");
        assert_eq!(name, "BitwiseAnd");
    }

    #[test]
    fn bitwise_or_and_xor_become_and() {
        use mago_syntax::ast::binary::BinaryOperator;
        assert_eq!(
            mutate_binary(&BinaryOperator::BitwiseOr(span(0, 1)))
                .unwrap()
                .2,
            b"&"
        );
        assert_eq!(
            mutate_binary(&BinaryOperator::BitwiseOr(span(0, 1)))
                .unwrap()
                .3,
            "BitwiseOr"
        );
        assert_eq!(
            mutate_binary(&BinaryOperator::BitwiseXor(span(0, 1)))
                .unwrap()
                .2,
            b"&"
        );
        assert_eq!(
            mutate_binary(&BinaryOperator::BitwiseXor(span(0, 1)))
                .unwrap()
                .3,
            "BitwiseXor"
        );
    }

    #[test]
    fn shifts_swap_direction() {
        use mago_syntax::ast::binary::BinaryOperator;
        // mago variant LeftShift -> Infection mutator name "ShiftLeft", replacement ">>".
        let (_, _, repl, name) = mutate_binary(&BinaryOperator::LeftShift(span(0, 2))).unwrap();
        assert_eq!(repl, b">>");
        assert_eq!(name, "ShiftLeft");
        let (_, _, repl, name) = mutate_binary(&BinaryOperator::RightShift(span(0, 2))).unwrap();
        assert_eq!(repl, b"<<");
        assert_eq!(name, "ShiftRight");
    }

    #[test]
    fn true_literal_becomes_false() {
        use mago_syntax::ast::keyword::Keyword;
        use mago_syntax::ast::literal::Literal;
        let lit = Literal::True(Keyword {
            span: span(3, 7),
            value: b"true",
        });
        let (start, end, repl, name) = mutate_literal(&lit).unwrap();
        assert_eq!((start, end), (3, 7));
        assert_eq!(repl, b"false");
        assert_eq!(name, "TrueValue");
    }

    #[test]
    fn false_literal_becomes_true() {
        use mago_syntax::ast::keyword::Keyword;
        use mago_syntax::ast::literal::Literal;
        let lit = Literal::False(Keyword {
            span: span(0, 5),
            value: b"false",
        });
        let (_, _, repl, name) = mutate_literal(&lit).unwrap();
        assert_eq!(repl, b"true");
        assert_eq!(name, "FalseValue");
    }

    #[test]
    fn null_literal_is_not_mutated() {
        use mago_syntax::ast::keyword::Keyword;
        use mago_syntax::ast::literal::Literal;
        assert!(mutate_literal(&Literal::Null(Keyword {
            span: span(0, 4),
            value: b"null"
        }))
        .is_none());
    }

    #[test]
    fn casts_unwrap_to_empty() {
        use mago_syntax::ast::unary::UnaryPrefixOperator as U;
        // `(int)` spans bytes [0,5); unwrap => remove it (replacement empty).
        let (start, end, repl, name) = mutate_cast(&U::IntCast(span(0, 5), b"(int)")).unwrap();
        assert_eq!((start, end), (0, 5));
        assert_eq!(repl, b"");
        assert_eq!(name, "CastInt");
        // mago spells float as Double — still the one Infection name.
        assert_eq!(
            mutate_cast(&U::DoubleCast(span(0, 7), b"(double)"))
                .unwrap()
                .3,
            "CastFloat"
        );
        assert_eq!(
            mutate_cast(&U::BoolCast(span(0, 6), b"(bool)")).unwrap().3,
            "CastBool"
        );
        assert_eq!(
            mutate_cast(&U::ArrayCast(span(0, 7), b"(array)"))
                .unwrap()
                .3,
            "CastArray"
        );
        assert_eq!(
            mutate_cast(&U::ObjectCast(span(0, 8), b"(object)"))
                .unwrap()
                .3,
            "CastObject"
        );
    }

    #[test]
    fn unset_cast_is_not_mutated() {
        use mago_syntax::ast::unary::UnaryPrefixOperator as U;
        assert!(mutate_cast(&U::UnsetCast(span(0, 7), b"(unset)")).is_none());
    }

    #[test]
    fn increment_becomes_decrement() {
        let (start, end, repl, name) = mutate_unary_suffix(span(2, 4), true);
        assert_eq!((start, end), (2, 4));
        assert_eq!(repl, b"--");
        assert_eq!(name, "IncrementInteger");
    }

    #[test]
    fn decrement_becomes_increment() {
        let (_, _, repl, name) = mutate_unary_suffix(span(2, 4), false);
        assert_eq!(repl, b"++");
        assert_eq!(name, "DecrementInteger");
    }
}
