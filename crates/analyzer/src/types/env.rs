//! Variable → Type environment for the type tracker.

use super::type_repr::Type;
use std::collections::HashMap;

/// A type environment: variable names → types, plus optional enclosing class.
#[derive(Debug, Clone, Default)]
pub struct TypeEnv {
    bindings: HashMap<String, Type>,
    enclosing_class: Option<String>,
}

impl TypeEnv {
    /// Create an empty env. `enclosing_class` is None until set explicitly.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an env scoped to a class (used when analyzing methods).
    /// Pre-binds `$this` to `Type::This` and sets `enclosing_class`.
    pub fn for_class(fqcn: impl Into<String>) -> Self {
        let mut env = Self::new();
        env.enclosing_class = Some(fqcn.into());
        env.set("$this".to_string(), Type::This);
        env
    }

    /// Look up a variable. Unknown vars return `Type::Mixed`.
    pub fn lookup(&self, var: &str) -> Type {
        self.bindings.get(var).cloned().unwrap_or(Type::Mixed)
    }

    /// Set a variable's type, overwriting any previous binding.
    pub fn set(&mut self, var: String, ty: Type) {
        self.bindings.insert(var, ty);
    }

    /// Return the enclosing class FQCN for resolving `$this`, `self::`, `static::`.
    pub fn enclosing_class(&self) -> Option<&str> {
        self.enclosing_class.as_deref()
    }

    /// Fork: produce a clone for use inside a branch. Discard after the branch.
    pub fn fork(&self) -> Self {
        self.clone()
    }

    /// Apply a list of (var, narrowed_type) facts. Used after fork() in branches.
    pub fn apply_narrowing(&mut self, narrowings: &[(String, Type)]) {
        for (var, ty) in narrowings {
            self.set(var.clone(), ty.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_var_is_mixed() {
        let env = TypeEnv::new();
        assert_eq!(env.lookup("$x"), Type::Mixed);
    }

    #[test]
    fn set_and_lookup_roundtrip() {
        let mut env = TypeEnv::new();
        env.set("$x".into(), Type::Class("Foo".into()));
        assert_eq!(env.lookup("$x"), Type::Class("Foo".into()));
    }

    #[test]
    fn for_class_sets_this_and_enclosing() {
        let env = TypeEnv::for_class("MyClass");
        assert_eq!(env.enclosing_class(), Some("MyClass"));
        assert_eq!(env.lookup("$this"), Type::This);
    }

    #[test]
    fn fork_isolates_changes() {
        let mut env = TypeEnv::new();
        env.set("$x".into(), Type::Class("A".into()));
        let mut forked = env.fork();
        forked.set("$x".into(), Type::Class("B".into()));
        forked.set("$y".into(), Type::Class("C".into()));
        // Original unchanged.
        assert_eq!(env.lookup("$x"), Type::Class("A".into()));
        assert_eq!(env.lookup("$y"), Type::Mixed);
        // Fork has new bindings.
        assert_eq!(forked.lookup("$x"), Type::Class("B".into()));
        assert_eq!(forked.lookup("$y"), Type::Class("C".into()));
    }

    #[test]
    fn apply_narrowing_overwrites_bindings() {
        let mut env = TypeEnv::new();
        env.set(
            "$x".into(),
            Type::Union(
                Box::new(Type::Class("A".into())),
                Box::new(Type::Class("B".into())),
            ),
        );
        env.apply_narrowing(&[("$x".into(), Type::Class("A".into()))]);
        assert_eq!(env.lookup("$x"), Type::Class("A".into()));
    }
}
