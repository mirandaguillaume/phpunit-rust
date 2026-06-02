//! Type tracking for forward dataflow analysis of PHP method bodies.
//!
//! The tracker propagates explicit type hints (params, returns, properties,
//! `new ClassName(...)`, `$this`, `self::`, `static::`, method chains,
//! instanceof narrowing, 2-way unions) through method bodies, enabling
//! dispatch resolution at call sites. Variables and expressions whose types
//! we can't track end up as `Type::Mixed` and downstream layers treat them
//! as opaque.

pub mod env;
pub mod narrowing;
pub mod resolver;
pub mod type_repr;
pub mod walker;

pub use env::TypeEnv;
pub use resolver::type_to_receiver_type;
pub use type_repr::Type;
