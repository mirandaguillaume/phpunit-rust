//! Expression interpreter — evaluates pure PHP expressions to concrete PhpValues.
//!
//! # Supported constructs (mago-syntax 0.26 AST)
//!
//! | AST variant                                   | PHP example         | Result              |
//! |-----------------------------------------------|---------------------|---------------------|
//! | `Expression::Literal(Literal::Integer(…))`    | `42`, `0xff`        | `PhpValue::Int`     |
//! | `Expression::Literal(Literal::Float(…))`      | `3.14`              | `PhpValue::Float`   |
//! | `Expression::Literal(Literal::String(…))`     | `'hello'`, `"hi"`   | `PhpValue::String`  |
//! | `Expression::Literal(Literal::True(_))`        | `true`              | `PhpValue::Bool(true)` |
//! | `Expression::Literal(Literal::False(_))`       | `false`             | `PhpValue::Bool(false)` |
//! | `Expression::Literal(Literal::Null(_))`        | `null`              | `PhpValue::Null`    |
//! | `Expression::Parenthesized(p)`                | `(expr)`            | delegates to inner  |
//! | `Expression::UnaryPrefix` + `Negation`        | `-expr`             | negates Int/Float   |
//! | `Expression::UnaryPrefix` + `Plus`            | `+expr`             | identity Int/Float  |
//! | `Expression::UnaryPrefix` + `Not`             | `!expr`             | boolean NOT         |
//! | `Expression::Binary` + `Addition`             | `a + b`             | Int or Float        |
//! | `Expression::Binary` + `Subtraction`          | `a - b`             | Int or Float        |
//! | `Expression::Binary` + `Multiplication`       | `a * b`             | Int or Float        |
//! | `Expression::Binary` + `Division`             | `a / b`             | Int or Float        |
//! | `Expression::Binary` + `Modulo`               | `a % b`             | Int                 |
//! | `Expression::Binary` + `Exponentiation`       | `a ** b`            | Float               |
//! | `Expression::Binary` + `StringConcat`         | `a . b`             | String              |
//! | `Expression::Array`                            | `[k => v, v]`       | PhpValue::Array     |
//! | `Expression::LegacyArray`                     | `array(k => v)`     | PhpValue::Array     |
//!
//! # Not supported (returns `ComputeError::Unsupported`)
//!
//! Variables, closures, function calls, property/constant access, composite strings
//! (interpolated/heredoc), `match`/`switch`/ternary, `instanceof`, and all constructs
//! outside the literal/arithmetic/array subset above.
//!
//! # LiteralString quirk
//!
//! mago-syntax 0.26 stores the resolved string in `LiteralString::value: Option<String>`.
//! When it is `None` (e.g. complex escape sequences) we return `Unsupported`.

use std::collections::BTreeMap;

use mago_syntax::ast::ast::array::{Array, ArrayElement, LegacyArray};
use mago_syntax::ast::ast::binary::BinaryOperator;
use mago_syntax::ast::ast::expression::Expression;
use mago_syntax::ast::ast::literal::Literal;
use mago_syntax::ast::ast::unary::UnaryPrefixOperator;

use super::value::{ArrayKey, PhpValue};

// ─── Error type ─────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ComputeError {
    #[error("unsupported construct: {0}")]
    Unsupported(String),
    #[error("type error: {0}")]
    TypeError(String),
    #[error("depth limit exceeded")]
    DepthExceeded,
}

// ─── Evaluation context ──────────────────────────────────────────────────────

/// Evaluation context — tracks recursion depth to guard against deep/circular inputs.
#[derive(Default)]
pub struct Context {
    pub depth: u32,
    pub max_depth: u32,
}

impl Context {
    pub fn new() -> Self {
        Self {
            depth: 0,
            max_depth: 100,
        }
    }
}

// ─── Public entry point ──────────────────────────────────────────────────────

