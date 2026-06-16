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
    /// Poison: an INDECIDABLE value (e.g. concrete i64 arithmetic that overflowed —
    /// PHP would promote to float, which the kernel does not model). Opaque is
    /// absorbing: any op / Field / Len / Obj containing it reduces to Opaque, and any
    /// decision touching it is fail-closed [`Decision::Unknown`].
    Opaque,
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
        Term::Sym(_) | Term::Int(_) | Term::Bool(_) | Term::Str(_) | Term::Opaque => t.clone(),

        // Object transparency: `(new C{f: g})->f` substitutes to `g` (factoring the
        // Given through the object). A field access on a non-object stays symbolic.
        // A field access on an Opaque receiver is itself indecidable → Opaque.
        Term::Field(obj, name) => {
            let robj = reduce(obj);
            if robj == Term::Opaque {
                return Term::Opaque;
            }
            if let Term::Obj(_, fields) = &robj {
                if let Some((_, value)) = fields.iter().rev().find(|(k, _)| k == name) {
                    return reduce(value);
                }
            }
            Term::Field(Box::new(robj), name.clone())
        }

        Term::Obj(class, fields) => {
            let reduced: Vec<(String, Term)> =
                fields.iter().map(|(k, v)| (k.clone(), reduce(v))).collect();
            // An object carrying an indecidable field is itself indecidable.
            if reduced.iter().any(|(_, v)| *v == Term::Opaque) {
                return Term::Opaque;
            }
            Term::Obj(class.clone(), reduced)
        }

        Term::List(items) => {
            let reduced: Vec<Term> = items.iter().map(reduce).collect();
            if reduced.contains(&Term::Opaque) {
                return Term::Opaque;
            }
            Term::List(reduced)
        }

        Term::Len(inner) => {
            let rinner = reduce(inner);
            match &rinner {
                Term::Opaque => Term::Opaque,
                Term::List(items) => Term::Int(items.len() as i64),
                _ => Term::Len(Box::new(rinner)),
            }
        }

        Term::Bin(op, a, b) => {
            let ra = reduce(a);
            let rb = reduce(b);
            // An operand that is already indecidable poisons the whole expression.
            if ra == Term::Opaque || rb == Term::Opaque {
                return Term::Opaque;
            }
            match op {
                Op::Add => reduce_commutative(Op::Add, ra, rb, 0, i64::checked_add),
                Op::Mul => reduce_commutative(Op::Mul, ra, rb, 1, i64::checked_mul),
                Op::Sub => match (&ra, &rb) {
                    // Concrete i64 subtraction that overflows promotes to float in
                    // PHP (unmodelled) → Opaque rather than a wrapped/panicking value.
                    (Term::Int(x), Term::Int(y)) => match x.checked_sub(*y) {
                        Some(v) => Term::Int(v),
                        None => Term::Opaque,
                    },
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
    fold: fn(i64, i64) -> Option<i64>,
) -> Term {
    let mut operands = Vec::new();
    flatten(op, ra, &mut operands);
    flatten(op, rb, &mut operands);

    let mut acc = identity;
    let mut symbolic = Vec::new();
    for o in operands {
        match o {
            // Concrete-constant folding uses CHECKED arithmetic: an i64 overflow
            // promotes to float in PHP (unmodelled) → the whole op is Opaque.
            Term::Int(n) => match fold(acc, n) {
                Some(v) => acc = v,
                None => return Term::Opaque,
            },
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
        Term::Opaque => false,
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
        Term::Opaque => false,
        _ => false,
    }
}

/// Whether a normal-form term carries an [`Term::Opaque`] poison anywhere. A decision
/// over such a term is fail-closed [`Decision::Unknown`] (the value is indecidable,
/// e.g. it overflowed i64 and PHP would have promoted to float). Note: after
/// [`reduce`] the poison is absorbing, so a reduced term is either Opaque at the root
/// or Opaque-free — but the recursive walk is kept for robustness / pre-reduce use.
fn contains_opaque(t: &Term) -> bool {
    match t {
        Term::Opaque => true,
        Term::Bin(_, a, b) => contains_opaque(a) || contains_opaque(b),
        Term::Field(o, _) => contains_opaque(o),
        Term::Obj(_, fields) => fields.iter().any(|(_, v)| contains_opaque(v)),
        Term::List(items) => items.iter().any(contains_opaque),
        Term::Len(i) => contains_opaque(i),
        _ => false,
    }
}

/// Whether a term is a CONCRETE scalar (no free Given, no object, no opaque): one of
/// the directly-comparable variants whose value is fully known.
fn is_concrete_scalar(t: &Term) -> bool {
    matches!(t, Term::Int(_) | Term::Bool(_) | Term::Str(_))
}

/// A purely-NUMERIC string (PHP `is_numeric`-ish, the subset that matters for loose
/// `==`): optional sign, decimal digits with an optional single fractional part. Used
/// to keep `decide_eq("1.0","1")`-style numeric-string comparisons fail-closed
/// (PHP `==` compares them as numbers, which the kernel does not model).
fn is_numeric_str(s: &str) -> bool {
    let s = s.trim();
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for c in body.chars() {
        match c {
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot => seen_dot = true,
            _ => return false,
        }
    }
    seen_digit
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
    // An indecidable (overflowed) operand poisons the decision: fail-closed.
    if contains_opaque(&ra) || contains_opaque(&rb) {
        return Decision::Unknown;
    }
    if contains_obj(&ra) || contains_obj(&rb) {
        return Decision::Unknown;
    }
    if ra == rb {
        return Decision::True;
    }
    // `===` is STRICT in type: two distinct concrete normal forms (incl. cross-type,
    // e.g. Str("5") vs Int(5)) are NOT identical → False. This is sound for identity.
    if !contains_sym(&ra) && !contains_sym(&rb) {
        return Decision::False;
    }
    Decision::Unknown
}

/// Decide `assertEquals(a, b)` — PHP `==`, a LOOSE / value-structural comparison (the
/// comparator chain for objects).
///
/// `True` iff the two normal forms are structurally identical (same scalar value, the
/// same algebra over the same free symbols, or two value-objects of the same class
/// with equal fields).
///
/// `False` is given ONLY when the kernel can PROVE inequality under PHP `==`: both
/// reduced forms are concrete scalars of the **SAME variant** (Int&Int, Bool&Bool, or
/// two NON-both-numeric Strings) with different values. A CROSS-variant concrete pair
/// (`Str("5")` vs `Int(5)`, `Bool(true)` vs `Int(1)`, …) is the loose-`==` coercion
/// territory the kernel does not model — PHP may judge them EQUAL — so it is
/// fail-closed `Unknown`, NEVER `False`. Two numeric strings (`"1.0"` vs `"1"`) are
/// likewise compared NUMERICALLY by PHP, so they too are `Unknown` rather than `False`.
/// Anything still symbolic, object-bearing, or opaque is `Unknown`.
pub fn decide_eq(a: &Term, b: &Term) -> Decision {
    let ra = reduce(a);
    let rb = reduce(b);
    // An indecidable (overflowed) operand poisons the decision: fail-closed.
    if contains_opaque(&ra) || contains_opaque(&rb) {
        return Decision::Unknown;
    }
    if ra == rb {
        return Decision::True;
    }
    // Definitive False ONLY for a same-variant concrete-scalar pair of differing
    // value (the kernel can prove `==` is false). Everything else — cross-variant
    // concrete (loose `==` coercion), numeric-string pairs (numeric `==`), symbolic,
    // objects, opaque — is fail-closed Unknown.
    if is_concrete_scalar(&ra) && is_concrete_scalar(&rb) {
        match (&ra, &rb) {
            (Term::Int(_), Term::Int(_)) | (Term::Bool(_), Term::Bool(_)) => {
                return Decision::False
            }
            (Term::Str(x), Term::Str(y)) => {
                // Two numeric strings are compared as NUMBERS by PHP `==`
                // (`"1.0" == "1"` is true), which the kernel does not model →
                // Unknown. Otherwise (at least one non-numeric) a byte-distinct
                // string pair is provably `==`-unequal → False.
                if is_numeric_str(x) && is_numeric_str(y) {
                    return Decision::Unknown;
                }
                return Decision::False;
            }
            // Cross-variant concrete (Str vs Int, Bool vs Int, …): loose-`==`
            // coercion territory, NOT modelled → fail-closed Unknown.
            _ => return Decision::Unknown,
        }
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

    // ── FIX 1: decide_eq is PHP loose `==`, not Rust structural equality ───────

    #[test]
    fn decide_eq_cross_variant_str_int_is_unknown_not_false() {
        // assertEquals("5", 5) — PHP `==` coerces → PASS; the kernel cannot model
        // the coercion, so it must be Unknown (NOT a definitive False).
        assert_eq!(
            decide_eq(&Term::Str("5".into()), &Term::Int(5)),
            Decision::Unknown
        );
    }

    #[test]
    fn decide_eq_cross_variant_bool_int_is_unknown_not_false() {
        // assertEquals(true, 1) — PHP `==` → PASS → Unknown.
        assert_eq!(
            decide_eq(&Term::Bool(true), &Term::Int(1)),
            Decision::Unknown
        );
    }

    #[test]
    fn decide_eq_cross_variant_int_str_is_unknown_not_false() {
        // assertEquals(1, "1") — PHP `==` → PASS → Unknown.
        assert_eq!(
            decide_eq(&Term::Int(1), &Term::Str("1".into())),
            Decision::Unknown
        );
    }

    #[test]
    fn decide_eq_numeric_string_pair_is_unknown_not_false() {
        // assertEquals("1.0", "1") — PHP compares them as NUMBERS → PASS → Unknown.
        assert_eq!(
            decide_eq(&Term::Str("1.0".into()), &Term::Str("1".into())),
            Decision::Unknown
        );
    }

    #[test]
    fn decide_eq_same_variant_distinct_ints_is_false() {
        assert_eq!(decide_eq(&Term::Int(5), &Term::Int(6)), Decision::False);
    }

    #[test]
    fn decide_eq_same_variant_distinct_nonnumeric_strings_is_false() {
        // assertEquals("a", "b") — neither numeric, byte-distinct → provably `==`
        // unequal → False.
        assert_eq!(
            decide_eq(&Term::Str("a".into()), &Term::Str("b".into())),
            Decision::False
        );
    }

    #[test]
    fn decide_eq_equal_ints_is_true() {
        assert_eq!(decide_eq(&Term::Int(5), &Term::Int(5)), Decision::True);
    }

    #[test]
    fn decide_same_cross_type_str_int_is_false() {
        // assertSame("5", 5) — `===` is strict in TYPE → genuinely FALSE in PHP.
        assert_eq!(
            decide_same(&Term::Str("5".into()), &Term::Int(5)),
            Decision::False
        );
    }

    // ── FIX 2: i64 overflow → Opaque, never a panic, never a wrong verdict ─────

    #[test]
    fn reduce_add_overflow_yields_opaque_not_panic() {
        // i64::MAX + 1 overflows → Opaque (PHP would promote to float).
        let t = add(Term::Int(i64::MAX), Term::Int(1));
        assert_eq!(reduce(&t), Term::Opaque);
    }

    #[test]
    fn reduce_mul_overflow_yields_opaque() {
        let t = Term::Bin(
            Op::Mul,
            Box::new(Term::Int(i64::MAX)),
            Box::new(Term::Int(2)),
        );
        assert_eq!(reduce(&t), Term::Opaque);
    }

    #[test]
    fn reduce_sub_overflow_yields_opaque() {
        let t = Term::Bin(
            Op::Sub,
            Box::new(Term::Int(i64::MIN)),
            Box::new(Term::Int(1)),
        );
        assert_eq!(reduce(&t), Term::Opaque);
    }

    #[test]
    fn opaque_propagates_through_field_and_obj() {
        // An Obj whose field overflowed reduces to Opaque, and a Field read off it too.
        let obj = money(add(Term::Int(i64::MAX), Term::Int(1)));
        assert_eq!(reduce(&obj), Term::Opaque);
        assert_eq!(reduce(&field(obj, "amount")), Term::Opaque);
    }

    #[test]
    fn decide_same_on_identical_overflow_exprs_is_unknown_not_true() {
        // assertSame(MAX+MAX, MAX+MAX): structurally identical Bin trees, but both
        // overflow → each reduces to Opaque → fail-closed Unknown (NOT True, and no
        // panic). PHP computes the same float on both sides (PASS), so True would be
        // unsound w.r.t. the kernel's claim of having decided it — Unknown is correct.
        let lhs = add(Term::Int(i64::MAX), Term::Int(i64::MAX));
        let rhs = add(Term::Int(i64::MAX), Term::Int(i64::MAX));
        assert_eq!(decide_same(&lhs, &rhs), Decision::Unknown);
    }

    #[test]
    fn decide_eq_with_opaque_operand_is_unknown() {
        let lhs = add(Term::Int(i64::MAX), Term::Int(1));
        assert_eq!(decide_eq(&lhs, &Term::Int(0)), Decision::Unknown);
    }
}
