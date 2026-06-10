//! Symbolic term reduction — the "réduction par substitution" core (v1 prototype).
//!
//! Carries a test's **Givens** (params / provider rows / constructor args) as FREE
//! symbols and reduces expressions — INCLUDING through object construction and pure
//! accessor/transform bodies — to a normal form, then decides an assertion by
//! STRUCTURAL identity / algebra over the symbols. Unlike the value-interpreter
//! ([`super::eval`]), which needs concrete Givens and bails on free variables, this
//! keeps the Givens symbolic, so a test true *for all inputs* decides with no values
//! at all (e.g. `assertSame($a + $b, (new Money($a))->plus($b)->getAmount())` → the
//! object is transparent: its state is a symbolic function of the Givens, both sides
//! reduce to `a + b`, decided `true` ∀ a, b).
//!
//! This module is the pure reduction kernel. Building [`Term`]s from a mago AST
//! (reading ctor + pure method bodies, substituting `$this` state) is the NEXT
//! increment; here the kernel is proven on hand-built terms.
//!
//! Fail-closed: anything the kernel cannot decide returns [`Decision::Unknown`] and
//! the runner executes the test for real. Soundness traps (object `===` identity vs
//! value equality) are handled conservatively — see [`decide_same`].

/// A binary operator. `Add`/`Mul` are commutative+associative (canonicalised);
/// `Sub` is neither (folded only when both operands are concrete).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Op {
    Add,
    Mul,
    Sub,
}

/// A symbolic term. Givens are [`Term::Sym`]; an object is a transparent
/// [`Term::Obj`] (class + ordered fields), each field itself a term over the Givens.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Term {
    /// A free Given (a test parameter / unknown input), keyed by name.
    Sym(String),
    Int(i64),
    Bool(bool),
    Str(String),
    /// Arithmetic over sub-terms.
    Bin(Op, Box<Term>, Box<Term>),
    /// A constructed object: class name + ordered `(field, term)` (last write wins).
    Obj(String, Vec<(String, Term)>),
    /// A property access `term->field` (reduces by substitution when `term` is an `Obj`).
    Field(Box<Term>, String),
    /// An array literal (for count-by-structure).
    List(Vec<Term>),
    /// `count(term)` — reduces to an `Int` when `term` is a `List`.
    Len(Box<Term>),
}

/// The verdict of deciding an assertion symbolically. `Unknown` is fail-closed: the
/// kernel could not prove the outcome, so the test must run for real.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    True,
    False,
    Unknown,
}

/// Reduce a term to its normal form: substitute object fields through accesses,
/// fold concrete arithmetic, and canonicalise commutative chains so `a + b` and
/// `b + a` share one normal form (the basis for the cross-test term-fragment cache).
pub fn reduce(t: &Term) -> Term {
    match t {
        Term::Sym(_) | Term::Int(_) | Term::Bool(_) | Term::Str(_) => t.clone(),

        // Object transparency: `(new C{f: g})->f` substitutes to `g` (factoring the
        // Given through the object). A field access on a non-object stays symbolic.
        Term::Field(obj, name) => {
            let robj = reduce(obj);
            if let Term::Obj(_, fields) = &robj {
                if let Some((_, value)) = fields.iter().rev().find(|(k, _)| k == name) {
                    return reduce(value);
                }
            }
            Term::Field(Box::new(robj), name.clone())
        }

        Term::Obj(class, fields) => Term::Obj(
            class.clone(),
            fields.iter().map(|(k, v)| (k.clone(), reduce(v))).collect(),
        ),

        Term::List(items) => Term::List(items.iter().map(reduce).collect()),

        Term::Len(inner) => {
            let rinner = reduce(inner);
            match &rinner {
                Term::List(items) => Term::Int(items.len() as i64),
                _ => Term::Len(Box::new(rinner)),
            }
        }

        Term::Bin(op, a, b) => {
            let ra = reduce(a);
            let rb = reduce(b);
            match op {
                Op::Add => reduce_commutative(Op::Add, ra, rb, 0, |x, y| x + y),
                Op::Mul => reduce_commutative(Op::Mul, ra, rb, 1, |x, y| x * y),
                Op::Sub => match (&ra, &rb) {
                    (Term::Int(x), Term::Int(y)) => Term::Int(x - y),
                    _ => Term::Bin(Op::Sub, Box::new(ra), Box::new(rb)),
                },
            }
        }
    }
}

