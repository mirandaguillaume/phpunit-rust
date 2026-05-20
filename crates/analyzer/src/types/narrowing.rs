//! Narrowing facts produced when walking conditional expressions.
//!
//! When the walker encounters `$x instanceof Foo`, it collects a Narrowing
//! { var: "$x", ty: Class("Foo") }. The walker then applies these to a forked
//! environment when entering the conditional's true branch. After the branch,
//! the fork is dropped and the original env restored.

use super::type_repr::Type;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Narrowing {
    pub var: String,
    pub ty: Type,
}

#[derive(Debug, Clone, Default)]
pub struct NarrowingSet {
    pub facts: Vec<Narrowing>,
}

impl NarrowingSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, var: String, ty: Type) {
        self.facts.push(Narrowing { var, ty });
    }

    pub fn extend(&mut self, other: NarrowingSet) {
        self.facts.extend(other.facts);
    }

    pub fn as_tuples(&self) -> Vec<(String, Type)> {
        self.facts.iter().map(|n| (n.var.clone(), n.ty.clone())).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_extend() {
        let mut a = NarrowingSet::new();
        a.push("$x".into(), Type::Class("Foo".into()));
        let mut b = NarrowingSet::new();
        b.push("$y".into(), Type::Class("Bar".into()));
        a.extend(b);
        assert_eq!(a.facts.len(), 2);
        assert_eq!(a.facts[0].var, "$x");
        assert_eq!(a.facts[1].var, "$y");
    }
}