/// Evaluate a mago-syntax 0.26 `Expression` node to a `PhpValue`.
///
/// Returns `ComputeError::DepthExceeded` when `ctx.depth >= ctx.max_depth`.
/// Returns `ComputeError::Unsupported` for any construct outside the supported subset.
pub fn compute(expr: &Expression, ctx: &mut Context) -> Result<PhpValue, ComputeError> {
    if ctx.depth >= ctx.max_depth {
        return Err(ComputeError::DepthExceeded);
    }
    ctx.depth += 1;
    let result = compute_inner(expr, ctx);
    ctx.depth -= 1;
    result
}

// ─── Core dispatch ───────────────────────────────────────────────────────────

fn compute_inner(expr: &Expression, ctx: &mut Context) -> Result<PhpValue, ComputeError> {
    match expr {
        Expression::Literal(lit) => compute_literal(lit),
        Expression::Parenthesized(p) => compute(p.expression, ctx),
        Expression::UnaryPrefix(u) => compute_unary_prefix(&u.operator, u.operand, ctx),
        Expression::Binary(b) => compute_binary(b.lhs, &b.operator, b.rhs, ctx),
        Expression::Array(arr) => compute_array(arr, ctx),
        Expression::LegacyArray(arr) => compute_legacy_array(arr, ctx),
        other => Err(ComputeError::Unsupported(format!("{}", other))),
    }
}

// ─── Literal evaluation ──────────────────────────────────────────────────────

fn compute_literal(lit: &Literal) -> Result<PhpValue, ComputeError> {
    match lit {
        Literal::Integer(i) => match i.value {
            Some(v) => Ok(PhpValue::Int(v as i64)),
            None => Err(ComputeError::Unsupported(
                "LiteralInteger with unresolved value (overflow)".into(),
            )),
        },
        Literal::Float(f) => Ok(PhpValue::Float(*f.value)),
        Literal::String(s) => match &s.value {
            Some(v) => Ok(PhpValue::String(String::from_utf8_lossy(v).into_owned())),
            None => Err(ComputeError::Unsupported(
                "LiteralString with unresolved value (complex escape sequence)".into(),
            )),
        },
        Literal::True(_) => Ok(PhpValue::Bool(true)),
        Literal::False(_) => Ok(PhpValue::Bool(false)),
        Literal::Null(_) => Ok(PhpValue::Null),
    }
}

// ─── Unary prefix ────────────────────────────────────────────────────────────

fn compute_unary_prefix(
    op: &UnaryPrefixOperator,
    operand: &Expression,
    ctx: &mut Context,
) -> Result<PhpValue, ComputeError> {
    match op {
        UnaryPrefixOperator::Negation(_) => {
            let val = compute(operand, ctx)?;
            match val {
                PhpValue::Int(n) => Ok(PhpValue::Int(n.wrapping_neg())),
                PhpValue::Float(f) => Ok(PhpValue::Float(-f)),
                _ => Err(ComputeError::TypeError(format!(
                    "cannot negate {}",
                    val.type_name()
                ))),
            }
        }
        UnaryPrefixOperator::Plus(_) => {
            let val = compute(operand, ctx)?;
            match &val {
                PhpValue::Int(_) | PhpValue::Float(_) => Ok(val),
                _ => Err(ComputeError::TypeError(format!(
                    "unary + on {}",
                    val.type_name()
                ))),
            }
        }
        UnaryPrefixOperator::Not(_) => {
            let val = compute(operand, ctx)?;
            Ok(PhpValue::Bool(!php_truthy(&val)))
        }
        other => Err(ComputeError::Unsupported(format!(
            "unary prefix operator {:?}",
            other
        ))),
    }
}

// ─── Binary evaluation ───────────────────────────────────────────────────────