/// Canonicalise a commutative+associative op: flatten the chain, fold the concrete
/// constants into one, sort the remaining symbolic operands by their stable term
/// order, and drop the identity element (`x + 0` → `x`, `x * 1` → `x`). The result
/// is order-independent, so `a + b` and `b + a` reduce to the same normal form.
fn reduce_commutative(
    op: Op,
    ra: Term,
    rb: Term,
    identity: i64,
    fold: fn(i64, i64) -> i64,
) -> Term {
    let mut operands = Vec::new();
    flatten(op, ra, &mut operands);
    flatten(op, rb, &mut operands);

    let mut acc = identity;
    let mut symbolic = Vec::new();
    for o in operands {
        match o {
            Term::Int(n) => acc = fold(acc, n),
            other => symbolic.push(other),
        }
    }
    symbolic.sort();

    // Keep the constant only when it is NOT the identity, or when there is nothing
    // else (so `0`/`1` themselves still reduce to a term).
    if acc != identity || symbolic.is_empty() {
        symbolic.push(Term::Int(acc));
    }

    symbolic
        .into_iter()
        .reduce(|l, r| Term::Bin(op, Box::new(l), Box::new(r)))
        .expect("at least one operand")
}

/// Flatten a (reduced) `op`-chain into its operands.
fn flatten(op: Op, t: Term, out: &mut Vec<Term>) {
    match t {
        Term::Bin(o, a, b) if o == op => {
            flatten(op, *a, out);
            flatten(op, *b, out);
        }
        other => out.push(other),
    }
}

/// Whether a normal-form term contains a CONSTRUCTED object anywhere.
fn contains_obj(t: &Term) -> bool {
    match t {
        Term::Obj(..) => true,
        Term::Bin(_, a, b) => contains_obj(a) || contains_obj(b),
        Term::Field(o, _) => contains_obj(o),
        Term::List(items) => items.iter().any(contains_obj),
        Term::Len(i) => contains_obj(i),
        _ => false,
    }
}

/// Whether a normal-form term mentions any free Given.
fn contains_sym(t: &Term) -> bool {
    match t {
        Term::Sym(_) => true,
        Term::Bin(_, a, b) => contains_sym(a) || contains_sym(b),
        Term::Field(o, _) => contains_sym(o),
        Term::Obj(_, fields) => fields.iter().any(|(_, v)| contains_sym(v)),
        Term::List(items) => items.iter().any(contains_sym),
        Term::Len(i) => contains_sym(i),
        _ => false,
    }
}

/// Decide `assertSame(a, b)` — PHP `===`, an IDENTITY check.
///
/// Sound only for scalar / symbolic normal forms: two equal scalar terms, or the
/// same arithmetic over the same free symbols, denote the same value and so are
/// `===`. Two FRESHLY constructed objects are NEVER `===` even when structurally
/// equal, and the kernel does not yet track object identity — so any normal form
/// containing a constructed object is fail-closed `Unknown` (the value-equality case
/// belongs to [`decide_eq`]). A symbolic object Given is a [`Term::Sym`], not an
/// `Obj`, so `assertSame($x, $x)` still decides `True` (same variable = same handle).
pub fn decide_same(a: &Term, b: &Term) -> Decision {
    let ra = reduce(a);
    let rb = reduce(b);
    if contains_obj(&ra) || contains_obj(&rb) {
        return Decision::Unknown;
    }
    if ra == rb {
        return Decision::True;
    }
    if !contains_sym(&ra) && !contains_sym(&rb) {
        return Decision::False;
    }
    Decision::Unknown
}

