//! Pure builtin function models for the concrete interpreter.
//!
//! Each entry is a small reimplementation of a PHP standard-library function
//! that has no side effects and pure inputs/outputs. The interpreter consults
//! this table when it encounters a function call during expression evaluation.
//!
//! Unmodeled builtins return `ComputeError::Unsupported` so the caller can
//! degrade to opaque handling.

use super::expr::ComputeError;
use super::value::PhpValue;

/// Dispatch a call to a pure builtin function by name.
///
/// Returns `Unsupported` for any function not in the table — the caller
/// should treat the surrounding expression as opaque.
pub fn call_builtin(name: &str, args: &[PhpValue]) -> Result<PhpValue, ComputeError> {
    match (name, args) {
        ("strlen", [PhpValue::String(s)]) => Ok(PhpValue::Int(s.len() as i64)),
        // PHP's strtolower/strtoupper are ASCII-only (mb_* is the Unicode variant).
        // Rust's to_lowercase()/to_uppercase() would do Unicode folding (e.g. ß→ss);
        // use the ASCII variants for byte-exact PHP semantics.
        ("strtolower", [PhpValue::String(s)]) => Ok(PhpValue::String(s.to_ascii_lowercase())),
        ("strtoupper", [PhpValue::String(s)]) => Ok(PhpValue::String(s.to_ascii_uppercase())),
        // PHP's trim() default character set: " \t\n\r\0\x0B" — not Rust's full
        // Unicode whitespace. Match PHP exactly.
        ("trim", [PhpValue::String(s)]) => Ok(PhpValue::String(
            s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\0' | '\x0B'))
                .to_string(),
        )),
        (
            "str_replace",
            [PhpValue::String(search), PhpValue::String(replace), PhpValue::String(subject)],
        ) => Ok(PhpValue::String(subject.replace(search.as_str(), replace))),
        ("count", [PhpValue::Array(a)]) => Ok(PhpValue::Int(a.len() as i64)),
        ("is_array", [v]) => Ok(PhpValue::Bool(matches!(v, PhpValue::Array(_)))),
        ("is_string", [v]) => Ok(PhpValue::Bool(matches!(v, PhpValue::String(_)))),
        ("is_int", [v]) => Ok(PhpValue::Bool(matches!(v, PhpValue::Int(_)))),
        (n, _) => Err(ComputeError::Unsupported(format!("builtin: {n}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strlen_works() {
        assert_eq!(
            call_builtin("strlen", &[PhpValue::String("hello".into())]).unwrap(),
            PhpValue::Int(5)
        );
    }

    #[test]
    fn strtolower_strtoupper_work() {
        assert_eq!(
            call_builtin("strtolower", &[PhpValue::String("ABC".into())]).unwrap(),
            PhpValue::String("abc".into())
        );
        assert_eq!(
            call_builtin("strtoupper", &[PhpValue::String("abc".into())]).unwrap(),
            PhpValue::String("ABC".into())
        );
    }

    #[test]
    fn trim_works() {
        assert_eq!(
            call_builtin("trim", &[PhpValue::String("  hi  ".into())]).unwrap(),
            PhpValue::String("hi".into())
        );
    }

    #[test]
    fn str_replace_works() {
        let r = call_builtin(
            "str_replace",
            &[
                PhpValue::String("o".into()),
                PhpValue::String("0".into()),
                PhpValue::String("hello".into()),
            ],
        );
        assert_eq!(r.unwrap(), PhpValue::String("hell0".into()));
    }

    #[test]
    fn count_array_returns_length() {
        let mut arr = std::collections::BTreeMap::new();
        arr.insert(super::super::value::ArrayKey::Int(0), PhpValue::Int(1));
        arr.insert(super::super::value::ArrayKey::Int(1), PhpValue::Int(2));
        let r = call_builtin("count", &[PhpValue::Array(arr)]);
        assert_eq!(r.unwrap(), PhpValue::Int(2));
    }

    #[test]
    fn is_predicates_match_type() {
        assert_eq!(
            call_builtin("is_string", &[PhpValue::String("x".into())]).unwrap(),
            PhpValue::Bool(true)
        );
        assert_eq!(
            call_builtin("is_string", &[PhpValue::Int(1)]).unwrap(),
            PhpValue::Bool(false)
        );
        assert_eq!(
            call_builtin("is_int", &[PhpValue::Int(1)]).unwrap(),
            PhpValue::Bool(true)
        );
        assert_eq!(
            call_builtin("is_int", &[PhpValue::String("1".into())]).unwrap(),
            PhpValue::Bool(false)
        );
        assert_eq!(
            call_builtin(
                "is_array",
                &[PhpValue::Array(std::collections::BTreeMap::new())]
            )
            .unwrap(),
            PhpValue::Bool(true)
        );
    }

    #[test]
    fn unknown_builtin_returns_unsupported() {
        let r = call_builtin("file_get_contents", &[PhpValue::String("x.txt".into())]);
        assert!(matches!(r, Err(ComputeError::Unsupported(_))));
    }

    #[test]
    fn type_mismatch_returns_unsupported() {
        // strlen on an int — no match arm fires, falls through to Unsupported.
        let r = call_builtin("strlen", &[PhpValue::Int(42)]);
        assert!(matches!(r, Err(ComputeError::Unsupported(_))));
    }
}
