//! Type → ReceiverType bridge for the opacity layer.

use crate::opacity::ReceiverType;
use super::type_repr::Type;

/// Convert an inferred Type into the ReceiverType the opacity rules consume.
///
/// Mapping:
///   Class(F) → Concrete(F)
///   Interface(F) → Interface(F)
///   Mock(F) → Mock
///   Nullable(inner) → recurse on inner (nullable doesn't affect opacity decision)
///   Union(a, b) → if both map to same ReceiverType, return that; otherwise Mixed
///   This → resolve via enclosing_class param
///   SelfRef(F) / StaticRef(F) → Concrete(F)
///   Mixed → Mixed
pub fn type_to_receiver_type(ty: &Type, enclosing_class: Option<&str>) -> ReceiverType {
    match ty {
        Type::Class(c) => ReceiverType::Concrete(c.clone()),
        Type::Interface(c) => ReceiverType::Interface(c.clone()),
        Type::Mock(_) => ReceiverType::Mock,
        Type::Nullable(inner) => type_to_receiver_type(inner, enclosing_class),
        Type::Union(a, b) => {
            let ra = type_to_receiver_type(a, enclosing_class);
            let rb = type_to_receiver_type(b, enclosing_class);
            if ra == rb { ra } else { ReceiverType::Mixed }
        }
        Type::This => {
            enclosing_class
                .map(|c| ReceiverType::Concrete(c.to_string()))
                .unwrap_or(ReceiverType::Mixed)
        }
        Type::SelfRef(c) | Type::StaticRef(c) => ReceiverType::Concrete(c.clone()),
        Type::Mixed => ReceiverType::Mixed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_to_concrete() {
        assert_eq!(
            type_to_receiver_type(&Type::Class("Foo".into()), None),
            ReceiverType::Concrete("Foo".into())
        );
    }

    #[test]
    fn interface_stays_interface() {
        assert_eq!(
            type_to_receiver_type(&Type::Interface("Bar".into()), None),
            ReceiverType::Interface("Bar".into())
        );
    }

    #[test]
    fn mock_becomes_mock() {
        assert_eq!(
            type_to_receiver_type(&Type::Mock("Repo".into()), None),
            ReceiverType::Mock
        );
    }

    #[test]
    fn this_resolves_via_enclosing_class() {
        assert_eq!(
            type_to_receiver_type(&Type::This, Some("MyService")),
            ReceiverType::Concrete("MyService".into())
        );
        assert_eq!(
            type_to_receiver_type(&Type::This, None),
            ReceiverType::Mixed
        );
    }

    #[test]
    fn union_of_different_concretes_is_mixed() {
        let u = Type::Union(
            Box::new(Type::Class("A".into())),
            Box::new(Type::Class("B".into())),
        );
        assert_eq!(type_to_receiver_type(&u, None), ReceiverType::Mixed);
    }

    #[test]
    fn union_of_same_class_collapses() {
        let u = Type::Union(
            Box::new(Type::Class("A".into())),
            Box::new(Type::Class("A".into())),
        );
        assert_eq!(type_to_receiver_type(&u, None), ReceiverType::Concrete("A".into()));
    }

    #[test]
    fn nullable_resolves_via_inner() {
        let n = Type::Nullable(Box::new(Type::Class("Foo".into())));
        assert_eq!(type_to_receiver_type(&n, None), ReceiverType::Concrete("Foo".into()));
    }

    #[test]
    fn self_and_static_resolve_to_concrete() {
        assert_eq!(
            type_to_receiver_type(&Type::SelfRef("Bar".into()), None),
            ReceiverType::Concrete("Bar".into())
        );
        assert_eq!(
            type_to_receiver_type(&Type::StaticRef("Baz".into()), None),
            ReceiverType::Concrete("Baz".into())
        );
    }
}
