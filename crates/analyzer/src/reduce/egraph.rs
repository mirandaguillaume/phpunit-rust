//! E-graph congruence-closure reduction engine (increment 1).
//!
//! # The model
//!
//! A PHPUnit test is a TERM — a tree of operations over its Givens (value objects,
//! arithmetic, method calls). A suite is a FOREST of such terms. We insert them all
//! into ONE e-graph, apply EQUATIONS (oriented method bodies + algebraic laws +
//! ground evaluation of known leaves) until a fixed point (equality saturation),
//! then DECIDE an assertion BY CONGRUENCE: `assertSame(L, R)` holds iff
//! `egraph.find(L) == egraph.find(R)` — L and R sit in the same e-class. No
//! execution happens.
//!
//! SHARING is automatic: an identical subterm is a single e-class (hash-consing =
//! minimisation of the tree automaton). A `Money(8)` that is PRODUCED by reducing
//! `(Money (+ 5 3))` and a `Money(8)` that is CONSTRUCTED directly are the SAME
//! e-class — computed once.
//!
//! This increment proves the mechanics end-to-end on a hard-coded mini value-object
//! fragment. AST extraction from mago lands later. It does NOT touch `term.rs` /
//! `bridge_term.rs`.
//!
//! # The mini fragment (value object `Money`)
//!
//! Equations E = { getamount-money, plus-money, comm-add, ground-fold }:
//! - `getAmount(Money a)            => a`            (accessor body)
//! - `plus(Money a, b)             => Money(a + b)`  (method body, oriented)
//! - `a + b                        => b + a`         (commutativity law)
//! - ground fold: `Int n + Int m   => Int (n+m)`     (constant evaluation, e-class analysis)
//!
//! So `assertSame(8, (new Money(5))->plus(3)->getAmount())` is decided TRUE purely
//! by congruence: the RHS reduces, via the equations, into the same e-class as the
//! literal `8`.
//!
//! # Fail-closed totality
//!
//! Ground folding is TOTAL via `checked_add`: on i64 overflow it yields `None`, i.e.
//! NO folded constant. PHP would promote the overflowing sum to float — a behaviour
//! this fragment does not model — so we decline to fold rather than guess. An
//! overflowing term therefore never becomes congruent to a concrete literal: the
//! engine never decides a term it cannot soundly evaluate.

use egg::{define_language, rewrite, Analysis, DidMerge, EGraph, Id, Rewrite, Runner};

define_language! {
    /// The mini value-object term language. Operators are matched by their leading
    /// string token in S-expression rewrite patterns.
    pub enum Php {
        Int(i64),
        "+" = Add([Id; 2]),
        "Money" = Money([Id; 1]),
        "plus" = Plus([Id; 2]),        // plus(money, int) — the method to reduce
        "getAmount" = GetAmount([Id; 1]),
    }
}

/// E-class analysis: ground evaluation (constant folding), fail-closed on overflow.
///
/// `Data` is the constant value of an e-class when it is decidably a concrete
/// integer, else `None`. Modelled after egg's `ConstantFold` tutorial analysis.
#[derive(Default)]
pub struct GroundEval;

impl Analysis<Php> for GroundEval {
    /// The known constant value of an e-class, if decidable.
    type Data = Option<i64>;

    fn make(egraph: &mut EGraph<Php, Self>, enode: &Php, _id: Id) -> Self::Data {
        let v = |i: &Id| egraph[*i].data;
        match enode {
            Php::Int(n) => Some(*n),
            // `checked_add` → `None` on overflow = no fold (totality, fail-closed:
            // PHP promotes to float, which this fragment does not model).
            Php::Add([a, b]) => Some(v(a)?.checked_add(v(b)?)?),
            _ => None,
        }
    }

    fn merge(&mut self, a: &mut Self::Data, b: Self::Data) -> DidMerge {
        egg::merge_option(a, b, |x, y| {
            // Two concrete constants in one e-class must agree — soundness invariant.
            debug_assert_eq!(*x, y);
            DidMerge(false, false)
        })
    }

    fn modify(egraph: &mut EGraph<Php, Self>, id: Id) {
        // When an e-class is known-constant, materialise that `Int` node and union
        // it in, so the literal and the computed term share one e-class.
        if let Some(c) = egraph[id].data {
            let n = egraph.add(Php::Int(c));
            egraph.union(id, n);
        }
    }
}

/// The equations, as oriented rewrite rules.
fn rules() -> Vec<Rewrite<Php, GroundEval>> {
    vec![
        rewrite!("getamount-money"; "(getAmount (Money ?a))" => "?a"),
        rewrite!("plus-money";      "(plus (Money ?a) ?b)"   => "(Money (+ ?a ?b))"),
        rewrite!("comm-add";        "(+ ?a ?b)"              => "(+ ?b ?a)"),
    ]
}

