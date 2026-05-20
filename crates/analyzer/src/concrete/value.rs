use std::collections::BTreeMap;

/// A concrete PHP value, produced by the concrete interpreter.
///
/// Supports the subset of PHP values needed for evaluating data providers,
/// pure helper calls, and value-object constructors: scalars, strings,
/// and key→value arrays. PHP objects, resources, and closures are not modeled.
#[derive(Debug, Clone, PartialEq)]
pub enum PhpValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(BTreeMap<ArrayKey, PhpValue>),
}

/// Array key — PHP supports int and string keys.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArrayKey {
    Int(i64),
    String(String),
}

impl PhpValue {
    /// Return PHP's name for the value's type (matches `gettype()`).
    pub fn type_name(&self) -> &'static str {
        match self {
            PhpValue::Null => "null",
            PhpValue::Bool(_) => "bool",
            PhpValue::Int(_) => "int",
            PhpValue::Float(_) => "float",
            PhpValue::String(_) => "string",
            PhpValue::Array(_) => "array",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(PhpValue::Null.type_name(), "null");
        assert_eq!(PhpValue::Bool(true).type_name(), "bool");
        assert_eq!(PhpValue::Int(1).type_name(), "int");
        assert_eq!(PhpValue::Float(1.5).type_name(), "float");
        assert_eq!(PhpValue::String("x".into()).type_name(), "string");
        assert_eq!(PhpValue::Array(BTreeMap::new()).type_name(), "array");
    }

    #[test]
    fn array_key_ordering_int_lt_string() {
        // Int comes before String in the enum, so BTreeMap will sort ints before strings.
        let int_key = ArrayKey::Int(1);
        let string_key = ArrayKey::String("a".into());
        assert!(int_key < string_key);
    }
}
