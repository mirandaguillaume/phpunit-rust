//! The V1 mutator set: pure functions mapping a mago AST operator/literal node
//! to its byte-range replacement, keyed by Infection's exact mutator names.

use mago_span::HasSpan;
use mago_syntax::ast::binary::BinaryOperator;
use mago_syntax::ast::literal::Literal;

/// All Infection-compatible mutations of a binary operator, each as
/// `(start_offset, end_offset, replacement_bytes, mutator_name)`. A single operator
/// often has SEVERAL Infection mutators (e.g. `==` → both `Equal` (`!=`) and
/// `EqualIdentical` (`===`); `>` → both `GreaterThan` (`>=`) and its negation
/// `GreaterThanNegotiation` (`<=`)). All share the operator token's span. Empty for
/// operators not in the set (`.`, `??`, `<=>`, …).
pub fn mutate_binary(op: &BinaryOperator) -> Vec<(usize, usize, &'static [u8], &'static str)> {
    let s = op.span();
    let (start, end) = (s.start.offset as usize, s.end.offset as usize);
    let muts: &[(&'static [u8], &'static str)] = match op {
        BinaryOperator::Addition(_) => &[(b"-", "Plus")],
        BinaryOperator::Subtraction(_) => &[(b"+", "Minus")],
        BinaryOperator::Multiplication(_) => &[(b"/", "Multiplication")],
        BinaryOperator::Division(_) => &[(b"*", "Division")],
        BinaryOperator::Modulo(_) => &[(b"*", "Modulus")],
        BinaryOperator::Exponentiation(_) => &[(b"/", "Exponentiation")],
        // Comparison: the ConditionalBoundary shift AND the ConditionalNegotiation flip.
        BinaryOperator::LessThan(_) => &[(b"<=", "LessThan"), (b">=", "LessThanNegotiation")],
        BinaryOperator::LessThanOrEqual(_) => &[
            (b"<", "LessThanOrEqualTo"),
            (b">", "LessThanOrEqualToNegotiation"),
        ],
        BinaryOperator::GreaterThan(_) => {
            &[(b">=", "GreaterThan"), (b"<=", "GreaterThanNegotiation")]
        }
        BinaryOperator::GreaterThanOrEqual(_) => &[
            (b">", "GreaterThanOrEqualTo"),
            (b"<", "GreaterThanOrEqualToNegotiation"),
        ],
        // Equality: the ConditionalNegotiation flip AND the Boolean loosen/tighten.
        BinaryOperator::Equal(_) => &[(b"!=", "Equal"), (b"===", "EqualIdentical")],
        BinaryOperator::NotEqual(_) => &[(b"==", "NotEqual"), (b"!==", "NotEqualNotIdentical")],
        BinaryOperator::Identical(_) => &[(b"!==", "Identical"), (b"==", "IdenticalEqual")],
        BinaryOperator::NotIdentical(_) => {
            &[(b"===", "NotIdentical"), (b"!=", "NotIdenticalNotEqual")]
        }
        BinaryOperator::And(_) => &[(b"||", "LogicalAnd")],
        BinaryOperator::Or(_) => &[(b"&&", "LogicalOr")],
        // Bitwise — swap &<->| and ^->& , reverse the shifts.
        // (mago's shift variants are Left/RightShift; Infection names them Shift{Left,Right}.)
        BinaryOperator::BitwiseAnd(_) => &[(b"|", "BitwiseAnd")],
        BinaryOperator::BitwiseOr(_) => &[(b"&", "BitwiseOr")],
        BinaryOperator::BitwiseXor(_) => &[(b"&", "BitwiseXor")],
        BinaryOperator::LeftShift(_) => &[(b">>", "ShiftLeft")],
        BinaryOperator::RightShift(_) => &[(b"<<", "ShiftRight")],
        _ => &[],
    };
    muts.iter().map(|&(r, n)| (start, end, r, n)).collect()
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

/// `++`↔`--` (Infection Arithmetic `Increment`/`Decrement` — NOT the Number
/// `IncrementInteger`/`DecrementInteger`, which mutate integer *literals*).
/// `is_increment` picks the direction; the caller passes the operator token's span.
pub fn mutate_unary_suffix(
    op_span: mago_span::Span,
    is_increment: bool,
) -> (usize, usize, &'static [u8], &'static str) {
    let (repl, name): (&'static [u8], &'static str) = if is_increment {
        (b"--", "Increment")
    } else {
        (b"++", "Decrement")
    };
    (
        op_span.start.offset as usize,
        op_span.end.offset as usize,
        repl,
        name,
    )
}

/// Compound-assignment operators (Infection Arithmetic `*Equal`): swap the arithmetic
/// half — `+=`↔`-=`, `*=`→`/=`, `/=`→`*=`, `%=`→`*=`, `**=`→`/=`. (`*= -1` has an
/// Infection skip-case we don't reproduce; the oracle fixture avoids it.)
pub fn mutate_assignment(
    op: &mago_syntax::ast::assignment::AssignmentOperator,
) -> Option<(usize, usize, &'static [u8], &'static str)> {
    use mago_syntax::ast::assignment::AssignmentOperator as A;
    let (span, repl, name): (mago_span::Span, &'static [u8], &'static str) = match op {
        A::Addition(s) => (*s, b"-=", "PlusEqual"),
        A::Subtraction(s) => (*s, b"+=", "MinusEqual"),
        A::Multiplication(s) => (*s, b"/=", "MulEqual"),
        A::Division(s) => (*s, b"*=", "DivEqual"),
        A::Modulo(s) => (*s, b"*=", "ModEqual"),
        A::Exponentiation(s) => (*s, b"/=", "PowEqual"),
        _ => return None,
    };
    Some((
        span.start.offset as usize,
        span.end.offset as usize,
        repl,
        name,
    ))
}

/// Infection's `Unwrap*` mutators that keep ONE argument: `f(…, a, …)` → `a`. Maps a
/// (lower-cased) PHP function name to `(Infection mutator name, kept arg index)`. Most
/// keep arg 0; a few keep a later single arg (str_replace/str_ireplace/array_reduce →
/// arg 2). The range-index ones (array_map/array_merge/array_intersect*) are a follow-up.
pub fn unwrap_arg(fn_lower: &[u8]) -> Option<(&'static str, usize)> {
    let name = match fn_lower {
        b"str_replace" => return Some(("UnwrapStrReplace", 2)),
        b"str_ireplace" => return Some(("UnwrapStrIreplace", 2)),
        b"array_reduce" => return Some(("UnwrapArrayReduce", 2)),
        // (array_combine is multi-index in Infection — keeps args 0 AND 1 — so it
        // belongs with the range-index unwraps, a follow-up.)
        b"strtolower" => "UnwrapStrToLower",
        b"strtoupper" => "UnwrapStrToUpper",
        b"trim" => "UnwrapTrim",
        b"ltrim" => "UnwrapLtrim",
        b"rtrim" => "UnwrapRtrim",
        b"ucfirst" => "UnwrapUcFirst",
        b"lcfirst" => "UnwrapLcFirst",
        b"ucwords" => "UnwrapUcWords",
        b"strrev" => "UnwrapStrRev",
        b"str_shuffle" => "UnwrapStrShuffle",
        b"str_repeat" => "UnwrapStrRepeat",
        b"substr" => "UnwrapSubstr",
        b"array_reverse" => "UnwrapArrayReverse",
        b"array_unique" => "UnwrapArrayUnique",
        b"array_values" => "UnwrapArrayValues",
        b"array_keys" => "UnwrapArrayKeys",
        b"array_flip" => "UnwrapArrayFlip",
        b"array_filter" => "UnwrapArrayFilter",
        b"array_change_key_case" => "UnwrapArrayChangeKeyCase",
        b"array_chunk" => "UnwrapArrayChunk",
        b"array_column" => "UnwrapArrayColumn",
        b"array_diff" => "UnwrapArrayDiff",
        b"array_diff_assoc" => "UnwrapArrayDiffAssoc",
        b"array_diff_key" => "UnwrapArrayDiffKey",
        b"array_diff_uassoc" => "UnwrapArrayDiffUassoc",
        b"array_diff_ukey" => "UnwrapArrayDiffUkey",
        b"array_pad" => "UnwrapArrayPad",
        b"array_slice" => "UnwrapArraySlice",
        b"array_splice" => "UnwrapArraySplice",
        b"array_udiff" => "UnwrapArrayUdiff",
        b"array_udiff_assoc" => "UnwrapArrayUdiffAssoc",
        b"array_udiff_uassoc" => "UnwrapArrayUdiffUassoc",
        _ => return None,
    };
    Some((name, 0))
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

    /// True when `mutate_binary(op)` yields a `(replacement, name)` pair.
    fn has(op: &mago_syntax::ast::binary::BinaryOperator, repl: &[u8], name: &str) -> bool {
        mutate_binary(op)
            .into_iter()
            .any(|(_, _, r, n)| r == repl && n == name)
    }

    #[test]
    fn plus_becomes_minus() {
        use mago_syntax::ast::binary::BinaryOperator;
        let v = mutate_binary(&BinaryOperator::Addition(span(10, 11)));
        assert_eq!(v, vec![(10usize, 11usize, b"-".as_slice(), "Plus")]);
    }

    #[test]
    fn comparison_has_boundary_and_negation() {
        use mago_syntax::ast::binary::BinaryOperator as B;
        assert!(has(&B::GreaterThan(span(4, 5)), b">=", "GreaterThan"));
        assert!(has(
            &B::GreaterThan(span(4, 5)),
            b"<=",
            "GreaterThanNegotiation"
        ));
        assert!(has(&B::LessThan(span(4, 5)), b"<=", "LessThan"));
        assert!(has(&B::LessThan(span(4, 5)), b">=", "LessThanNegotiation"));
    }

    #[test]
    fn equality_has_flip_and_tighten() {
        use mago_syntax::ast::binary::BinaryOperator as B;
        assert!(has(&B::Equal(span(0, 2)), b"!=", "Equal"));
        assert!(has(&B::Equal(span(0, 2)), b"===", "EqualIdentical"));
        assert!(has(&B::Identical(span(0, 3)), b"!==", "Identical"));
        assert!(has(&B::Identical(span(0, 3)), b"==", "IdenticalEqual"));
    }

    #[test]
    fn bitwise_swaps() {
        use mago_syntax::ast::binary::BinaryOperator as B;
        assert!(has(&B::BitwiseAnd(span(0, 1)), b"|", "BitwiseAnd"));
        assert!(has(&B::BitwiseOr(span(0, 1)), b"&", "BitwiseOr"));
        assert!(has(&B::BitwiseXor(span(0, 1)), b"&", "BitwiseXor"));
    }

    #[test]
    fn shifts_swap_direction() {
        use mago_syntax::ast::binary::BinaryOperator as B;
        // mago variant LeftShift -> Infection mutator name "ShiftLeft", replacement ">>".
        assert!(has(&B::LeftShift(span(0, 2)), b">>", "ShiftLeft"));
        assert!(has(&B::RightShift(span(0, 2)), b"<<", "ShiftRight"));
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
    fn assignment_operators_swap_arithmetic() {
        use mago_syntax::ast::assignment::AssignmentOperator as A;
        let (start, end, repl, name) = mutate_assignment(&A::Addition(span(0, 2))).unwrap();
        assert_eq!((start, end), (0, 2));
        assert_eq!(repl, b"-=");
        assert_eq!(name, "PlusEqual");
        assert_eq!(
            mutate_assignment(&A::Multiplication(span(0, 2))).unwrap().3,
            "MulEqual"
        );
        assert_eq!(
            mutate_assignment(&A::Exponentiation(span(0, 3))).unwrap().2,
            b"/="
        );
        assert!(mutate_assignment(&A::Assign(span(0, 1))).is_none());
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
    fn increment_operator_becomes_decrement() {
        // The `++` operator mutator is Infection's Arithmetic `Increment`.
        let (start, end, repl, name) = mutate_unary_suffix(span(2, 4), true);
        assert_eq!((start, end), (2, 4));
        assert_eq!(repl, b"--");
        assert_eq!(name, "Increment");
    }

    #[test]
    fn decrement_operator_becomes_increment() {
        let (_, _, repl, name) = mutate_unary_suffix(span(2, 4), false);
        assert_eq!(repl, b"++");
        assert_eq!(name, "Decrement");
    }

    #[test]
    fn exponentiation_becomes_division() {
        use mago_syntax::ast::binary::BinaryOperator;
        assert!(has(
            &BinaryOperator::Exponentiation(span(0, 2)),
            b"/",
            "Exponentiation"
        ));
    }
}
