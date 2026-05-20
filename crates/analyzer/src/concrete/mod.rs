//! Concrete interpretation of pure PHP expressions.
//!
//! This module provides a small interpreter that can evaluate a subset of PHP
//! expressions to concrete `PhpValue` outputs at analysis time — used for things
//! like static data providers. Constructs outside the supported subset return
//! `ComputeError::Unsupported`, and callers must treat the input as opaque.

pub mod value;
pub mod expr;
pub mod builtins;

pub use value::{PhpValue, ArrayKey};
pub use expr::{compute, Context, ComputeError};