fn compute_binary(
    lhs: &Expression,
    op: &BinaryOperator,
    rhs: &Expression,
    ctx: &mut Context,
) -> Result<PhpValue, ComputeError> {
    match op {
        BinaryOperator::Addition(_) => {
            let (l, r) = (compute(lhs, ctx)?, compute(rhs, ctx)?);
            numeric_op(l, r, i64::wrapping_add, |a, b| a + b)
        }
        BinaryOperator::Subtraction(_) => {
            let (l, r) = (compute(lhs, ctx)?, compute(rhs, ctx)?);
            numeric_op(l, r, i64::wrapping_sub, |a, b| a - b)
        }
        BinaryOperator::Multiplication(_) => {
            let (l, r) = (compute(lhs, ctx)?, compute(rhs, ctx)?);
            numeric_op(l, r, i64::wrapping_mul, |a, b| a * b)
        }
        BinaryOperator::Division(_) => {
            let (l, r) = (compute(lhs, ctx)?, compute(rhs, ctx)?);
            match (&l, &r) {
                (PhpValue::Int(a), PhpValue::Int(b)) => {
                    if *b == 0 {
                        return Err(ComputeError::Unsupported("division by zero".into()));
                    }
                    if a % b == 0 {
                        Ok(PhpValue::Int(a / b))
                    } else {
                        Ok(PhpValue::Float(*a as f64 / *b as f64))
                    }
                }
                _ => {
                    let (af, bf) = (coerce_to_float(&l)?, coerce_to_float(&r)?);
                    if bf == 0.0 {
                        return Err(ComputeError::Unsupported("division by zero".into()));
                    }
                    Ok(PhpValue::Float(af / bf))
                }
            }
        }
        BinaryOperator::Modulo(_) => {
            let (l, r) = (compute(lhs, ctx)?, compute(rhs, ctx)?);
            match (&l, &r) {
                (PhpValue::Int(a), PhpValue::Int(b)) => {
                    if *b == 0 {
                        return Err(ComputeError::Unsupported("modulo by zero".into()));
                    }
                    Ok(PhpValue::Int(a % b))
                }
                _ => Err(ComputeError::TypeError(format!(
                    "modulo requires int operands, got {} and {}",
                    l.type_name(),
                    r.type_name()
                ))),
            }
        }
        BinaryOperator::Exponentiation(_) => {
            let (l, r) = (compute(lhs, ctx)?, compute(rhs, ctx)?);
            let (base, exp) = (coerce_to_float(&l)?, coerce_to_float(&r)?);
            Ok(PhpValue::Float(base.powf(exp)))
        }
        BinaryOperator::StringConcat(_) => {
            let (l, r) = (compute(lhs, ctx)?, compute(rhs, ctx)?);
            let ls = coerce_to_string(l)?;
            let rs = coerce_to_string(r)?;
            Ok(PhpValue::String(ls + &rs))
        }
        other => Err(ComputeError::Unsupported(format!(
            "binary operator {:?}",
            other
        ))),
    }
}

// ─── Array construction ──────────────────────────────────────────────────────

fn compute_array(arr: &Array, ctx: &mut Context) -> Result<PhpValue, ComputeError> {
    build_php_array(arr.elements.as_slice(), ctx)
}

fn compute_legacy_array(arr: &LegacyArray, ctx: &mut Context) -> Result<PhpValue, ComputeError> {
    build_php_array(arr.elements.as_slice(), ctx)
}

