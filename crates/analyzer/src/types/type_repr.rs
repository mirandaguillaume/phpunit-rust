//! Type representation for the type tracker.

/// A PHP type, as inferred by the lexical type tracker.
///
/// Phase 2 scope: explicit annotations, instanceof narrowing, 2-way unions.
/// Generics, intersection types, and unions of 3+ variants land in Phase 3.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    /// Concrete class from `new ClassName(...)` or a declared property type.
    Class(String),
    /// Interface type — no known concrete impl. Triggers opaque dispatch.
    Interface(String),
    /// Produced by `$this->createMock(X::class)`. Wraps the mocked FQCN.
    Mock(String),
    /// `?T` — nullable wrapper around an inner type.
    Nullable(Box<Type>),
    /// `A|B` — exactly two variants in Phase 2.
    Union(Box<Type>, Box<Type>),
    /// `$this` — resolved via the env's enclosing_class.
    This,
    /// `self::` — resolved at parse time.
    SelfRef(String),
    /// `static::` — resolved best-effort; distinct from Self.
    StaticRef(String),
    /// Unknown / unmodeled. Triggers opaque dispatch.
    Mixed,
}

impl Type {
    /// Returns true if this type cannot resolve to a concrete dispatch target.
    pub fn is_opaque(&self) -> bool {
        matches!(self, Type::Mixed | Type::Interface(_) | Type::Mock(_))
    }

    /// Unwrap one layer of nullability: `Nullable(T) -> T`. Other types unchanged.
    pub fn non_nullable(&self) -> Type {
        match self {
            Type::Nullable(inner) => (**inner).clone(),
            other => other.clone(),
        }
    }

    /// Concrete-class FQCN if known, else None. `SelfRef`/`StaticRef` are
    /// treated as concrete here.
    pub fn concrete_class_fqcn(&self) -> Option<&str> {
        match self {
            Type::Class(c) | Type::SelfRef(c) | Type::StaticRef(c) => Some(c.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_opaque_matches_only_mixed_interface_mock() {
        assert!(Type::Mixed.is_opaque());
        assert!(Type::Interface("Foo".into()).is_opaque());
        assert!(Type::Mock("Foo".into()).is_opaque());
        assert!(!Type::Class("Foo".into()).is_opaque());
        assert!(!Type::This.is_opaque());
        assert!(!Type::SelfRef("Foo".into()).is_opaque());
        assert!(!Type::Nullable(Box::new(Type::Class("Foo".into()))).is_opaque());
    }

    #[test]
    fn non_nullable_unwraps_one_layer() {
        let inner = Type::Class("Foo".into());
        let nullable = Type::Nullable(Box::new(inner.clone()));
        assert_eq!(nullable.non_nullable(), inner);
        assert_eq!(Type::Class("Foo".into()).non_nullable(), Type::Class("Foo".into()));
    }

    #[test]
    fn concrete_class_fqcn_returns_for_class_self_static() {
        assert_eq!(Type::Class("Foo".into()).concrete_class_fqcn(), Some("Foo"));
        assert_eq!(Type::SelfRef("Bar".into()).concrete_class_fqcn(), Some("Bar"));
        assert_eq!(Type::StaticRef("Baz".into()).concrete_class_fqcn(), Some("Baz"));
        assert_eq!(Type::This.concrete_class_fqcn(), None);
        assert_eq!(Type::Interface("X".into()).concrete_class_fqcn(), None);
        assert_eq!(Type::Mixed.concrete_class_fqcn(), None);
    }
}