/// Decide `assertEquals(a, b)` — value / structural equality (the comparator chain
/// for objects). Two value-objects of the same class with equal fields are equal, so
/// structural identity of the normal forms decides `True`; two fully concrete and
/// distinct normal forms decide `False`; anything still symbolic is `Unknown`.
pub fn decide_eq(a: &Term, b: &Term) -> Decision {
    let ra = reduce(a);
    let rb = reduce(b);
    if ra == rb {
        return Decision::True;
    }
    if !contains_sym(&ra) && !contains_sym(&rb) {
        return Decision::False;
    }
    Decision::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(s: &str) -> Term {
        Term::Sym(s.to_string())
    }
    fn add(a: Term, b: Term) -> Term {
        Term::Bin(Op::Add, Box::new(a), Box::new(b))
    }
    fn field(o: Term, f: &str) -> Term {
        Term::Field(Box::new(o), f.to_string())
    }
    fn money(amount: Term) -> Term {
        Term::Obj("Money".to_string(), vec![("amount".to_string(), amount)])
    }

    #[test]
    fn field_access_substitutes_the_given_through_the_object() {
        // (new Money($a))->amount  ⇒  $a   — the object is transparent.
        assert_eq!(reduce(&field(money(sym("a")), "amount")), sym("a"));
    }

    #[test]
    fn addition_is_commutative_in_normal_form() {
        assert_eq!(
            reduce(&add(sym("a"), sym("b"))),
            reduce(&add(sym("b"), sym("a")))
        );
    }

    #[test]
    fn concrete_arithmetic_folds() {
        assert_eq!(reduce(&add(Term::Int(2), Term::Int(3))), Term::Int(5));
    }

    #[test]
    fn additive_identity_is_eliminated() {
        // $a + 0  ⇒  $a
        assert_eq!(reduce(&add(sym("a"), Term::Int(0))), sym("a"));
    }

    #[test]
    fn decide_same_identity_on_symbol_is_true() {
        // assertSame($x, $x) — same variable = same handle.
        assert_eq!(decide_same(&sym("x"), &sym("x")), Decision::True);
    }

    #[test]
    fn decide_same_distinct_symbols_is_unknown() {
        assert_eq!(decide_same(&sym("a"), &sym("b")), Decision::Unknown);
    }

    #[test]
    fn decide_same_distinct_concrete_is_false() {
        assert_eq!(decide_same(&Term::Int(5), &Term::Int(6)), Decision::False);
    }

    #[test]
    fn money_plus_getamount_decides_true_for_all_givens() {
        // THE canonical test, decided ∀ a, b with NO concrete values:
        //   $r = (new Money($a))->plus($b);   // plus(x){ return new Money($this->amount + x); }
        //   assertSame($a + $b, $r->getAmount());
        // RHS modelled as the post-substitution term tree (the mago→Term wiring,
        // next increment, will build this from the read method bodies):
        let plus_result = money(add(field(money(sym("a")), "amount"), sym("b")));
        let rhs = field(plus_result, "amount");
        let lhs = add(sym("a"), sym("b"));
        assert_eq!(decide_same(&lhs, &rhs), Decision::True);
    }

    #[test]
    fn decide_same_on_two_fresh_objects_is_unknown_not_true() {
        // SOUNDNESS: assertSame(new Money(5), new Money(5)) is FALSE in PHP (distinct
        // handles), so the structurally-equal normal forms must NOT claim True.
        assert_eq!(
            decide_same(&money(Term::Int(5)), &money(Term::Int(5))),
            Decision::Unknown
        );
    }

    #[test]
    fn decide_eq_on_equal_value_objects_is_true() {
        // assertEquals(new Money(5), new Money(5)) IS true (value equality).
        assert_eq!(
            decide_eq(&money(Term::Int(5)), &money(Term::Int(5))),
            Decision::True
        );
    }

    #[test]
    fn count_by_structure_decides_without_values() {
        // assertCount(2, [$x, $y])  ⇒  decide_eq(2, len[x,y]) → True ∀ x, y.
        let list = Term::List(vec![sym("x"), sym("y")]);
        assert_eq!(
            decide_eq(&Term::Int(2), &Term::Len(Box::new(list))),
            Decision::True
        );
    }
}