fn build_php_array(elements: &[ArrayElement], ctx: &mut Context) -> Result<PhpValue, ComputeError> {
    let mut map: BTreeMap<ArrayKey, PhpValue> = BTreeMap::new();
    let mut next_int_key: i64 = 0;

    for element in elements {
        match element {
            ArrayElement::KeyValue(kv) => {
                let k = compute(kv.key, ctx)?;
                let v = compute(kv.value, ctx)?;
                let key = php_value_to_array_key(k)?;
                if let ArrayKey::Int(n) = &key {
                    if *n >= next_int_key {
                        next_int_key = n + 1;
                    }
                }
                map.insert(key, v);
            }
            ArrayElement::Value(ve) => {
                let v = compute(ve.value, ctx)?;
                map.insert(ArrayKey::Int(next_int_key), v);
                next_int_key += 1;
            }
            ArrayElement::Variadic(_) => {
                return Err(ComputeError::Unsupported(
                    "variadic array element (...$x) not supported".into(),
                ));
            }
            ArrayElement::Missing(_) => {
                // PHP allows trailing commas; skip silently.
            }
        }
    }

    Ok(PhpValue::Array(map))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn php_truthy(val: &PhpValue) -> bool {
    match val {
        PhpValue::Null => false,
        PhpValue::Bool(b) => *b,
        PhpValue::Int(n) => *n != 0,
        PhpValue::Float(f) => *f != 0.0,
        PhpValue::String(s) => !s.is_empty() && s != "0",
        PhpValue::Array(a) => !a.is_empty(),
    }
}

fn coerce_to_float(val: &PhpValue) -> Result<f64, ComputeError> {
    match val {
        PhpValue::Int(n) => Ok(*n as f64),
        PhpValue::Float(f) => Ok(*f),
        other => Err(ComputeError::TypeError(format!(
            "expected numeric value, got {}",
            other.type_name()
        ))),
    }
}

fn coerce_to_string(val: PhpValue) -> Result<String, ComputeError> {
    match val {
        PhpValue::String(s) => Ok(s),
        PhpValue::Int(n) => Ok(n.to_string()),
        PhpValue::Float(f) => Ok(format!("{}", f)),
        PhpValue::Bool(true) => Ok("1".into()),
        PhpValue::Bool(false) => Ok(String::new()),
        PhpValue::Null => Ok(String::new()),
        PhpValue::Array(_) => Err(ComputeError::TypeError(
            "cannot coerce array to string".into(),
        )),
    }
}

fn php_value_to_array_key(val: PhpValue) -> Result<ArrayKey, ComputeError> {
    match val {
        PhpValue::Int(n) => Ok(ArrayKey::Int(n)),
        PhpValue::String(s) => {
            // PHP silently coerces numeric strings to integer keys.
            if let Ok(n) = s.parse::<i64>() {
                Ok(ArrayKey::Int(n))
            } else {
                Ok(ArrayKey::String(s))
            }
        }
        PhpValue::Bool(true) => Ok(ArrayKey::Int(1)),
        PhpValue::Bool(false) => Ok(ArrayKey::Int(0)),
        PhpValue::Null => Ok(ArrayKey::String(String::new())),
        PhpValue::Float(f) => Ok(ArrayKey::Int(f as i64)),
        PhpValue::Array(_) => Err(ComputeError::TypeError(
            "cannot use array as array key".into(),
        )),
    }
}

/// Perform a numeric binary operation, promoting to float if either operand is float.
fn numeric_op(
    l: PhpValue,
    r: PhpValue,
    int_op: fn(i64, i64) -> i64,
    float_op: fn(f64, f64) -> f64,
) -> Result<PhpValue, ComputeError> {
    match (&l, &r) {
        (PhpValue::Int(a), PhpValue::Int(b)) => Ok(PhpValue::Int(int_op(*a, *b))),
        _ => {
            let a = coerce_to_float(&l)?;
            let b = coerce_to_float(&r)?;
            Ok(PhpValue::Float(float_op(a, b)))
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bumpalo::Bump;
    use mago_database::file::File;
    use mago_syntax::ast::ast::statement::Statement;
    use mago_syntax::parser::parse_file;

    /// Parse `<?php <snippet>;` and return the first expression statement's
    /// expression. The arena is leaked so the returned reference is `'static`
    /// (test-only; the leak is bounded by the test process lifetime).
    fn parse_expr(snippet: &str) -> &'static Expression<'static> {
        let full = format!("<?php {};", snippet);
        let arena: &'static Bump = Box::leak(Box::new(Bump::new()));
        let file = File::ephemeral(
            std::borrow::Cow::Borrowed(b"test.php".as_slice()),
            std::borrow::Cow::Owned(full.clone().into_bytes()),
        );
        let file: &'static File = Box::leak(Box::new(file));
        let program = parse_file(arena, file);
        for stmt in program.statements.iter() {
            if let Statement::Expression(es) = stmt {
                return es.expression;
            }
        }
        panic!("no ExpressionStatement found in: {}", full);
    }

    #[test]
    fn depth_limit_returns_error() {
        let expr = parse_expr("42");
        let mut ctx = Context {
            depth: 100,
            max_depth: 100,
        };
        assert!(matches!(
            compute(expr, &mut ctx),
            Err(ComputeError::DepthExceeded)
        ));
    }

    #[test]
    fn evaluates_integer_literal() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("42"), &mut ctx).unwrap(),
            PhpValue::Int(42)
        );
    }

    #[test]
    fn evaluates_negative_integer() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("-42"), &mut ctx).unwrap(),
            PhpValue::Int(-42)
        );
    }

    // `3.14` is intentionally a bare literal (we are verifying the string
    // "3.14" parses to the float 3.14), not an approximation of PI.
    #[allow(clippy::approx_constant)]
    #[test]
    fn evaluates_float_literal() {
        let mut ctx = Context::new();
        match compute(parse_expr("3.14"), &mut ctx).unwrap() {
            PhpValue::Float(f) => assert!((f - 3.14).abs() < 1e-10),
            v => panic!("expected Float, got {:?}", v),
        }
    }

    #[test]
    fn evaluates_string_literal() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("'hello'"), &mut ctx).unwrap(),
            PhpValue::String("hello".into())
        );
    }

    #[test]
    fn evaluates_double_quoted_string() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("\"world\""), &mut ctx).unwrap(),
            PhpValue::String("world".into())
        );
    }

    #[test]
    fn evaluates_true_false_null() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("true"), &mut ctx).unwrap(),
            PhpValue::Bool(true)
        );
        assert_eq!(
            compute(parse_expr("false"), &mut ctx).unwrap(),
            PhpValue::Bool(false)
        );
        assert_eq!(
            compute(parse_expr("null"), &mut ctx).unwrap(),
            PhpValue::Null
        );
    }

    #[test]
    fn evaluates_int_addition() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("1 + 2"), &mut ctx).unwrap(),
            PhpValue::Int(3)
        );
    }

    #[test]
    fn evaluates_int_subtraction() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("10 - 3"), &mut ctx).unwrap(),
            PhpValue::Int(7)
        );
    }

    #[test]
    fn evaluates_int_multiplication() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("6 * 7"), &mut ctx).unwrap(),
            PhpValue::Int(42)
        );
    }

    #[test]
    fn evaluates_int_division_exact() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("10 / 2"), &mut ctx).unwrap(),
            PhpValue::Int(5)
        );
    }

    #[test]
    fn evaluates_int_division_to_float() {
        let mut ctx = Context::new();
        match compute(parse_expr("7 / 2"), &mut ctx).unwrap() {
            PhpValue::Float(f) => assert!((f - 3.5).abs() < 1e-10),
            v => panic!("expected Float, got {:?}", v),
        }
    }

    #[test]
    fn evaluates_string_concat() {
        let mut ctx = Context::new();
        assert_eq!(
            compute(parse_expr("'hello' . ' world'"), &mut ctx).unwrap(),
            PhpValue::String("hello world".into())
        );
    }

    #[test]
    fn evaluates_array_literal() {
        let mut ctx = Context::new();
        match compute(parse_expr("[1, 2, 3]"), &mut ctx).unwrap() {
            PhpValue::Array(map) => {
                assert_eq!(map.get(&ArrayKey::Int(0)), Some(&PhpValue::Int(1)));
                assert_eq!(map.get(&ArrayKey::Int(1)), Some(&PhpValue::Int(2)));
                assert_eq!(map.get(&ArrayKey::Int(2)), Some(&PhpValue::Int(3)));
            }
            v => panic!("expected Array, got {:?}", v),
        }
    }

    #[test]
    fn evaluates_array_with_string_keys() {
        let mut ctx = Context::new();
        match compute(parse_expr("['a' => 1, 'b' => 2]"), &mut ctx).unwrap() {
            PhpValue::Array(map) => {
                assert_eq!(
                    map.get(&ArrayKey::String("a".into())),
                    Some(&PhpValue::Int(1))
                );
                assert_eq!(
                    map.get(&ArrayKey::String("b".into())),
                    Some(&PhpValue::Int(2))
                );
            }
            v => panic!("expected Array, got {:?}", v),
        }
    }

    #[test]
    fn unsupported_variable_returns_error() {
        let mut ctx = Context::new();
        assert!(matches!(
            compute(parse_expr("$x"), &mut ctx),
            Err(ComputeError::Unsupported(_))
        ));
    }
}