/// Insert each S-expression into ONE shared e-graph, saturate with the equations,
/// and return the saturated graph plus the canonical (post-`find`) `Id` of each
/// input expression, in order.
pub fn build_and_saturate(exprs: &[&str]) -> (EGraph<Php, GroundEval>, Vec<Id>) {
    let mut egraph: EGraph<Php, GroundEval> = EGraph::default();
    let roots: Vec<Id> = exprs
        .iter()
        .map(|s| {
            let expr = s.parse().expect("S-expression must parse as Php term");
            egraph.add_expr(&expr)
        })
        .collect();

    let runner = Runner::default().with_egraph(egraph).run(&rules());
    let egraph = runner.egraph;

    // Canonicalise the recorded roots in the saturated graph.
    let roots = roots.iter().map(|id| egraph.find(*id)).collect();
    (egraph, roots)
}

/// Decide congruence: are the two ids in the same e-class of the saturated graph?
pub fn congruent(graph: &EGraph<Php, GroundEval>, id_l: Id, id_r: Id) -> bool {
    graph.find(id_l) == graph.find(id_r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T1: `assertSame(8, (new Money(5))->plus(3)->getAmount())` decided TRUE by
    /// congruence. LHS `8` and RHS `(getAmount (plus (Money 5) 3))` land in one
    /// e-class after saturation.
    #[test]
    fn t1_congruent() {
        let (graph, roots) = build_and_saturate(&["8", "(getAmount (plus (Money 5) 3))"]);
        assert!(
            congruent(&graph, roots[0], roots[1]),
            "8 and (getAmount (plus (Money 5) 3)) must be congruent"
        );
    }

    /// T2: `8` and `(getAmount (Money 8))` land in one e-class (accessor body only).
    #[test]
    fn t2_congruent() {
        let (graph, roots) = build_and_saturate(&["8", "(getAmount (Money 8))"]);
        assert!(
            congruent(&graph, roots[0], roots[1]),
            "8 and (getAmount (Money 8)) must be congruent"
        );
    }

    /// The cache proof: in ONE saturated graph, the `Money(8)` PRODUCED by reducing
    /// T1's `(Money (+ 5 3))` and the `Money(8)` CONSTRUCTED directly are the SAME
    /// e-class. The shared subterm is computed once = minimisation.
    #[test]
    fn partage_money8() {
        let (graph, roots) = build_and_saturate(&[
            "(getAmount (plus (Money 5) 3))", // T1 RHS — reduces through (Money (+ 5 3))
            "(getAmount (Money 8))",          // T2 RHS
            "(Money (+ 5 3))",                // money8_t1: the PRODUCED Money
            "(Money 8)",                      // money8_t2: the CONSTRUCTED Money
        ]);
        let money8_t1 = roots[2];
        let money8_t2 = roots[3];
        assert_eq!(
            graph.find(money8_t1),
            graph.find(money8_t2),
            "the produced Money(8) and the constructed Money(8) must share one e-class"
        );
        assert!(congruent(&graph, money8_t1, money8_t2));
    }

    /// The soundness proof: congruence is NOT abusive fusion. `7` is NOT congruent
    /// to the RHS that reduces to `8`; nor is `9`. The engine never declares
    /// `7 === 8` or `9 === 8`.
    #[test]
    fn non_fusion_sound() {
        let (graph, roots) = build_and_saturate(&["7", "(getAmount (plus (Money 5) 3))", "9"]);
        assert!(
            !congruent(&graph, roots[0], roots[1]),
            "7 must NOT be congruent to a term that reduces to 8"
        );
        assert!(
            !congruent(&graph, roots[2], roots[1]),
            "9 must NOT be congruent to a term that reduces to 8"
        );
    }

    /// Fail-closed totality: `(+ i64::MAX 1)` overflows, so its e-class acquires NO
    /// folded constant (`data == None`). Hence `(getAmount (Money (+ MAX 1)))` is
    /// not congruent to any concrete literal: the engine declines to decide a term
    /// it cannot soundly evaluate.
    #[test]
    fn overflow_pas_de_fold() {
        let max = i64::MAX;
        let add_expr = format!("(+ {max} 1)");
        let (graph, roots) = build_and_saturate(&[&add_expr]);
        let add_id = roots[0];
        assert_eq!(
            graph[add_id].data, None,
            "(+ i64::MAX 1) must NOT acquire a folded constant (overflow is fail-closed)"
        );

        // And the surrounding term cannot become congruent to a concrete literal.
        let lit = format!("{max}");
        let (graph2, roots2) =
            build_and_saturate(&[&format!("(getAmount (Money (+ {max} 1)))"), &lit]);
        assert!(
            !congruent(&graph2, roots2[0], roots2[1]),
            "an overflowing term must not be decided congruent to a concrete literal"
        );
    }
}
