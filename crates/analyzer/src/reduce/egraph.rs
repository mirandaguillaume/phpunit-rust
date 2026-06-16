//! E-graph congruence-closure reduction engine (increment 2).
//!
//! # From a fixed fragment (v1) to equations DERIVED from real PHP (v2)
//!
//! v1 proved the mechanics on a HARD-CODED `Money` fragment with three hand-written
//! rules over a closed `define_language!` enum. v2 **opens the signature** and
//! **derives the equations from the actual method bodies** of the classes a test
//! touches — the step that crosses the wall of STATIC FACTORIES (real value-object
//! libraries write `Num::of(5)`, not `new Num(5)`).
//!
//! ## The model
//!
//! * **Open signature** — every e-node is [`egg::SymbolLang`]: an `op: Symbol` plus
//!   child `Id`s. A literal integer `5` is a childless node whose op parses to `5`;
//!   `(+ a b)`, `(Num 5)` (a constructed object), `(Num::of 5)` (a factory call),
//!   `(value x)` (a method call) are all SymbolLang nodes. No enum to widen — any
//!   class, factory, or method name is just another `Symbol`.
//!
//! * **Ground evaluation** ([`GroundEval`]) over the open signature — an op that
//!   parses to `i64` is that constant; `{+,-,*}` over two concrete children folds
//!   via `checked_{add,sub,mul}` (overflow → `None` = no fold, fail-closed, since
//!   PHP would promote to float, which this fragment does not model); everything
//!   else is `None`.
//!
//! * **Equation derivation** ([`derive_rules`]) — the heart. For each class the test
//!   touches we compute its FIELD LAYOUT (the order of its construction properties:
//!   promoted ctor params + the ctor body's `$this->x = …` writes). Then for each
//!   PURE method of the form `{ return <expr>; }` (a single return; anything else
//!   leaves the method an OPAQUE symbol → no rule → congruence cannot fire →
//!   fail-closed) we build one oriented [`egg::Rewrite`] PROGRAMMATICALLY:
//!     - the receiver `$this` of class `C` becomes the pattern `(C ?c_f0 ?c_f1 …)`
//!       (its fields as fresh pattern variables); a STATIC method (a factory) has no
//!       receiver;
//!     - a param `p: T` where `T` is a known class `D` becomes `(D ?p_f0 …)`; a
//!       scalar/unknown param becomes a bare variable `?p`;
//!     - **LHS** = `(m <receiver-pattern> <param-patterns>)`;
//!     - **RHS** = the return expression translated into the same pattern alphabet
//!       (`$this->fj → ?c_fj`, `$p->fj → ?p_fj`, `$p → ?p`, literal → literal,
//!       `new D(args)` / `D::fab(args) → (D <args>)`, `$x+$y → (+ <x> <y>)`, …).
//!       Because `new C(args)` and a factory `C::of(args)` both map onto `(C …)`, the
//!       constructed and factory-produced objects share one e-class — wall crossed.
//!
//! * **Decision** — insert the two arguments of the final `assertSame(L, R)` into
//!   ONE e-graph, saturate with ALL derived rules + ground folding, and decide by
//!   CONGRUENCE: `find(L) == find(R)`. No execution, no object is ever constructed —
//!   only symbols, equations, and the known leaves.
//!
//! ## The decisive fixture (proved end-to-end)
//!
//! ```php
//! final class Num {
//!     public function __construct(private int $v) {}
//!     public static function of(int $v): self { return new Num($v); }
//!     public function plus(Num $o): Num { return new Num($this->v + $o->v); }
//!     public function value(): int { return $this->v; }
//! }
//! // testStaticFactoryPlus: assertSame(8, Num::of(5)->plus(Num::of(3))->value())
//! ```
//! Derived rules: `(Num::of ?v) => (Num ?v)`, `(plus (Num ?a) (Num ?b)) => (Num (+ ?a ?b))`,
//! `(value (Num ?a)) => ?a`. Path: `of(5)→(Num 5)`, `of(3)→(Num 3)`,
//! `plus((Num 5),(Num 3))→(Num (+ 5 3))→(Num 8)`, `value((Num 8))→8`; LHS `8` ≡ RHS.
//! The real PHPUnit php8.4 gold-gate of this test PASSES, so the `True` is sound.
//!
//! ## egg-0.11 API notes (where v2 deviates from the macro path)
//!
//! The prompt asks to build the rewrites PROGRAMMATICALLY rather than via the
//! `rewrite!` macro. We do exactly that: each LHS/RHS motif is materialised as a
//! `PatternAst<SymbolLang>` (= `RecExpr<ENodeOrVar<SymbolLang>>`) node-by-node, with
//! pattern variables built as `ENodeOrVar::Var("?x".parse()?)` and concrete e-nodes
//! as `ENodeOrVar::ENode(SymbolLang::new(op, children))`; the two asts are wrapped in
//! `Pattern::from(ast)` and handed to `Rewrite::new(name, searcher, applier)`. This
//! sidesteps the s-expression text parser entirely, so class names containing `::`
//! or `\` never round-trip through tokenisation. Input test expressions are likewise
//! inserted with `egraph.add(SymbolLang::new(...))` rather than `add_expr` on a
//! parsed string.
//!
//! # v3 — data-provider substitution + arithmetic/assertion routing
//!
//! Real tests are mostly `#[DataProvider]`-parametrised. v3 evaluates a static
//! provider whose body is a literal array of INTEGER rows, binds each row's columns
//! to the test parameters as concrete leaves, and aggregates: the method decides
//! `True` iff EVERY row decides `True`, `False` iff some row is provably `False` and
//! none is `Unknown`, else `Unknown`. Ground folding gains `/` (only when `b != 0 &&
//! a % b == 0`, since PHP promotes a non-exact `/` to a float we do not model) and
//! `%`; assertion routing covers `assertEquals`/`assertNotSame`/`assertNotEquals`/
//! `assertCount`/`assertTrue` beside `assertSame` (all still fail-closed on anything
//! outside the integer fragment).
//!
//! # v4 — cross-file rule derivation (the same-file wall)
//!
//! v1–v3 derived rules from the TEST file's program only, so a value object living
//! in `src/Foo.php` (every PSR-4 library) yielded no rules → `Unknown` by
//! construction. v4 computes the transitive closure of classes a test references
//! (`new C`, `C::fab`, and the classes appearing in derived bodies), resolves each
//! through the codex (`get_class_like` → `file_of_span` → `with_program` reparse, the
//! same path `subst.rs` uses), and derives the field catalogue + rules across ALL
//! those files. A `visited` set and a class cap bound it. `constructible_layout`
//! only admits a class whose ctor is a PURE positional seed (promoted params, or
//! `$this->f = $param` pass-throughs in argument order); a validating / normalising /
//! reordering / branching ctor leaves the class OUT of the catalogue → `new C(...)`
//! builds no node → `Unknown`. So a normalising VO (e.g. a fraction reducing
//! `new F(0, 5)` to `0/1`) is never given a forged construction rule.
//!
//! # Scope & limits (the honest perimeter)
//!
//! DECIDES (sound, gold-verified vs real php8.4): tests of INTEGER value objects —
//! `i64` fields stored by a pure positional ctor, pure `{ return <i64-expr-over-
//! fields> }` getters/transforms, `new` + static factories, `{+,-,*,exact-/,%}`,
//! integer `assertSame`/`assertEquals`/…, integer `#[DataProvider]`/`#[TestWith]`
//! rows — across `src/`+`tests/` files. This is the natural shape of APPLICATION
//! domain value objects (Money-in-cents, Point, Score, Coordinate).
//!
//! FAILS CLOSED (→ `Unknown`, the runner executes for real; never a wrong verdict):
//! non-integer DOMAINS (bignum/GMP-string, `BigDecimal`, float, date/`DateTime`,
//! enums, formatted strings), validating/normalising constructors, multi-statement
//! or branching method bodies, `__get`/magic getters, thrown-exception tests,
//! collections/arrays beyond `assertCount`, and Pest functional-closure tests
//! (no method discovery).
//!
//! MEASURED OFFER: ~0% on mature OSS PRIMITIVE libraries (brick-math, carbon,
//! moneyphp, …) — they store GMP/`DateTime`/string state and validate in their
//! ctors, so the integer-VO shape is structurally absent. The reachable surface is
//! application-level integer value objects, proven end-to-end here (split-file Point
//! and a Money-in-cents fixture both decide 100% with a passing real-PHPUnit gold
//! gate) but not present in the surveyed OSS corpus.

use std::collections::HashMap;

use egg::{Analysis, DidMerge, EGraph, ENodeOrVar, Id, Pattern, PatternAst, Rewrite, Runner};
use egg::{Condition, ConditionalApplier, Language, Subst, SymbolLang, Var};

use mago_syntax::ast::ast::access::Access;
use mago_syntax::ast::ast::argument::{Argument, ArgumentList};
use mago_syntax::ast::ast::binary::BinaryOperator;
use mago_syntax::ast::ast::call::Call;
use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
use mago_syntax::ast::ast::class_like::method::{Method, MethodBody};
use mago_syntax::ast::ast::class_like::Class;
use mago_syntax::ast::ast::expression::Expression;
use mago_syntax::ast::ast::function_like::parameter::FunctionLikeParameter;
use mago_syntax::ast::ast::instantiation::Instantiation;
use mago_syntax::ast::ast::literal::Literal;
use mago_syntax::ast::ast::statement::Statement;
use mago_syntax::ast::ast::type_hint::Hint;
use mago_syntax::ast::ast::variable::Variable;
use mago_syntax::ast::Program;

use super::eval::BailReason;
use super::subst::{find_class_method, normalize_fqcn, strip_dollar};
use super::term::Decision;
use crate::concrete::{compute, ArrayKey, Context, PhpValue};
use crate::mago_bridge::MagoProject;
use mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector;

/// The open signature: every e-node is a `SymbolLang` `(op child0 child1 …)`.
pub type PhpL = SymbolLang;

// ─── Ground evaluation over the OPEN signature ─────────────────────────────────

/// E-class analysis: ground evaluation (constant folding) over `SymbolLang`,
/// fail-closed on overflow.
///
/// `Data` is the constant value of an e-class when it is decidably a concrete
/// integer, else `None`. An `op` that parses to `i64` (a numeric leaf) is that
/// constant; `{+,-,*}` over two concrete children folds via `checked_*` (overflow
/// → `None`, since PHP promotes to float which this fragment does not model);
/// everything else is `None`. Modelled after egg's `ConstantFold` tutorial.
#[derive(Default)]
pub struct GroundEval;

impl Analysis<PhpL> for GroundEval {
    type Data = Option<i64>;

    fn make(egraph: &mut EGraph<PhpL, Self>, enode: &PhpL, _id: Id) -> Self::Data {
        let v = |i: &Id| egraph[*i].data;
        let op = enode.op.as_str();
        let kids = enode.children();
        // A childless op that parses as a base-10 i64 is that constant.
        if kids.is_empty() {
            return op.parse::<i64>().ok();
        }
        if kids.len() == 2 {
            let (a, b) = (v(&kids[0])?, v(&kids[1])?);
            // `checked_*` → `None` on overflow = no fold (totality, fail-closed).
            return match op {
                "+" => a.checked_add(b),
                "-" => a.checked_sub(b),
                "*" => a.checked_mul(b),
                // PHP `/` is a FLOAT operator: `7 / 2` is `3.5`, not `3`. This
                // fragment models only i64, so `/` folds ONLY when the quotient is
                // EXACT (`b != 0` and `a % b == 0`) — then PHP yields the integer
                // anyway; otherwise no fold (fail-closed, the float case is
                // unmodelled). `i64::MIN / -1` overflows → `checked_div` = None.
                "/" => {
                    if b != 0 && a % b == 0 {
                        a.checked_div(b)
                    } else {
                        None
                    }
                }
                // PHP `%` is integer modulo; `b == 0` raises (DivisionByZeroError),
                // unmodelled → no fold. `i64::MIN % -1` overflows → `checked_rem`.
                "%" => {
                    if b != 0 {
                        a.checked_rem(b)
                    } else {
                        None
                    }
                }
                _ => None,
            };
        }
        None
    }

    fn merge(&mut self, a: &mut Self::Data, b: Self::Data) -> DidMerge {
        egg::merge_option(a, b, |x, y| {
            // Two concrete constants in one e-class must agree — soundness invariant.
            debug_assert_eq!(*x, y);
            DidMerge(false, false)
        })
    }

    fn modify(egraph: &mut EGraph<PhpL, Self>, id: Id) {
        // When an e-class is known-constant, materialise that integer leaf and union
        // it in, so the literal and the computed term share one e-class.
        if let Some(c) = egraph[id].data {
            let n = egraph.add(SymbolLang::leaf(c.to_string()));
            egraph.union(id, n);
        }
    }
}

// ─── Pattern (motif) building blocks for programmatic rewrites ─────────────────

/// A motif: a tiny tree we build during derivation and later lower into either a
/// `PatternAst` (for a rewrite side) — variables stay variables — or a concrete
/// `RecExpr<SymbolLang>` is built directly in the e-graph for test inputs.
#[derive(Clone, Debug)]
enum Motif {
    /// A pattern variable (`?x`); only valid inside a rewrite.
    Var(String),
    /// An operator node `(op child…)`. A childless `op` that is a decimal integer
    /// is a literal; otherwise it is a constructor / method / arithmetic node.
    Node(String, Vec<Motif>),
}

impl Motif {
    fn leaf(op: impl Into<String>) -> Self {
        Motif::Node(op.into(), Vec::new())
    }

    /// Lower this motif into a `PatternAst` node, returning the root `Id`. A
    /// `Motif::Var` becomes `ENodeOrVar::Var`; a `Motif::Node` becomes an
    /// `ENodeOrVar::ENode` once its children are lowered.
    fn lower_into_pattern(&self, ast: &mut PatternAst<PhpL>) -> Result<Id, BailReason> {
        match self {
            Motif::Var(name) => {
                let var: Var = name
                    .parse()
                    .map_err(|_| BailReason::Other(format!("bad pattern var {name}")))?;
                Ok(ast.add(ENodeOrVar::Var(var)))
            }
            Motif::Node(op, kids) => {
                let child_ids: Vec<Id> = kids
                    .iter()
                    .map(|k| k.lower_into_pattern(ast))
                    .collect::<Result<_, _>>()?;
                Ok(ast.add(ENodeOrVar::ENode(SymbolLang::new(op.clone(), child_ids))))
            }
        }
    }

    /// Build a `Pattern` from this motif (for a rewrite LHS or RHS).
    fn to_pattern(&self) -> Result<Pattern<PhpL>, BailReason> {
        let mut ast = PatternAst::default();
        self.lower_into_pattern(&mut ast)?;
        Ok(Pattern::new(ast))
    }
}

// ─── The class catalogue: field layouts gathered from the test file ────────────

/// A class's FIELD LAYOUT: the ordered names of its construction properties
/// (promoted ctor params followed by the ctor body's `$this->x = …` writes).
type FieldLayout = Vec<String>;

/// The catalogue of classes declared in the test file: FQCN-key → field layout.
/// Keyed by the lower-cased simple class name (the program is single-file; the
/// fixtures are namespace-free, mirroring the bridge's resolution scope).
type ClassCatalogue = HashMap<String, FieldLayout>;

/// A comparison a ctor VALIDATION GUARD applies to one field: the guard `if ($f <op> N)
/// throw` means the construction THROWS when `field <op> N` holds. The construction is
/// VALID (the rule may fire) only when it does NOT hold — proven per-row via the field's
/// ground constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

/// One admitted ctor validation guard, resolved to the FIELD it constrains.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FieldGuard {
    field: String,
    op: CmpOp,
    literal: i64,
}

/// Whether the guard's THROW comparison `v <op> lit` holds for a concrete field value.
fn cmp_throws(op: CmpOp, v: i64, lit: i64) -> bool {
    match op {
        CmpOp::Lt => v < lit,
        CmpOp::Le => v <= lit,
        CmpOp::Gt => v > lit,
        CmpOp::Ge => v >= lit,
        CmpOp::Eq => v == lit,
        CmpOp::Ne => v != lit,
    }
}

/// Map a binary comparison operator to a foldable `CmpOp`; `None` for non-comparisons or
/// the loose `==`/`!=` over which an integer guard is not soundly decided here (we keep
/// only the strict integer comparisons and identical/not-identical, all exact on i64).
fn cmp_op_of(op: &BinaryOperator) -> Option<CmpOp> {
    Some(match op {
        BinaryOperator::LessThan(_) => CmpOp::Lt,
        BinaryOperator::LessThanOrEqual(_) => CmpOp::Le,
        BinaryOperator::GreaterThan(_) => CmpOp::Gt,
        BinaryOperator::GreaterThanOrEqual(_) => CmpOp::Ge,
        BinaryOperator::Identical(_) | BinaryOperator::Equal(_) => CmpOp::Eq,
        BinaryOperator::NotIdentical(_) | BinaryOperator::NotEqual(_) => CmpOp::Ne,
        _ => return None,
    })
}

/// An egg rewrite CONDITION encoding a guarded class's ctor validity: a getter/transform
/// of the class fires ONLY when EVERY ctor guard provably does NOT throw for the bound
/// receiver fields. Each check `(field-var, throw-cmp, literal)` passes iff the field folds
/// to a concrete `v` for which the throw-comparison is FALSE. An unfolded field, or a
/// field that DOES satisfy the throw-comparison, fails the check → the rule does not fire →
/// the construction stays opaque → fail-closed Unknown (the runner executes it).
struct GuardCondition {
    checks: Vec<(Var, CmpOp, i64)>,
}

impl Condition<PhpL, GroundEval> for GuardCondition {
    fn check(&self, egraph: &mut EGraph<PhpL, GroundEval>, _eclass: Id, subst: &Subst) -> bool {
        for (var, op, lit) in &self.checks {
            let Some(&id) = subst.get(*var) else {
                return false;
            };
            let Some(v) = egraph[id].data else {
                return false; // field not folded to a constant → cannot prove valid
            };
            if cmp_throws(*op, v, *lit) {
                return false; // this guard throws → construction invalid → do not fire
            }
        }
        true
    }
}

/// The CONSTRUCTIBLE field layout of a class, or `None` when the ctor is not a pure
/// positional seed (so NO construction rule is derivable and the class stays opaque →
/// fail-closed Unknown). The construction node is `(C arg0 arg1 …)`, with `argK` bound
/// positionally to the K-th ctor parameter; for a getter `(f (C a b)) => a` to be
/// SOUND, layout slot K must hold the value of construction arg K. We therefore accept
/// ONLY ctors whose K-th parameter flows, unchanged, into exactly one field:
///   * a PROMOTED param `private int $x` (PHP itself assigns arg K → field `x`), or
///   * a body that is EXCLUSIVELY pure pass-through assignments `$this->f = $param;`
///     where `$param` is a direct ctor parameter — and the layout is ordered by PARAM
///     position so slot K ≡ arg K.
///
/// A ctor that VALIDATES (`if (…) throw`), NORMALISES (`$this->n = $n / $g`, a ternary,
/// a call), reorders, or has ANY non-pass-through statement returns `None`: `new C(a,
/// b)` is then NOT soundly `(C a b)` (it could reject or rewrite its inputs), so we must
/// not invent that node. No ctor at all → an empty layout (`new C()`).
fn constructible_layout(class: &Class) -> Option<FieldLayout> {
    ctor_model(class).map(|(layout, _)| layout)
}

/// The ctor VALIDATION GUARDS resolved to the fields they constrain (empty when the ctor
/// has none, or when the class is opaque).
fn ctor_guards(class: &Class) -> Vec<FieldGuard> {
    ctor_model(class).map(|(_, g)| g).unwrap_or_default()
}

/// The CONSTRUCTIBLE model: a class's field layout PLUS any admitted leading validation
/// guards `if ($param <cmp> N) throw;`, resolved to the fields they constrain. A guard is
/// no longer a bail — the construction is CONDITIONALLY valid, decided per-row by folding
/// the field (a getter rule fires only when no guard throws). Still `None` (opaque) for a
/// normalising/reordering ctor, a guard with an `else`/non-throw body, or a guard over a
/// param that does not flow into a stored field (it could not be expressed as a receiver
/// condition).
fn ctor_model(class: &Class) -> Option<(FieldLayout, Vec<FieldGuard>)> {
    let Some(ctor) = find_method_in_class(class, b"__construct") else {
        return Some((Vec::new(), Vec::new()));
    };
    let mut passthrough: HashMap<Vec<u8>, String> = HashMap::new();
    let mut raw_guards: Vec<(Vec<u8>, CmpOp, i64)> = Vec::new();
    if let MethodBody::Concrete(block) = &ctor.body {
        for stmt in block.statements.iter() {
            // A leading validation guard is ADMITTED (recorded), not bailed.
            if let Some(g) = validation_guard(stmt) {
                raw_guards.push(g);
                continue;
            }
            let (field, src_param) = pure_passthrough_assignment(stmt)?;
            if passthrough.values().any(|f| *f == field) || passthrough.contains_key(&src_param) {
                return None;
            }
            passthrough.insert(src_param, field);
        }
    }
    // Build the layout in PARAMETER order (slot K ≡ arg K) and a param→field map to
    // resolve guards against.
    let mut layout: FieldLayout = Vec::new();
    let mut param_field: HashMap<Vec<u8>, String> = HashMap::new();
    let mut used = 0usize;
    for p in ctor.parameter_list.parameters.iter() {
        if p.ellipsis.is_some() || p.ampersand.is_some() {
            return None;
        }
        let pname = strip_dollar(p.variable.name);
        let field = if p.is_promoted_property() {
            String::from_utf8_lossy(&pname).into_owned()
        } else if let Some(f) = passthrough.get(&pname) {
            used += 1;
            f.clone()
        } else {
            return None;
        };
        param_field.insert(pname.clone(), field.clone());
        layout.push(field);
    }
    if used != passthrough.len() {
        return None;
    }
    // Resolve each guard's param to its stored field; a guard over an UNSTORED param
    // cannot become a receiver condition ⇒ bail (keep the class opaque).
    let mut guards = Vec::new();
    for (param, op, literal) in raw_guards {
        let field = param_field.get(&param)?.clone();
        guards.push(FieldGuard { field, op, literal });
    }
    Some((layout, guards))
}

/// Recognise a ctor EXIT GUARD `if ($param <cmp> <int>) <body>;` whose body ALWAYS exits
/// (a `throw`, or a normalise-and-`return` branch), with no `elseif`/`else`. Returns
/// `(param, exit-cmp, literal)`: the pass-through below runs only when the comparison is
/// FALSE, so the rules are conditioned on "guard not taken". `None` for anything else (the
/// caller then treats it as a failing pass-through, keeping the ctor opaque).
fn validation_guard(stmt: &Statement) -> Option<(Vec<u8>, CmpOp, i64)> {
    use mago_syntax::ast::ast::control_flow::r#if::IfBody;
    let Statement::If(if_stmt) = stmt else {
        return None;
    };
    let IfBody::Statement(body) = &if_stmt.body else {
        return None;
    };
    if !body.else_if_clauses.is_empty() || body.else_clause.is_some() {
        return None;
    }
    let body_stmt: &Statement = body.statement;
    if !statement_always_exits(body_stmt) {
        return None;
    }
    let cond: &Expression = if_stmt.condition;
    let Expression::Binary(b) = cond else {
        return None;
    };
    let op = cmp_op_of(&b.operator)?;
    param_cmp_literal(b.lhs, b.rhs, op).or_else(|| param_cmp_literal(b.rhs, b.lhs, flip_cmp(op)))
}

/// `($param, op, literal)` when `a` is a direct variable and `b` an integer literal.
fn param_cmp_literal(a: &Expression, b: &Expression, op: CmpOp) -> Option<(Vec<u8>, CmpOp, i64)> {
    let Expression::Variable(Variable::Direct(v)) = a else {
        return None;
    };
    let Expression::Literal(Literal::Integer(i)) = b else {
        return None;
    };
    let lit = i.value.map(|v| v as i64)?;
    Some((strip_dollar(v.name), op, lit))
}

/// Mirror a comparison when its operands are swapped (`N < $p` ≡ `$p > N`).
fn flip_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
    }
}

/// Whether `stmt`, when reached, ALWAYS exits the ctor without falling through to the
/// pass-through assignments — a `throw …;`, a `return …;`, or a block whose LAST statement
/// always exits. A guarded branch that always exits means the pass-through below runs ONLY
/// when the guard's condition is FALSE, so conditioning the rules on "guard not taken" is
/// sound whether the branch throws (invalid input) or normalises-and-returns (a different,
/// unmodelled path); in both cases we decide only the not-taken case.
fn statement_always_exits(stmt: &Statement) -> bool {
    match stmt {
        Statement::Block(block) => block
            .statements
            .iter()
            .last()
            .is_some_and(statement_always_exits),
        Statement::Expression(es) => matches!(es.expression, Expression::Throw(_)),
        Statement::Return(_) => true,
        _ => false,
    }
}

/// If `stmt` is a PURE pass-through `$this->field = $param;` (the rhs a direct ctor
/// parameter variable, nothing computed), the `(field, source-param)` pair; else
/// `None` — which fails the whole ctor closed (a throw / if / normalisation / call).
fn pure_passthrough_assignment(stmt: &Statement) -> Option<(String, Vec<u8>)> {
    use mago_syntax::ast::ast::assignment::AssignmentOperator;
    let Statement::Expression(es) = stmt else {
        return None;
    };
    let Expression::Assignment(a) = es.expression else {
        return None;
    };
    // Only a plain `=` seed; `+=` etc. are not pure pass-throughs.
    if !matches!(a.operator, AssignmentOperator::Assign(_)) {
        return None;
    }
    let Expression::Access(Access::Property(pa)) = a.lhs else {
        return None;
    };
    let Expression::Variable(Variable::Direct(recv)) = pa.object else {
        return None;
    };
    if strip_dollar(recv.name) != b"this" {
        return None;
    }
    let ClassLikeMemberSelector::Identifier(prop_id) = &pa.property else {
        return None;
    };
    // The RHS must be a DIRECT parameter variable — no arithmetic, ternary, call, or
    // literal (those would normalise, so the positional `(C a b)` node would be a lie).
    let Expression::Variable(Variable::Direct(src)) = a.rhs else {
        return None;
    };
    let field = String::from_utf8_lossy(prop_id.value).into_owned();
    Some((field, strip_dollar(src.name)))
}

fn find_method_in_class<'a>(class: &'a Class<'a>, method: &[u8]) -> Option<&'a Method<'a>> {
    for member in class.members.iter() {
        if let ClassLikeMember::Method(m) = member {
            if m.name.value.eq_ignore_ascii_case(method) {
                return Some(m);
            }
        }
    }
    None
}

// ─── Equation derivation ───────────────────────────────────────────────────────

/// Derive the `{ return <expr>; }` rules of ONE class (each pure single-return method
/// or factory → one oriented rewrite; an opaque body contributes NO rule and stays an
/// opaque symbol → congruence cannot fire → fail-closed).
fn derive_from_class(
    class: &Class,
    cat: &ClassCatalogue,
    rules: &mut Vec<Rewrite<PhpL, GroundEval>>,
    descriptions: &mut Vec<String>,
) {
    let class_name = String::from_utf8_lossy(class.name.value).into_owned();
    // The ctor's admitted validation guards condition every instance rule of this class.
    let guards = ctor_guards(class);
    for member in class.members.iter() {
        let ClassLikeMember::Method(m) = member else {
            continue;
        };
        let method_name = String::from_utf8_lossy(m.name.value).into_owned();
        // The constructor is captured by the field layout, not as a rewrite.
        if method_name.eq_ignore_ascii_case("__construct") {
            continue;
        }
        if let Some(rule) = derive_method_rule(&class_name, m, cat, &guards) {
            descriptions.push(rule.description);
            rules.push(rule.rewrite);
        }
    }
}

struct DerivedRule {
    rewrite: Rewrite<PhpL, GroundEval>,
    description: String,
}

/// Try to derive a single oriented rewrite from one method body. Returns `None`
/// (fail-closed) whenever the method is not a single-return or the return
/// expression escapes the modelled fragment.
fn derive_method_rule(
    class_name: &str,
    m: &Method,
    cat: &ClassCatalogue,
    guards: &[FieldGuard],
) -> Option<DerivedRule> {
    let MethodBody::Concrete(block) = &m.body else {
        return None;
    };
    let ret_expr = single_return_expr(block)?;
    let method_name = String::from_utf8_lossy(m.name.value).into_owned();

    // The pattern-variable environment: a bare var name (`$p`) → its motif, and a
    // class param / `$this` → its field map (field-name → the `?p_fj` motif).
    let mut var_motifs: HashMap<Vec<u8>, Motif> = HashMap::new();
    // `$p->field` and `$this->field` projections: (object-var, field) → motif.
    let mut field_motifs: HashMap<(Vec<u8>, String), Motif> = HashMap::new();

    // Build the receiver motif (`$this`) unless the method is static (a factory).
    let is_static = method_is_static(m);
    let lhs_children = {
        let mut kids: Vec<Motif> = Vec::new();
        if !is_static {
            let class_key = class_name.to_ascii_lowercase();
            let layout = cat.get(&class_key)?;
            let mut fields = Vec::new();
            for f in layout {
                let v = format!("?this_{f}");
                fields.push(Motif::Var(v.clone()));
                field_motifs.insert((b"this".to_vec(), f.clone()), Motif::Var(v));
            }
            kids.push(Motif::Node(class_name.to_string(), fields));
        }
        // Each parameter: a known class → expand its fields as `(D ?p_f0 …)`; else a
        // bare variable `?p`. The param's motif is BOTH the LHS child at its position
        // AND the binding used to translate its occurrences in the RHS.
        for p in m.parameter_list.parameters.iter() {
            // Variadic / by-ref params are not modelled.
            if p.ellipsis.is_some() || p.ampersand.is_some() {
                return None;
            }
            let pname = strip_dollar(p.variable.name);
            let motif = match param_class(p, cat) {
                Some((class_fqcn, layout)) => {
                    let mut fields = Vec::new();
                    for f in &layout {
                        let v = format!("?{}_{f}", String::from_utf8_lossy(&pname));
                        fields.push(Motif::Var(v.clone()));
                        field_motifs.insert((pname.clone(), f.clone()), Motif::Var(v));
                    }
                    Motif::Node(class_fqcn, fields)
                }
                None => Motif::Var(format!("?{}", String::from_utf8_lossy(&pname))),
            };
            var_motifs.insert(pname.clone(), motif.clone());
            kids.push(motif);
        }
        kids
    };

    // A STATIC method is a factory: its call op is `Class::method` (matching how the
    // test extractor emits `C::factory(...)`); an instance method's op is the bare
    // method name (the receiver is the first child).
    let lhs_op = if is_static {
        format!("{class_name}::{method_name}")
    } else {
        method_name.clone()
    };
    let lhs = Motif::Node(lhs_op, lhs_children);
    let rhs = build_rhs_motif(ret_expr, &var_motifs, &field_motifs, cat)?;

    let lhs_pat = lhs.to_pattern().ok()?;
    let rhs_pat = rhs.to_pattern().ok()?;
    let rule_name = format!("{class_name}::{method_name}");
    let description = format!("{lhs_pat} => {rhs_pat}");
    // `Rewrite::new` rejects an applier referring to a var the searcher does not
    // bind — a built-in soundness check we rely on (fail-closed on a malformed rule).
    let rewrite = if is_static || guards.is_empty() {
        Rewrite::new(rule_name, lhs_pat, rhs_pat).ok()?
    } else {
        // An INSTANCE rule of a guarded class fires only when no ctor guard throws for the
        // bound receiver fields (`?this_<field>`). A field whose Var cannot be formed bails
        // the whole rule (opaque, fail-closed).
        let checks: Option<Vec<(Var, CmpOp, i64)>> = guards
            .iter()
            .map(|g| {
                format!("?this_{}", g.field)
                    .parse::<Var>()
                    .ok()
                    .map(|v| (v, g.op, g.literal))
            })
            .collect();
        let applier = ConditionalApplier {
            condition: GuardCondition { checks: checks? },
            applier: rhs_pat,
        };
        Rewrite::new(rule_name, lhs_pat, applier).ok()?
    };
    Some(DerivedRule {
        rewrite,
        description,
    })
}

/// Whether a method carries the `static` modifier (a factory).
fn method_is_static(m: &Method) -> bool {
    use mago_syntax::ast::ast::modifier::Modifier;
    m.modifiers
        .iter()
        .any(|modifier| matches!(modifier, Modifier::Static(_)))
}

/// If a parameter's hint names a class present in the catalogue, its FQCN (original
/// cased) and field layout; else `None` (scalar / unknown → a bare variable).
fn param_class(p: &FunctionLikeParameter, cat: &ClassCatalogue) -> Option<(String, FieldLayout)> {
    let hint = p.hint.as_ref()?;
    let name = class_hint_name(hint)?;
    let key = String::from_utf8_lossy(&normalize_fqcn(&name)).to_ascii_lowercase();
    let layout = cat.get(&key)?;
    Some((String::from_utf8_lossy(&name).into_owned(), layout.clone()))
}

/// The class name of a `Hint` if it is a plain identifier hint (not a scalar / union
/// / nullable / array / `self`-family).
fn class_hint_name(hint: &Hint) -> Option<Vec<u8>> {
    match hint {
        Hint::Identifier(id) => Some(id.value().to_vec()),
        _ => None,
    }
}

/// Translate a method's return expression into a motif over the param/`$this`
/// pattern variables. Returns `None` when the expression escapes the fragment.
fn build_rhs_motif(
    expr: &Expression,
    var_motifs: &HashMap<Vec<u8>, Motif>,
    field_motifs: &HashMap<(Vec<u8>, String), Motif>,
    cat: &ClassCatalogue,
) -> Option<Motif> {
    match expr {
        Expression::Parenthesized(p) => {
            build_rhs_motif(p.expression, var_motifs, field_motifs, cat)
        }
        Expression::Literal(lit) => literal_motif(lit),
        Expression::Variable(Variable::Direct(v)) => {
            let key = strip_dollar(v.name);
            var_motifs.get(&key).cloned()
        }
        Expression::Binary(b) => {
            let op = arith_op(&b.operator)?;
            let a = build_rhs_motif(b.lhs, var_motifs, field_motifs, cat)?;
            let c = build_rhs_motif(b.rhs, var_motifs, field_motifs, cat)?;
            Some(Motif::Node(op.to_string(), vec![a, c]))
        }
        // `$obj->field` projection → the matching `?obj_field` pattern variable.
        Expression::Access(Access::Property(pa)) => {
            let Expression::Variable(Variable::Direct(recv)) = pa.object else {
                return None;
            };
            let ClassLikeMemberSelector::Identifier(prop_id) = &pa.property else {
                return None;
            };
            let recv_key = strip_dollar(recv.name);
            let field = String::from_utf8_lossy(prop_id.value).into_owned();
            field_motifs.get(&(recv_key, field)).cloned()
        }
        // `new D(args)` → `(D <args>)`.
        Expression::Instantiation(inst) => {
            let class = instantiation_class_name(inst)?;
            let key = String::from_utf8_lossy(&normalize_fqcn(&class)).to_ascii_lowercase();
            cat.get(&key)?; // D must be a known class (its layout must exist)
            let args = motif_args(inst.argument_list.as_ref(), var_motifs, field_motifs, cat)?;
            Some(Motif::Node(
                String::from_utf8_lossy(&class).into_owned(),
                args,
            ))
        }
        // `D::factory(args)` → `(D::factory <args>)` (a symbol like any other; the
        // factory's own derived rule rewrites it onto `(D …)`).
        Expression::Call(Call::StaticMethod(sm)) => {
            let class = static_call_class_name(sm.class)?;
            let ClassLikeMemberSelector::Identifier(mid) = &sm.method else {
                return None;
            };
            let method = String::from_utf8_lossy(mid.value).into_owned();
            let op = format!("{}::{}", String::from_utf8_lossy(&class), method);
            let args = motif_args(Some(&sm.argument_list), var_motifs, field_motifs, cat)?;
            Some(Motif::Node(op, args))
        }
        // `$recv->method(args)` → `(method <recv> <args>)`.
        Expression::Call(Call::Method(mc)) => {
            let ClassLikeMemberSelector::Identifier(mid) = &mc.method else {
                return None;
            };
            let method = String::from_utf8_lossy(mid.value).into_owned();
            let recv = build_rhs_motif(mc.object, var_motifs, field_motifs, cat)?;
            let mut kids = vec![recv];
            kids.extend(motif_args(
                Some(&mc.argument_list),
                var_motifs,
                field_motifs,
                cat,
            )?);
            Some(Motif::Node(method, kids))
        }
        _ => None,
    }
}

fn motif_args(
    args: Option<&ArgumentList>,
    var_motifs: &HashMap<Vec<u8>, Motif>,
    field_motifs: &HashMap<(Vec<u8>, String), Motif>,
    cat: &ClassCatalogue,
) -> Option<Vec<Motif>> {
    let Some(args) = args else {
        return Some(Vec::new());
    };
    let mut out = Vec::new();
    for arg in args.arguments.iter() {
        match arg {
            Argument::Positional(p) => {
                if p.ellipsis.is_some() {
                    return None;
                }
                out.push(build_rhs_motif(p.value, var_motifs, field_motifs, cat)?);
            }
            Argument::Named(_) => return None,
        }
    }
    Some(out)
}

/// A literal → a motif. Integers become decimal-string leaf ops (so `GroundEval`
/// re-reads them); other literals leave the modelled fragment.
/// The leaf op-string for a SCALAR literal, byte-for-byte IDENTICAL to `literal_node`'s
/// encoding (and `value_leaf`'s) so a decision-path literal, a derived-rule literal, and a
/// compression-/provider-substituted leaf all land in ONE e-class. `None` for a dynamic or
/// overflowing literal (fail-closed). This is what opens the automaton past the integer
/// fragment: a string/bool/float/null Given flows through congruence exactly like an int.
fn literal_op(lit: &Literal) -> Option<String> {
    match lit {
        Literal::Integer(i) => i.value.map(|v| (v as i64).to_string()),
        Literal::Float(f) => Some(format!("float:{}", f.value)),
        Literal::String(s) => s
            .value
            .as_ref()
            .map(|bytes| format!("str:'{}'", String::from_utf8_lossy(bytes))),
        Literal::True(_) => Some("true".to_string()),
        Literal::False(_) => Some("false".to_string()),
        Literal::Null(_) => Some("null".to_string()),
    }
}

fn literal_motif(lit: &Literal) -> Option<Motif> {
    literal_op(lit).map(Motif::leaf)
}

/// The leaf op-string for a pre-computed SCALAR provider Given, IDENTICAL to `literal_op`
/// (and `value_leaf`) so a substituted provider row fuses with a source literal of the
/// same value. `None` for an array/object column — that param stays unbound for the row
/// (its use then bails the row to Unknown, fail-closed). This is what lets a PARAMETRIZED
/// string/bool value-object test decide: the row's string Given binds the param exactly as
/// a literal would.
fn phpvalue_scalar_op(value: &PhpValue) -> Option<String> {
    Some(match value {
        PhpValue::Int(i) => i.to_string(),
        PhpValue::String(s) => format!("str:'{}'", String::from_utf8_lossy(s.as_bytes())),
        PhpValue::Bool(true) => "true".to_string(),
        PhpValue::Bool(false) => "false".to_string(),
        PhpValue::Null => "null".to_string(),
        PhpValue::Float(f) => format!("float:{f}"),
        PhpValue::Array(_) => return None,
    })
}

fn arith_op(op: &BinaryOperator) -> Option<&'static str> {
    match op {
        BinaryOperator::Addition(_) => Some("+"),
        BinaryOperator::Subtraction(_) => Some("-"),
        BinaryOperator::Multiplication(_) => Some("*"),
        // `/` and `%` fold only on exact/non-zero ground operands (see `GroundEval`).
        BinaryOperator::Division(_) => Some("/"),
        BinaryOperator::Modulo(_) => Some("%"),
        _ => None,
    }
}

// ─── Extracting the test expression into the shared e-graph ────────────────────

/// Build a concrete test expression directly into the e-graph as `SymbolLang`
/// nodes (no pattern variables — a test argument has only known leaves, constructed
/// objects, factory calls and method calls). Returns the root `Id` or `None` when
/// the expression escapes the fragment.
struct ExprBuilder<'a> {
    egraph: &'a mut EGraph<PhpL, GroundEval>,
    cat: &'a ClassCatalogue,
    vars: &'a HashMap<Vec<u8>, Id>,
}

impl ExprBuilder<'_> {
    fn build(&mut self, expr: &Expression) -> Option<Id> {
        match expr {
            Expression::Parenthesized(p) => self.build(p.expression),
            Expression::Literal(lit) => {
                let m = literal_motif(lit)?;
                self.add_motif(&m)
            }
            Expression::Variable(Variable::Direct(v)) => {
                self.vars.get(&strip_dollar(v.name)).copied()
            }
            Expression::Binary(b) => {
                let op = arith_op(&b.operator)?;
                let a = self.build(b.lhs)?;
                let c = self.build(b.rhs)?;
                Some(self.egraph.add(SymbolLang::new(op, vec![a, c])))
            }
            // `new C(args)` → `(C <args>)`.
            Expression::Instantiation(inst) => {
                let class = instantiation_class_name(inst)?;
                let key = String::from_utf8_lossy(&normalize_fqcn(&class)).to_ascii_lowercase();
                self.cat.get(&key)?;
                let args = self.build_args(inst.argument_list.as_ref())?;
                Some(self.egraph.add(SymbolLang::new(
                    String::from_utf8_lossy(&class).into_owned(),
                    args,
                )))
            }
            // `C::factory(args)` → `(C::factory <args>)`.
            Expression::Call(Call::StaticMethod(sm)) => {
                let class = static_call_class_name(sm.class)?;
                let ClassLikeMemberSelector::Identifier(mid) = &sm.method else {
                    return None;
                };
                let op = format!(
                    "{}::{}",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(mid.value)
                );
                let args = self.build_args(Some(&sm.argument_list))?;
                Some(self.egraph.add(SymbolLang::new(op, args)))
            }
            // `$recv->method(args)` → `(method <recv> <args>)`.
            Expression::Call(Call::Method(mc)) => {
                let ClassLikeMemberSelector::Identifier(mid) = &mc.method else {
                    return None;
                };
                let recv = self.build(mc.object)?;
                let mut kids = vec![recv];
                kids.extend(self.build_args(Some(&mc.argument_list))?);
                Some(self.egraph.add(SymbolLang::new(
                    String::from_utf8_lossy(mid.value).into_owned(),
                    kids,
                )))
            }
            // A static-property/const or other access is out of fragment.
            _ => None,
        }
    }

    fn build_args(&mut self, args: Option<&ArgumentList>) -> Option<Vec<Id>> {
        let Some(args) = args else {
            return Some(Vec::new());
        };
        let mut out = Vec::new();
        for arg in args.arguments.iter() {
            match arg {
                Argument::Positional(p) => {
                    if p.ellipsis.is_some() {
                        return None;
                    }
                    out.push(self.build(p.value)?);
                }
                Argument::Named(_) => return None,
            }
        }
        Some(out)
    }

    /// Add a concrete (variable-free) motif to the e-graph.
    fn add_motif(&mut self, m: &Motif) -> Option<Id> {
        match m {
            Motif::Var(_) => None,
            Motif::Node(op, kids) => {
                let child_ids: Vec<Id> = kids
                    .iter()
                    .map(|k| self.add_motif(k))
                    .collect::<Option<_>>()?;
                Some(self.egraph.add(SymbolLang::new(op.clone(), child_ids)))
            }
        }
    }
}

// ─── Suite-wide COMPRESSION extraction (share, don't decide) ────────────────────
//
// The recentred invariant. `decide_test_egraph` (above) measures DECIDABILITY — does
// a test's `assertSame(L,R)` collapse `L` and `R` into one e-class. That BAILS the
// instant an operand leaves the integer-VO fragment, so a `Carbon::create(...)` or a
// `BigInteger::of(...)` test scores 0% decided.
//
// COMPRESSION is a different, weaker question that the SAME e-graph answers for free:
// how many DISTINCT sub-terms does a whole suite have, once the structurally-identical
// ones are shared? Hash-consing already collapses two identical `(Carbon::create 2024
// 1 15)` nodes — repeated across 50 tests — into ONE e-class, EVEN THOUGH the call is
// totally opaque (we cannot reduce it, only recognise it is the same computation). So
// the extractor below NEVER bails: every operator, call, access, or literal becomes a
// SymbolLang term, with anything outside the modelled fragment represented as an OPAQUE
// SHAREABLE symbol (`(Carbon::create …)`, `(prop_second <recv>)`, a string leaf, an
// unknown call `(strlen …)`). Two opaque terms fuse ONLY when syntactically identical
// (same op + same children) — which is sound: two `Carbon::create(2024,1,15)` ARE the
// same value. Nothing is fused abusively.
//
// We insert EVERY test's relevant expressions into ONE shared e-graph and report:
//   * `n_naive`          — total nodes MATERIALISED across all tests, before any
//                          sharing (counted at each `add`, pre-hash-cons);
//   * `classes_struct`   — `number_of_classes()` right after insertion (sharing by
//                          structural hash-consing alone);
//   * `classes_sat`      — `number_of_classes()` after running the DERIVED rules +
//                          ground fold (structural sharing PLUS reduction/substitution
//                          fusion).
// Ratios `n_naive/classes_struct` and `n_naive/classes_sat` are the compression.

/// The compression statistics of one suite file's shared e-graph.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompressionStats {
    /// Number of test methods whose body contributed terms.
    pub tests: usize,
    /// Total term-nodes materialised across all tests BEFORE any sharing (the count
    /// every test would have in isolation = nodes inserted pre-hash-cons).
    pub n_naive: usize,
    /// Distinct e-classes after insertion = structural (hash-cons) sharing only.
    pub classes_struct: usize,
    /// Distinct e-classes after saturation = structural sharing + rule/ground fusion.
    pub classes_sat: usize,
    /// COST-WEIGHTED, naive: Σ over every materialised node (with repetition) of its
    /// CALL-cost (1 per call/magic-getter, 0 per literal/var/arith/known-field). The
    /// total call-work if every test computed everything independently.
    pub cost_naive: usize,
    /// COST-WEIGHTED, shared: Σ over the DISTINCT (hash-consed) e-classes of their
    /// CALL-cost (each shared computation counted once). The call-work after memoising
    /// every structurally-identical computation.
    pub cost_shared: usize,
    /// The TOP cost targets: the structurally-shared call-nodes ranked by
    /// `multiplicity × cost` (= total call-work saved by memoising that one node).
    /// Aggregated/merged across files in the harness; truncated to the top entries.
    pub top_targets: Vec<CostTarget>,
}

/// One memoisation target: a structurally-shared call-node, its op (symbol), how many
/// naive insertions landed in its e-class (`mult`), and its per-evaluation `cost`.
/// `saved = (mult - 1) * cost` = the call-units removed by computing it once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CostTarget {
    pub op: String,
    pub mult: usize,
    pub cost: usize,
}

impl CostTarget {
    /// Call-units saved by memoising this node: `(mult - 1) * cost`.
    pub fn saved(&self) -> usize {
        self.mult.saturating_sub(1) * self.cost
    }
    /// Total call-work this node represents naively: `mult * cost`.
    pub fn weight(&self) -> usize {
        self.mult * self.cost
    }
}

impl CompressionStats {
    /// Structural compression ratio (`n_naive / classes_struct`); `0.0` when empty.
    pub fn ratio_struct(&self) -> f64 {
        if self.classes_struct == 0 {
            0.0
        } else {
            self.n_naive as f64 / self.classes_struct as f64
        }
    }

    /// Total compression ratio (`n_naive / classes_sat`); `0.0` when empty.
    pub fn ratio_total(&self) -> f64 {
        if self.classes_sat == 0 {
            0.0
        } else {
            self.n_naive as f64 / self.classes_sat as f64
        }
    }

    /// COST-WEIGHTED compression = `cost_naive / cost_shared`: the TIME-gain ceiling
    /// under the call-cost proxy (every call ~ one unit of PHP work). `0.0` when there
    /// is no call-work to share.
    pub fn cost_compression(&self) -> f64 {
        if self.cost_shared == 0 {
            0.0
        } else {
            self.cost_naive as f64 / self.cost_shared as f64
        }
    }
}

/// The non-bailing suite extractor: turns ANY expression into a `SymbolLang` term in a
/// shared e-graph, counting every materialised node (pre-sharing) into `n_naive`.
/// Unmodelled ops/calls/accesses become OPAQUE shareable symbols — the point is to
/// SHARE structurally-identical computations, never to decide them.
struct SuiteExtractor<'a> {
    egraph: &'a mut EGraph<PhpL, GroundEval>,
    /// Total nodes materialised, BEFORE the e-graph's hash-consing dedups them.
    n_naive: &'a mut usize,
    /// Per-insertion `(raw Id, call-cost)` ledger: ONE entry per materialised node,
    /// recorded BEFORE saturation so we can later canonicalise each Id (post-rebuild)
    /// and weight the STRUCTURAL sharing by cost + multiplicity. Cost is `1` for a
    /// call / magic-getter, `0` for a literal/var/arith/known-field. Shared so every
    /// test's nodes land in one ledger (cross-test sharing is the whole point).
    ledger: &'a mut Vec<(Id, u32)>,
    /// Per-test local bindings (`$x = <expr>`) → the expr's e-class Id, so a local
    /// resolves to the SHARED node it was assigned (cross-test sharing flows through).
    vars: HashMap<Vec<u8>, Id>,
    /// A per-test salt: a genuinely-FREE variable (a param, an undefined local) must
    /// NOT fuse across tests (test A's `$ci` is a different object from test B's), so
    /// its opaque leaf op carries this salt.
    test_salt: usize,
}

impl SuiteExtractor<'_> {
    /// Materialise one node `(op child…)` of a given `cost`, counting it into
    /// `n_naive` BEFORE the e-graph hash-conses it (so two identical nodes count twice
    /// in `n_naive` but land in one e-class — that gap IS the compression) and pushing
    /// `(raw Id, cost)` into the ledger for the later cost-weighted aggregation.
    fn node_costed(&mut self, op: impl Into<String>, kids: Vec<Id>, cost: u32) -> Id {
        *self.n_naive += 1;
        let id = self.egraph.add(SymbolLang::new(op.into(), kids));
        self.ledger.push((id, cost));
        id
    }

    /// A FREE node (`cost = 0`): an arithmetic op, a ternary, an array access, a
    /// named-arg wrapper, a unary — cheap relative to a PHP call.
    fn node(&mut self, op: impl Into<String>, kids: Vec<Id>) -> Id {
        self.node_costed(op, kids, 0)
    }

    /// A CALL node (`cost = 1`): a construction `(C …)`, a factory `(C::m …)`, an
    /// instance/nullsafe method, a free function — each ~ one unit of PHP call-work.
    fn node_call(&mut self, op: impl Into<String>, kids: Vec<Id>) -> Id {
        self.node_costed(op, kids, 1)
    }

    /// A FREE childless leaf (`cost = 0`): a literal, a free/indirect variable, a
    /// bare constant.
    fn leaf(&mut self, op: impl Into<String>) -> Id {
        self.node_costed(op, Vec::new(), 0)
    }

    /// Materialise a pre-computed data-provider Given as a CONCRETE shareable leaf,
    /// using the EXACT same op naming as `literal_node` so a provider int `0` fuses
    /// with a source `0`, a provider `'0'` with a `str:'0'`, etc. Returns `None` for a
    /// non-scalar Given (an array/object) — that column is not substitutable, the
    /// caller leaves the param salted for the row. Two leaves fuse iff syntactically
    /// identical (same op), so sharing here is sound by construction.
    fn value_leaf(&mut self, value: &PhpValue) -> Option<Id> {
        let op = match value {
            PhpValue::Int(i) => i.to_string(),
            PhpValue::String(s) => {
                let text = String::from_utf8_lossy(s.as_bytes());
                format!("str:'{text}'")
            }
            PhpValue::Bool(true) => "true".to_string(),
            PhpValue::Bool(false) => "false".to_string(),
            PhpValue::Null => "null".to_string(),
            // A provider float Given: a numeric leaf namespaced exactly like
            // `literal_node`'s float case, formatted from the f64 so two rows carrying
            // the same provider float share. (Matching a SOURCE float literal's text is
            // not guaranteed — only identical strings fuse — but that is sound.)
            PhpValue::Float(f) => format!("float:{f}"),
            // A concrete array Given (e.g. a data-provider matrix `[[1,2],[3,4]]` or a
            // coordinate list) → a STRUCTURAL leaf `(array_lit <k0> <v0> <k1> <v1> …)`:
            // keys and values materialised recursively, so two syntactically-identical
            // arrays (same keys, values, order) fuse into ONE e-class. That is the
            // cross-row / cross-test sharing array providers carry, previously missed
            // (the column was left as a free salted param). Any non-substitutable element
            // fails the WHOLE array (fail-closed → param stays salted, no false fusion).
            PhpValue::Array(map) => {
                let mut kids = Vec::with_capacity(map.len() * 2);
                for (k, v) in map.iter() {
                    let key_op = match k {
                        ArrayKey::Int(i) => format!("k:{i}"),
                        ArrayKey::String(s) => format!("k:'{s}'"),
                    };
                    let key_id = self.leaf(key_op);
                    let val_id = self.value_leaf(v)?;
                    kids.push(key_id);
                    kids.push(val_id);
                }
                // An array literal is free DATA, not a call: cost 0.
                return Some(self.node("array_lit", kids));
            }
        };
        Some(self.leaf(op))
    }

    /// Build ANY expression into the shared e-graph as a `SymbolLang` term. NEVER
    /// returns `None`: an unmodelled construct becomes an opaque shareable symbol.
    fn build(&mut self, expr: &Expression) -> Id {
        match expr {
            Expression::Parenthesized(p) => self.build(p.expression),
            Expression::Literal(lit) => self.literal_node(lit),
            Expression::Variable(v) => self.variable_node(v),
            Expression::Binary(b) => {
                // A modelled arithmetic op keeps its `{+,-,*,/,%}` symbol (so ground
                // fold + derived rules can fire); any other binary op is opaque but
                // still shareable under its own operator name.
                let op = arith_op(&b.operator)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("binop:{}", binop_name(&b.operator)));
                let a = self.build(b.lhs);
                let c = self.build(b.rhs);
                self.node(op, vec![a, c])
            }
            Expression::UnaryPrefix(u) => {
                let k = self.build(u.operand);
                self.node("unary_prefix", vec![k])
            }
            Expression::UnaryPostfix(u) => {
                let k = self.build(u.operand);
                self.node("unary_postfix", vec![k])
            }
            // `new C(args)`: if `C` is a CONSTRUCTIBLE catalogue class we reuse the
            // sound `(C args)` shape (so it can share with a factory's rewrite RHS);
            // otherwise an opaque `(new\C args)` that still shares with itself.
            Expression::Instantiation(inst) => {
                let args = self.build_args(inst.argument_list.as_ref());
                let op = match instantiation_class_name(inst) {
                    Some(class) => String::from_utf8_lossy(&class).into_owned(),
                    None => "new\\<dynamic>".to_string(),
                };
                // A construction is a CALL (the ctor runs): cost 1.
                self.node_call(op, args)
            }
            Expression::Call(call) => self.call_node(call),
            Expression::Access(access) => self.access_node(access),
            Expression::ArrayAccess(aa) => {
                let base = self.build(aa.array);
                let idx = self.build(aa.index);
                self.node("array_access", vec![base, idx])
            }
            Expression::Conditional(c) => {
                let cond = self.build(c.condition);
                let then_id = match c.then {
                    Some(t) => self.build(t),
                    None => self.leaf("<elvis>"),
                };
                let else_id = self.build(c.r#else);
                self.node("ternary", vec![cond, then_id, else_id])
            }
            Expression::Identifier(_) | Expression::ConstantAccess(_) => {
                // A bare constant / global identifier (`PHP_INT_MAX`, a function name
                // in callable position): an opaque leaf keyed by its text, so the same
                // constant shares everywhere.
                let name = expression_text_key(expr);
                self.leaf(format!("const:{name}"))
            }
            // Anything else (closures, match, array literals, throw, clone, …) is a
            // single opaque leaf keyed by its discriminant — still shareable when two
            // tests write the exact same construct, never fused with a different one.
            other => self.leaf(format!("opaque:{}", expr_discriminant(other))),
        }
    }

    fn literal_node(&mut self, lit: &Literal) -> Id {
        match lit {
            // An integer keeps its decimal-string op, so `GroundEval` re-reads it and
            // numeric folds still fire across the shared graph.
            Literal::Integer(i) => match i.value {
                Some(v) => self.leaf((v as i64).to_string()),
                None => self.leaf("int:<overflow>"),
            },
            Literal::Float(f) => self.leaf(format!("float:{}", f.value)),
            // String literals: a leaf keyed by content (single-quoted to namespace it
            // away from integer leaves), so identical strings across tests share.
            Literal::String(s) => match s.value.as_ref() {
                Some(bytes) => {
                    let text = String::from_utf8_lossy(bytes);
                    self.leaf(format!("str:'{text}'"))
                }
                None => self.leaf("str:<dynamic>"),
            },
            Literal::True(_) => self.leaf("true"),
            Literal::False(_) => self.leaf("false"),
            Literal::Null(_) => self.leaf("null"),
        }
    }

    fn variable_node(&mut self, v: &Variable) -> Id {
        match v {
            Variable::Direct(d) => {
                let name = strip_dollar(d.name);
                // A bound local resolves to the SHARED node it was assigned. A free
                // variable (param / undefined) gets a per-test-salted opaque leaf so
                // it never fuses with a same-named variable in a DIFFERENT test.
                if let Some(id) = self.vars.get(&name) {
                    *id
                } else {
                    let nm = String::from_utf8_lossy(&name).into_owned();
                    self.leaf(format!("$free:{nm}#{}", self.test_salt))
                }
            }
            // `$$x` / `${…}` indirect/nested: an opaque per-test leaf (cannot model).
            _ => {
                let salt = self.test_salt;
                self.leaf(format!("$indirect#{salt}"))
            }
        }
    }

    fn call_node(&mut self, call: &Call) -> Id {
        match call {
            // `C::method(args)` → `(C::method args)` (matches the decision extractor's
            // factory op, so a factory's derived rule can rewrite it onto `(C …)`).
            Call::StaticMethod(sm) => {
                let directive = matches!(
                    &sm.method,
                    ClassLikeMemberSelector::Identifier(mid) if is_test_directive(mid.value)
                );
                let op = match (static_call_class_name(sm.class), &sm.method) {
                    (Some(class), ClassLikeMemberSelector::Identifier(mid)) => format!(
                        "{}::{}",
                        String::from_utf8_lossy(&class),
                        String::from_utf8_lossy(mid.value)
                    ),
                    _ => "static_call:<dynamic>".to_string(),
                };
                let args = self.build_args(Some(&sm.argument_list));
                // A static test DIRECTIVE (`self::assertX` / `static::assertX`) is a
                // per-test sink: cost 0. A static factory / static method is a CALL: cost 1.
                if directive {
                    self.node(op, args)
                } else {
                    self.node_call(op, args)
                }
            }
            // `$recv->method(args)` → `(method <recv> args)` (instance-method op, recv
            // first child — same shape the decision extractor emits).
            Call::Method(mc) => {
                let recv = self.build(mc.object);
                let on_this = matches!(
                    mc.object,
                    Expression::Variable(Variable::Direct(v)) if strip_dollar(v.name) == b"this"
                );
                let (op, directive) = match &mc.method {
                    ClassLikeMemberSelector::Identifier(mid) => (
                        String::from_utf8_lossy(mid.value).into_owned(),
                        on_this && is_test_directive(mid.value),
                    ),
                    _ => ("method:<dynamic>".to_string(), false),
                };
                let mut kids = vec![recv];
                kids.extend(self.build_args(Some(&mc.argument_list)));
                // A test DIRECTIVE on `$this` (`$this->assertX` / `$this->expectException`)
                // is a per-test sink: cost 0, never a target. Any other instance method is
                // a CALL: cost 1.
                if directive {
                    self.node(op, kids)
                } else {
                    self.node_call(op, kids)
                }
            }
            Call::NullSafeMethod(mc) => {
                let recv = self.build(mc.object);
                let op = match &mc.method {
                    ClassLikeMemberSelector::Identifier(mid) => {
                        format!("?->{}", String::from_utf8_lossy(mid.value))
                    }
                    _ => "nullsafe_method:<dynamic>".to_string(),
                };
                let mut kids = vec![recv];
                kids.extend(self.build_args(Some(&mc.argument_list)));
                // A null-safe instance method is a CALL: cost 1.
                self.node_call(op, kids)
            }
            // A free function `f(args)` → `(f args)` (opaque, but shareable per name).
            Call::Function(fc) => {
                let op = match function_call_name(fc.function) {
                    Some(name) => format!("fn:{}", String::from_utf8_lossy(&name)),
                    None => "fn:<dynamic>".to_string(),
                };
                let args = self.build_args(Some(&fc.argument_list));
                // A free function is a CALL: cost 1.
                self.node_call(op, args)
            }
        }
    }

    fn access_node(&mut self, access: &Access) -> Id {
        match access {
            // `$o->field` → `(prop_field <recv>)`: the property name is BAKED INTO the
            // op so two reads of the SAME property on the SAME receiver share one node.
            Access::Property(pa) => {
                let recv = self.build(pa.object);
                let op = match &pa.property {
                    ClassLikeMemberSelector::Identifier(pid) => {
                        format!("prop_{}", String::from_utf8_lossy(pid.value))
                    }
                    _ => "prop:<dynamic>".to_string(),
                };
                // A property read on an opaque receiver IS a magic `__get` CALL in PHP:
                // cost 1. (Structurally we cannot know the receiver is a constructible,
                // statically-known field — that distinction lives in saturation; at the
                // struct level every `$o->x` is treated as a getter call.)
                self.node_call(op, vec![recv])
            }
            Access::NullSafeProperty(pa) => {
                let recv = self.build(pa.object);
                let op = match &pa.property {
                    ClassLikeMemberSelector::Identifier(pid) => {
                        format!("?->prop_{}", String::from_utf8_lossy(pid.value))
                    }
                    _ => "nullsafe_prop:<dynamic>".to_string(),
                };
                // Null-safe property read = magic `__get` CALL: cost 1.
                self.node_call(op, vec![recv])
            }
            // `C::CONST` / `C::$static` → an opaque leaf keyed by its text.
            Access::ClassConstant(cc) => {
                let class = identifier_class_name(cc.class)
                    .map(|c| String::from_utf8_lossy(&c).into_owned())
                    .unwrap_or_else(|| "<dynamic>".to_string());
                use mago_syntax::ast::ast::class_like::member::ClassLikeConstantSelector;
                let member = match &cc.constant {
                    ClassLikeConstantSelector::Identifier(id) => {
                        String::from_utf8_lossy(id.value).into_owned()
                    }
                    _ => "<dynamic>".to_string(),
                };
                self.leaf(format!("const:{class}::{member}"))
            }
            Access::StaticProperty(sp) => {
                let class = identifier_class_name(sp.class)
                    .map(|c| String::from_utf8_lossy(&c).into_owned())
                    .unwrap_or_else(|| "<dynamic>".to_string());
                self.leaf(format!("staticprop:{class}"))
            }
        }
    }

    /// Build a call's argument list into child Ids (a spread/named arg becomes its own
    /// opaque leaf so the term stays total — nothing bails).
    fn build_args(&mut self, args: Option<&ArgumentList>) -> Vec<Id> {
        let Some(args) = args else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for arg in args.arguments.iter() {
            match arg {
                Argument::Positional(p) => out.push(self.build(p.value)),
                Argument::Named(n) => {
                    let v = self.build(n.value);
                    out.push(self.node("named_arg", vec![v]));
                }
            }
        }
        out
    }

    /// Extract every relevant sub-term of ONE test method body into the shared graph,
    /// binding its `$x = <expr>` locals first so later uses resolve to the shared node.
    /// Returns `true` if the body contributed any term. NEVER bails.
    fn extract_test_body(&mut self, m: &Method) -> bool {
        let MethodBody::Concrete(block) = &m.body else {
            return false;
        };
        let mut contributed = false;
        for stmt in block.statements.iter() {
            contributed |= self.extract_statement(stmt);
        }
        contributed
    }

    fn extract_statement(&mut self, stmt: &Statement) -> bool {
        match stmt {
            Statement::Expression(es) => {
                // A `$x = <expr>;` assignment binds the local to the rhs's SHARED node.
                if let Expression::Assignment(a) = es.expression {
                    use mago_syntax::ast::ast::assignment::AssignmentOperator;
                    if matches!(a.operator, AssignmentOperator::Assign(_)) {
                        if let Expression::Variable(Variable::Direct(t)) = a.lhs {
                            let id = self.build(a.rhs);
                            self.vars.insert(strip_dollar(t.name), id);
                            return true;
                        }
                    }
                }
                self.build(es.expression);
                true
            }
            Statement::Return(ret) => {
                if let Some(e) = ret.value {
                    self.build(e);
                    return true;
                }
                false
            }
            _ => false,
        }
    }
}

/// The discriminant tag of a binary operator, for the opaque `binop:<tag>` op name of a
/// non-arithmetic binary (so `&&`, `===`, `.`, … each share under their own symbol).
fn binop_name(op: &BinaryOperator) -> &'static str {
    use BinaryOperator as B;
    match op {
        B::Addition(_) => "+",
        B::Subtraction(_) => "-",
        B::Multiplication(_) => "*",
        B::Division(_) => "/",
        B::Modulo(_) => "%",
        B::Exponentiation(_) => "**",
        B::And(_) => "&&",
        B::Or(_) => "||",
        B::LowAnd(_) => "and",
        B::LowOr(_) => "or",
        B::LowXor(_) => "xor",
        B::Equal(_) => "==",
        B::NotEqual(_) => "!=",
        B::Identical(_) => "===",
        B::NotIdentical(_) => "!==",
        B::AngledNotEqual(_) => "<>",
        B::LessThan(_) => "<",
        B::LessThanOrEqual(_) => "<=",
        B::GreaterThan(_) => ">",
        B::GreaterThanOrEqual(_) => ">=",
        B::Spaceship(_) => "<=>",
        B::StringConcat(_) => ".",
        B::BitwiseAnd(_) => "&",
        B::BitwiseOr(_) => "|",
        B::BitwiseXor(_) => "^",
        B::LeftShift(_) => "<<",
        B::RightShift(_) => ">>",
        B::NullCoalesce(_) => "??",
        B::Instanceof(_) => "instanceof",
    }
}

/// A short discriminant tag for the `opaque:<tag>` leaf of an un-handled expression
/// variant (so two tests writing the same kind of construct share their leaf, and two
/// DIFFERENT kinds never do).
fn expr_discriminant(expr: &Expression) -> &'static str {
    match expr {
        Expression::Array(_) | Expression::LegacyArray(_) => "array",
        Expression::List(_) => "list",
        Expression::Closure(_) => "closure",
        Expression::ArrowFunction(_) => "arrow_fn",
        Expression::AnonymousClass(_) => "anon_class",
        Expression::Match(_) => "match",
        Expression::Yield(_) => "yield",
        Expression::Throw(_) => "throw",
        Expression::Clone(_) => "clone",
        Expression::CompositeString(_) => "interp_string",
        Expression::Construct(_) => "lang_construct",
        Expression::MagicConstant(_) => "magic_const",
        Expression::ArrayAppend(_) => "array_append",
        Expression::Assignment(_) => "assignment",
        Expression::Pipe(_) => "pipe",
        Expression::Parent(_) => "parent",
        Expression::Static(_) => "static",
        _ => "other",
    }
}

/// A best-effort text key for a bare identifier / constant-access expression (used to
/// share `PHP_INT_MAX` and friends across tests). Falls back to a generic tag.
fn expression_text_key(expr: &Expression) -> String {
    if let Expression::Identifier(id) = expr {
        use mago_syntax::ast::ast::identifier::Identifier;
        let bytes = match id {
            Identifier::Local(l) => l.value,
            Identifier::Qualified(q) => q.value,
            Identifier::FullyQualified(f) => f.value,
        };
        return String::from_utf8_lossy(bytes).into_owned();
    }
    if let Expression::ConstantAccess(_) = expr {
        return "<const_access>".to_string();
    }
    "<ident>".to_string()
}

/// The name of a free-function call target, if it is a plain identifier.
fn function_call_name(function: &Expression) -> Option<Vec<u8>> {
    use mago_syntax::ast::ast::identifier::Identifier;
    let Expression::Identifier(id) = function else {
        return None;
    };
    Some(match id {
        Identifier::Local(l) => l.value.to_vec(),
        Identifier::Qualified(q) => q.value.to_vec(),
        Identifier::FullyQualified(f) => f.value.to_vec(),
    })
}

/// Build the SUITE-WIDE compression e-graph for one test file: insert EVERY test
/// method's relevant expressions into ONE shared e-graph (sharing structurally-
/// identical sub-terms by hash-consing), then saturate with the rules DERIVED from the
/// closure of classes those tests reference. Returns the compression statistics.
///
/// `test_methods` is the list of `(declaring-class-fqcn, &Method)` to extract — the
/// caller (the harness) discovers them through the production machinery. The closure
/// rules are derived once, seeded by every class every test references.
///
/// `program` / `source_text` are the (already-reparsed) test file's AST and bytes,
/// used to resolve each parametrized method's DATA-PROVIDER rows. A provider row is a
/// tuple of pre-computed Givens (Rust evaluates the provider at discovery): for every
/// row we materialise the test body ONCE with each parameter BOUND to its concrete
/// literal leaf — so two rows (within a method or across methods) that build the same
/// `(BigInteger::of str:'0')` share ONE e-class. A column that is not a substitutable
/// literal (an object, an array, a `yield`/computed row) keeps the param salted for
/// that row (fail-safe); a method with no derivable provider extracts once as before.
pub fn build_suite_egraph(
    project: &MagoProject,
    program: &Program,
    source_text: &str,
    test_methods: &[(Vec<u8>, &Method)],
) -> CompressionStats {
    // Derive the rules once, from the union of every test's referenced-class closure.
    let mut seeds: Vec<Vec<u8>> = Vec::new();
    for (class_fqcn, m) in test_methods {
        seeds.extend(seed_class_refs(m, class_fqcn));
    }
    let (rules, _cat, _descs) = derive_closure(project, seeds);

    let mut egraph: EGraph<PhpL, GroundEval> = EGraph::default();
    let mut n_naive = 0usize;
    let mut ledger: Vec<(Id, u32)> = Vec::new();
    let mut tests = 0usize;

    for (salt, (class_fqcn, m)) in test_methods.iter().enumerate() {
        // The substitution rows for this method. `None` → no provider (or a zero-param
        // method): extract the body ONCE with free params salted (the prior behaviour).
        // `Some(rows)` → materialise the body once PER ROW, binding each param to its
        // pre-computed literal leaf (cross-row sharing is the whole point).
        let rows = provider_value_rows(program, source_text, class_fqcn, m);
        let params: Vec<Vec<u8>> = m
            .parameter_list
            .parameters
            .iter()
            .map(|p| strip_dollar(p.variable.name))
            .collect();

        let mut contributed = false;
        match rows {
            Some(rows) if !params.is_empty() => {
                for row in &rows {
                    let mut ex = SuiteExtractor {
                        egraph: &mut egraph,
                        n_naive: &mut n_naive,
                        ledger: &mut ledger,
                        vars: HashMap::new(),
                        test_salt: salt,
                    };
                    // Pre-bind each parameter to the row's concrete Given. A column
                    // that is absent or not a substitutable literal stays UNBOUND, so
                    // `variable_node` falls back to the per-test salted free leaf for
                    // that param on this row (no false fusion).
                    for (i, pname) in params.iter().enumerate() {
                        if let Some(Some(value)) = row.get(i) {
                            if let Some(id) = ex.value_leaf(value) {
                                ex.vars.insert(pname.clone(), id);
                            }
                        }
                    }
                    contributed |= ex.extract_test_body(m);
                }
            }
            _ => {
                let mut ex = SuiteExtractor {
                    egraph: &mut egraph,
                    n_naive: &mut n_naive,
                    ledger: &mut ledger,
                    vars: HashMap::new(),
                    test_salt: salt,
                };
                contributed = ex.extract_test_body(m);
            }
        }
        if contributed {
            tests += 1;
        }
    }

    egraph.rebuild();
    let classes_struct = egraph.number_of_classes();

    // COST-WEIGHTED metric at the STRUCTURAL-sharing level. Canonicalise each insertion's
    // raw Id (post-rebuild it may have been hash-cons-merged into a representative class),
    // then fold the ledger into per-class `(multiplicity, cost)`. Every node sharing a
    // structural class has the SAME op, hence the SAME cost, so `max` is just a robust
    // reduce; the canonical op name comes from that class's first node.
    let mut per_class: HashMap<Id, (usize, u32)> = HashMap::new();
    for (raw_id, cost) in &ledger {
        let canon = egraph.find(*raw_id);
        let entry = per_class.entry(canon).or_insert((0, 0));
        entry.0 += 1; // multiplicity: one more naive insertion landed here
        entry.1 = entry.1.max(*cost);
    }

    // cost_naive = Σ over every insertion (with repetition) of its cost = Σ mult*cost.
    // cost_shared = Σ over the DISTINCT classes of cost (each shared compute counted once).
    let cost_naive: usize = per_class
        .values()
        .map(|(mult, c)| mult * (*c as usize))
        .sum();
    let cost_shared: usize = per_class.values().map(|(_, c)| *c as usize).sum();

    // TOP cost targets: the call-classes ranked by `mult * cost` (= total call-work this
    // shared node represents). The op label is read from one representative node of the
    // class. Only cost>0 classes (actual calls) are memoisation candidates.
    let mut top: Vec<CostTarget> = per_class
        .iter()
        .filter(|(_, (_, cost))| *cost > 0)
        .map(|(id, (mult, cost))| {
            let op = egraph[*id]
                .nodes
                .first()
                .map(|n| n.op.to_string())
                .unwrap_or_default();
            CostTarget {
                op,
                mult: *mult,
                cost: *cost as usize,
            }
        })
        .collect();
    top.sort_by(|a, b| b.weight().cmp(&a.weight()).then(b.mult.cmp(&a.mult)));
    top.truncate(TOP_TARGETS);

    let runner = Runner::default().with_egraph(egraph).run(&rules);
    let egraph = runner.egraph;
    let classes_sat = egraph.number_of_classes();

    CompressionStats {
        tests,
        n_naive,
        classes_struct,
        classes_sat,
        cost_naive,
        cost_shared,
        top_targets: top,
    }
}

/// How many memoisation targets the harness keeps/prints per file.
const TOP_TARGETS: usize = 10;

// ─── Shared AST helpers ────────────────────────────────────────────────────────

fn instantiation_class_name(inst: &Instantiation) -> Option<Vec<u8>> {
    identifier_class_name(inst.class)
}

fn static_call_class_name(class: &Expression) -> Option<Vec<u8>> {
    identifier_class_name(class)
}

/// PHPUnit test DIRECTIVES — assertions, expectation setters, and skip/fail markers.
/// They are per-test SINKS: they register with the framework and run once per test, and
/// produce NO shareable value. In the cost model they are weighted `0` (not call-work)
/// and are therefore never memoisation targets — only their value ARGUMENTS are. Inferred
/// purely from the AST by the `assert*`/`expect*` method-name shape plus a few markers.
fn is_test_directive(name: &[u8]) -> bool {
    name.starts_with(b"assert")
        || name.starts_with(b"expect")
        || matches!(
            name,
            b"fail" | b"markTestSkipped" | b"markTestIncomplete" | b"addToAssertionCount"
        )
}

/// The simple/qualified class name of a class-position expression, if it is a plain
/// identifier (not `self`/`static`/`parent`, not `new $var`).
fn identifier_class_name(expr: &Expression) -> Option<Vec<u8>> {
    use mago_syntax::ast::ast::identifier::Identifier;
    if let Expression::Identifier(id) = expr {
        return Some(match id {
            Identifier::Local(l) => l.value.to_vec(),
            Identifier::Qualified(q) => q.value.to_vec(),
            Identifier::FullyQualified(f) => f.value.to_vec(),
        });
    }
    None
}

/// If a block is EXACTLY one `return <expr>;`, the returned expression; else `None`
/// (the strict single-return purity gate; a mutator / multi-statement body has no
/// derived rule and stays opaque).
fn single_return_expr<'a>(
    block: &'a mago_syntax::ast::ast::block::Block<'a>,
) -> Option<&'a Expression<'a>> {
    let stmts: Vec<&Statement> = block.statements.iter().collect();
    if stmts.len() != 1 {
        return None;
    }
    if let Statement::Return(ret) = stmts[0] {
        return ret.value;
    }
    None
}

// ─── Data-provider substitution: rows of literal leaves ─────────────────────────
//
// Both the DECISION and the COMPRESSION paths bind a parametrized test's params from a
// statically-evaluated provider, each column a concrete `PhpValue` literal Given (int /
// string / bool / float / null → a leaf; a non-literal array/object/computed column →
// `None`, that param staying unbound/salted for the row). A method with no derivable
// provider returns `None` (no rows).

/// The data-provider rows for a parametrized test method, evaluated as
/// per-column literal Givens for the COMPRESSION substitution. Returns:
///   * `None` — the method has NO derivable provider (no `#[DataProvider]` /
///     `#[TestWith]` / `@dataProvider`, or the named provider is missing / not a pure
///     array literal). The caller then extracts the body once with free params salted.
///   * `Some(rows)` — one entry per provider row; each row is a `Vec<Option<PhpValue>>`
///     positionally aligned to the method's parameters. `Some(v)` = a substitutable
///     literal Given for that column; `None` = a non-literal column (array/object/yield/
///     computed) → the caller leaves that one param salted for the row (fail-safe).
fn provider_value_rows(
    program: &Program,
    source_text: &str,
    class_fqcn: &[u8],
    method: &Method,
) -> Option<Vec<Vec<Option<PhpValue>>>> {
    // `#[TestWith([..])]` rows live ON the method — collect them first.
    let test_with = test_with_value_rows(method);
    if !test_with.is_empty() {
        return Some(test_with);
    }
    // Otherwise a `#[DataProvider('name')]` attribute or `@dataProvider name` docblock
    // names a sibling static provider method.
    let provider_name = data_provider_name(source_text, method)?;
    let provider = find_class_method(program, class_fqcn, provider_name.as_bytes())?;
    static_provider_value_rows(provider)
}

/// The `#[TestWith([..])]` rows declared directly on `method`, each evaluated to a row
/// of per-column literal Givens (non-literal columns → `None`). A `TestWith` whose
/// argument does not concretely evaluate to an array contributes no row.
fn test_with_value_rows(method: &Method) -> Vec<Vec<Option<PhpValue>>> {
    let mut rows = Vec::new();
    for list in method.attribute_lists.iter() {
        for attr in list.attributes.iter() {
            if !attribute_simple_name(&attr.name).eq_ignore_ascii_case(b"TestWith") {
                continue;
            }
            let Some(arg_list) = &attr.argument_list else {
                continue;
            };
            let Some(Argument::Positional(p)) = arg_list.arguments.iter().next() else {
                continue;
            };
            if let Some(row) = value_row_from_expr(p.value) {
                rows.push(row);
            }
        }
    }
    rows
}

/// Evaluate a static data-provider method (`{ return [ [..], [..] ]; }`) to rows of
/// per-column literal Givens. The body must be a single `return` of an `array`
/// LITERAL whose every element is itself an array (positional or `key => [..]`); a
/// non-array element fails the whole provider (`None`). Anything that does not compute
/// statically (a `yield`, a loop, a computed expression) also fails — the method then
/// keeps its params salted (no substitution, no false fusion).
fn static_provider_value_rows(provider: &Method) -> Option<Vec<Vec<Option<PhpValue>>>> {
    let MethodBody::Concrete(block) = &provider.body else {
        return None;
    };
    let ret = single_return_expr(block)?;
    value_rows_from_array_literal(ret)
}

/// A `[ [..], [..] ]` (or `[ 'k' => [..] ]`) outer array literal → its rows, each a
/// per-column literal Given vector. `None` if `expr` does not compute to an array, or
/// any row is not itself an array.
fn value_rows_from_array_literal(expr: &Expression) -> Option<Vec<Vec<Option<PhpValue>>>> {
    let mut ctx = Context::new();
    let value = compute(expr, &mut ctx).ok()?;
    let PhpValue::Array(outer) = value else {
        return None;
    };
    let mut rows = Vec::new();
    for (_key, row_val) in outer {
        rows.push(value_row_from_phpvalue(&row_val)?);
    }
    Some(rows)
}

/// One row literal (`[1, 'x', true]`) → its per-column Givens. `None` if `expr` does
/// not compute to an array.
fn value_row_from_expr(expr: &Expression) -> Option<Vec<Option<PhpValue>>> {
    let mut ctx = Context::new();
    let value = compute(expr, &mut ctx).ok()?;
    value_row_from_phpvalue(&value)
}

/// A concretely-evaluated `PhpValue` row → its per-column Givens. Each scalar column
/// (int / string / bool / float / null) is a substitutable `Some(value)`; a nested
/// array (or any non-scalar) column is `None` (not a single shareable leaf — the
/// param stays salted). `None` only when the row itself is not an array.
fn value_row_from_phpvalue(value: &PhpValue) -> Option<Vec<Option<PhpValue>>> {
    let PhpValue::Array(map) = value else {
        return None;
    };
    // Every column is offered for substitution — scalars AND arrays (matrices, lists).
    // `value_leaf` materialises arrays structurally; a column it cannot represent returns
    // `None` there and the param stays salted for the row.
    let cols = map.values().map(|v| Some(v.clone())).collect();
    Some(cols)
}

/// The data-provider method name bound to `method`: from a `#[DataProvider('name')]`
/// attribute (its first string-literal argument) or, failing that, a legacy
/// `/** @dataProvider name */` docblock immediately above the method. `None` if
/// neither is present (or the provider is external / dynamic — unmodelled).
fn data_provider_name(source_text: &str, method: &Method) -> Option<String> {
    use mago_span::HasSpan;
    for list in method.attribute_lists.iter() {
        for attr in list.attributes.iter() {
            if !attribute_simple_name(&attr.name).eq_ignore_ascii_case(b"DataProvider") {
                continue;
            }
            let arg_list = attr.argument_list.as_ref()?;
            let Some(Argument::Positional(p)) = arg_list.arguments.iter().next() else {
                continue;
            };
            if let Some(name) = string_literal_value(p.value) {
                return Some(name);
            }
        }
    }
    // Legacy docblock fallback.
    let offset = method.span().start.offset as usize;
    doc_data_provider_name(source_text, offset)
}

/// The literal value of a `'name'` / `"name"` string argument, if `expr` is one.
fn string_literal_value(expr: &Expression) -> Option<String> {
    let Expression::Literal(Literal::String(s)) = expr else {
        return None;
    };
    s.value
        .as_ref()
        .map(|v| String::from_utf8_lossy(v).into_owned())
}

/// Scan the ≤400-byte window ending at the method declaration for a
/// `@dataProvider <name>` docblock tag, returning `<name>`. Mirrors discovery's
/// docblock scanner (byte-offset-safe slicing).
fn doc_data_provider_name(source_text: &str, method_offset: usize) -> Option<String> {
    let end = floor_char_boundary(source_text, method_offset.min(source_text.len()));
    let window_start = floor_char_boundary(source_text, end.saturating_sub(400));
    let window = &source_text[window_start..end];
    // The docblock must be the one IMMEDIATELY above the method: take the last `/**`.
    let open = window.rfind("/**")?;
    let docblock = &window[open..];
    for line in docblock.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(pos) = lower.find("@dataprovider") {
            let after = &line[pos + "@dataprovider".len()..];
            let name = after.split_whitespace().next()?;
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Floor `index` to a UTF-8 char boundary so slicing a byte offset that may land
/// mid-codepoint (real suites have multibyte docblocks) cannot panic.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The simple (namespace-stripped) name of an attribute identifier.
fn attribute_simple_name<'a>(
    name: &'a mago_syntax::ast::ast::identifier::Identifier<'a>,
) -> &'a [u8] {
    use mago_syntax::ast::ast::identifier::Identifier;
    let full = match name {
        Identifier::Local(l) => l.value,
        Identifier::Qualified(q) => q.value,
        Identifier::FullyQualified(f) => f.value,
    };
    full.rsplit(|b| *b == b'\\').next().unwrap_or(full)
}

// ─── Cross-file rule derivation: resolve the VO classes the test references ─────
//
// THE WALL v4 crosses. v1–v3 derived rules and field layouts from the SINGLE program
// of the TEST file. But every PSR-4 library puts its value objects in `src/` and its
// tests in `tests/` — separate files. So a test `new Frac(…)` / `Frac::of(…)` where
// `Frac` lives in `src/Frac.php` derived NO rule for `Frac` (it is absent from the
// test program) → Unknown by construction, on 100% of real code.
//
// v4 computes the CLOSURE of classes the test references and derives rules + field
// layouts from THEIR OWN files, resolved exactly the way `subst.rs` resolves a method
// body: codex `get_class_like` → `file_of_span` → `with_program` (reparse). mago 1.30
// keeps no AST around, so each reparsed arena drops at the end of its closure — every
// derived datum (a `FieldLayout = Vec<String>`, an owned `Rewrite`) is extracted while
// the arena is live. The closure is bounded by a `visited` FQCN set and a depth cap.

/// The cap on how many distinct classes the cross-file closure will resolve, mirroring
/// `subst.rs`'s `max_depth = 64`. A reference chain longer than this bails (the rules
/// derived so far still stand — extra classes only ADD rules, and a missing rule is
/// fail-closed Unknown, never a wrong verdict).
const MAX_CLOSURE_CLASSES: usize = 64;

/// Build the cross-file catalogue (field layouts) AND rule set for the closure of
/// classes the test method references. `seed_fqcns` are the classes named directly by
/// the test body; the closure follows each derivable method/ctor body's own class
/// references transitively. Resolution is codex → file → reparse, per class.
///
/// Returns `(rules, catalogue, descriptions)` — all fully owned, so they outlive every
/// reparsed arena. A class the codex cannot resolve, or whose file will not reparse,
/// simply contributes nothing (fail-closed): a method that needs its layout then fails
/// to derive and stays opaque. `descriptions` are the human-readable `(lhs) => (rhs)`
/// of each derived rule (for white-box tests / diagnostics).
fn derive_closure(
    project: &MagoProject,
    seed_fqcns: Vec<Vec<u8>>,
) -> (Vec<Rewrite<PhpL, GroundEval>>, ClassCatalogue, Vec<String>) {
    // Phase A — catalogue closure. A worklist of FQCNs; for each we reparse its file,
    // record its field layout, and enqueue every OTHER class it references (param
    // hints, `new D`, `D::fab`, `$x->m()` is not a class ref but `new`/static are).
    // Visited is keyed by the lower-cased codex FQCN so each class is reparsed once
    // per phase.
    let mut catalogue = ClassCatalogue::new();
    let mut layout_files: HashMap<String, (String, Vec<u8>)> = HashMap::new();
    let mut visited: Vec<Vec<u8>> = Vec::new();
    let mut worklist: Vec<Vec<u8>> = seed_fqcns;

    while let Some(fqcn) = worklist.pop() {
        let key = String::from_utf8_lossy(&normalize_fqcn(&fqcn)).to_ascii_lowercase();
        if visited.iter().any(|v| String::from_utf8_lossy(v) == key) {
            continue;
        }
        if visited.len() >= MAX_CLOSURE_CLASSES {
            break; // depth/size cap — bail the rest (fail-closed: fewer rules only).
        }
        visited.push(key.clone().into_bytes());

        let Some((logical, declaring_fqcn)) = locate_class_file(project, &fqcn) else {
            continue; // class not in codebase / file not loaded → no contribution.
        };
        let extracted = project.with_program(&logical, |program, _file, _names| {
            extract_class_layout_and_refs(program, &declaring_fqcn)
        });
        let Some(Some((layout, refs))) = extracted else {
            continue;
        };
        // The catalogue is keyed by the SIMPLE lower-cased name (how `cat.get` is
        // called everywhere: `instantiation_class_name` / `param_class` lower-case the
        // simple name). Resolve the simple name from the declaring FQCN.
        let simple = simple_name_lower(&declaring_fqcn);
        catalogue.insert(simple.clone(), layout);
        layout_files.insert(simple, (logical, declaring_fqcn.clone()));
        for r in refs {
            worklist.push(r);
        }
    }

    // Phase B — rule derivation against the COMPLETE catalogue. Reparse each resolved
    // class once more and derive its `{return e}` rules (now that every param-typed
    // class's layout is present). Reparsing twice is bounded and test-scoped.
    let mut rules = Vec::new();
    let mut descriptions = Vec::new();
    for (logical, declaring_fqcn) in layout_files.values() {
        let _ = project.with_program(logical, |program, _file, _names| {
            derive_rules_for_class(
                program,
                declaring_fqcn,
                &catalogue,
                &mut rules,
                &mut descriptions,
            )
        });
    }
    (rules, catalogue, descriptions)
}

/// Resolve a class FQCN to its declaring file's logical name and the codex's canonical
/// FQCN (used to match the class AST after reparse). Mirrors `subst.rs`'s codex hop.
fn locate_class_file(project: &MagoProject, fqcn: &[u8]) -> Option<(String, Vec<u8>)> {
    let key = normalize_fqcn(fqcn).to_ascii_lowercase();
    let meta = project.codebase().get_class_like(&key)?;
    let file = project.file_of_span(&meta.span)?;
    let logical = String::from_utf8_lossy(&file.name).into_owned();
    let declaring_fqcn = meta.name.as_bytes().to_vec();
    Some((logical, declaring_fqcn))
}

/// The simple (namespace-stripped), lower-cased name of an FQCN — the catalogue key.
fn simple_name_lower(fqcn: &[u8]) -> String {
    let simple = fqcn.rsplit(|b| *b == b'\\').next().unwrap_or(fqcn);
    String::from_utf8_lossy(simple).to_ascii_lowercase()
}

/// Inside a reparsed program, find the class named `declaring_fqcn` (by simple name,
/// the only name available on the AST node), compute its CONSTRUCTIBLE field layout,
/// and collect the OTHER class names its methods/ctor reference — so the closure
/// follows them. Returns `None` when the class is not in this program OR its ctor is
/// not a pure positional seed (a validating/normalising ctor): such a class is left
/// OUT of the catalogue entirely, so `new C(…)` builds no node and the test is
/// fail-closed Unknown (never a fabricated `(C a b)` verdict).
fn extract_class_layout_and_refs(
    program: &Program,
    declaring_fqcn: &[u8],
) -> Option<(FieldLayout, Vec<Vec<u8>>)> {
    let class = find_class_ast(program, declaring_fqcn)?;
    let layout = constructible_layout(class)?;
    let mut refs: Vec<Vec<u8>> = Vec::new();
    collect_class_refs(class, &mut refs);
    Some((layout, refs))
}

/// Locate a class AST node by simple name (case-insensitive), descending one level of
/// namespaces — the same scope the class-catalogue closure uses.
fn find_class_ast<'a>(program: &'a Program<'a>, fqcn: &[u8]) -> Option<&'a Class<'a>> {
    let simple = fqcn.rsplit(|b| *b == b'\\').next().unwrap_or(fqcn);
    find_class_ast_in(program.statements.iter(), simple)
}

fn find_class_ast_in<'a, I>(stmts: I, simple: &[u8]) -> Option<&'a Class<'a>>
where
    I: Iterator<Item = &'a Statement<'a>>,
{
    for stmt in stmts {
        match stmt {
            Statement::Class(class) if class.name.value.eq_ignore_ascii_case(simple) => {
                return Some(class);
            }
            Statement::Namespace(ns) => {
                if let Some(c) = find_class_ast_in(ns.statements().iter(), simple) {
                    return Some(c);
                }
            }
            _ => {}
        }
    }
    None
}

/// Derive the `{return e}` rules of ONE class (by simple name) against a catalogue.
/// Used by the closure's Phase B (the single-class analogue of `derive_from_class`).
fn derive_rules_for_class(
    program: &Program,
    declaring_fqcn: &[u8],
    cat: &ClassCatalogue,
    rules: &mut Vec<Rewrite<PhpL, GroundEval>>,
    descriptions: &mut Vec<String>,
) {
    if let Some(class) = find_class_ast(program, declaring_fqcn) {
        derive_from_class(class, cat, rules, descriptions);
    }
}

/// Collect the class names a class's bodies reference: each param's class hint, every
/// `new D(…)` and `D::fab(…)` in a method/ctor body, plus param hints — so the closure
/// keeps each referenced VO's layout/rules in scope (e.g. `plus` returning `new Frac`).
fn collect_class_refs(class: &Class, out: &mut Vec<Vec<u8>>) {
    for member in class.members.iter() {
        let ClassLikeMember::Method(m) = member else {
            continue;
        };
        for p in m.parameter_list.parameters.iter() {
            if let Some(h) = p.hint.as_ref() {
                if let Some(name) = class_hint_name(h) {
                    out.push(name);
                }
            }
        }
        if let MethodBody::Concrete(block) = &m.body {
            for stmt in block.statements.iter() {
                collect_refs_in_statement(stmt, out);
            }
        }
    }
}

fn collect_refs_in_statement(stmt: &Statement, out: &mut Vec<Vec<u8>>) {
    match stmt {
        Statement::Expression(es) => collect_refs_in_expr(es.expression, out),
        Statement::Return(ret) => {
            if let Some(e) = ret.value {
                collect_refs_in_expr(e, out);
            }
        }
        _ => {}
    }
}

/// Walk an expression collecting `new D` / `D::fab` class names (the only things that
/// introduce a NEW class into the closure; a `$x->m()` receiver class is already in
/// scope via whatever produced `$x`).
fn collect_refs_in_expr(expr: &Expression, out: &mut Vec<Vec<u8>>) {
    match expr {
        Expression::Parenthesized(p) => collect_refs_in_expr(p.expression, out),
        Expression::Assignment(a) => {
            collect_refs_in_expr(a.lhs, out);
            collect_refs_in_expr(a.rhs, out);
        }
        Expression::Binary(b) => {
            collect_refs_in_expr(b.lhs, out);
            collect_refs_in_expr(b.rhs, out);
        }
        Expression::Instantiation(inst) => {
            if let Some(name) = instantiation_class_name(inst) {
                out.push(name);
            }
            if let Some(args) = inst.argument_list.as_ref() {
                collect_refs_in_args(args, out);
            }
        }
        Expression::Call(Call::StaticMethod(sm)) => {
            if let Some(name) = static_call_class_name(sm.class) {
                out.push(name);
            }
            collect_refs_in_args(&sm.argument_list, out);
        }
        Expression::Call(Call::Method(mc)) => {
            collect_refs_in_expr(mc.object, out);
            collect_refs_in_args(&mc.argument_list, out);
        }
        Expression::Call(Call::Function(fc)) => {
            collect_refs_in_args(&fc.argument_list, out);
        }
        Expression::Access(Access::Property(pa)) => collect_refs_in_expr(pa.object, out),
        _ => {}
    }
}

fn collect_refs_in_args(args: &ArgumentList, out: &mut Vec<Vec<u8>>) {
    for arg in args.arguments.iter() {
        match arg {
            Argument::Positional(p) => collect_refs_in_expr(p.value, out),
            Argument::Named(n) => collect_refs_in_expr(n.value, out),
        }
    }
}

/// The class FQCNs the TEST method directly references — the closure seed. Walks the
/// test method body's statements (its bindings and the final assertion's operands) for
/// `new C` / `C::fab`, plus the test class's own simple name (so same-file VOs and the
/// test class itself are resolved through the same machinery).
fn seed_class_refs(test_method: &Method, test_class_fqcn: &[u8]) -> Vec<Vec<u8>> {
    let mut refs: Vec<Vec<u8>> = vec![test_class_fqcn.to_vec()];
    if let MethodBody::Concrete(block) = &test_method.body {
        for stmt in block.statements.iter() {
            collect_refs_in_statement(stmt, &mut refs);
        }
    }
    refs
}

// ─── Public entry point ────────────────────────────────────────────────────────

/// Decide a test method's final `assertSame(L, R)` by e-graph congruence over rules
/// DERIVED from the touched classes' method bodies. Returns [`Decision::True`] when
/// `L` and `R` land in one e-class after saturation, [`Decision::False`] when both
/// reduce to DISTINCT concrete integers, and [`Decision::Unknown`] otherwise (any
/// bail, an opaque method, an un-decided congruence — fail-closed; the runner then
/// executes the test for real).
pub fn decide_test_egraph(project: &MagoProject, class: &str, method: &str) -> Decision {
    decide_inner(project, class, method).unwrap_or(Decision::Unknown)
}

fn decide_inner(project: &MagoProject, class: &str, method: &str) -> Option<Decision> {
    let class_meta = project.find_class(class)?;
    let file = project.file_of_span(&class_meta.span)?;
    let logical = String::from_utf8_lossy(&file.name).into_owned();
    let class_fqcn = class_meta.name.as_bytes().to_vec();

    project.with_program(&logical, |program, file, _names| {
        let source_text = String::from_utf8_lossy(&file.contents);
        decide_with_program(project, program, &source_text, &class_fqcn, method)
    })?
}

fn decide_with_program(
    project: &MagoProject,
    program: &Program,
    source_text: &str,
    class_fqcn: &[u8],
    method: &str,
) -> Option<Decision> {
    // 1. Locate the test method first — its body seeds the cross-file closure.
    let m = find_class_method(program, class_fqcn, method.as_bytes())?;

    // 1b. Derive the equations + field layouts from the CLOSURE of classes the test
    //     references — resolved cross-file via the codex (the v4 wall-crossing). This
    //     subsumes the old single-file `derive_rules(program)`: same-file classes are
    //     resolved through the same codex→reparse path (the test file is just one more
    //     file in the closure).
    let seed = seed_class_refs(m, class_fqcn);
    let (rules, cat, _descs) = derive_closure(project, seed);

    // 2. The test method body, its parameters, and its substitution rows. A zero-param
    //    method has exactly ONE empty row (the v2 static-factory path). A parametrized
    //    method binds its params, positionally, from a STATICALLY-evaluated data provider
    //    — every row a tuple of concrete literal Givens (int/string/bool/float/null). A
    //    parametrized method with no derivable provider, or a provider that is not a pure
    //    array literal, yields no rows → fail-closed Unknown (the runner executes it).
    let MethodBody::Concrete(block) = &m.body else {
        return None;
    };
    let params: Vec<Vec<u8>> = m
        .parameter_list
        .parameters
        .iter()
        .map(|p| strip_dollar(p.variable.name))
        .collect();
    let rows: Vec<Vec<Option<PhpValue>>> = if params.is_empty() {
        vec![Vec::new()]
    } else {
        provider_value_rows(program, source_text, class_fqcn, m)?
    };

    // 2b. EXCEPTION-test decision (arithmetic): an `expectException(DivisionByZero…)` whose
    //     subject divides by a provably-ZERO divisor throws as expected — decide it here
    //     instead of executing. `None` falls through to the value path (then Unknown).
    if let Some(d) = decide_exception_rows(&cat, &rules, &params, &rows, block) {
        return Some(d);
    }

    // 3. The value-decision path: the test method's final assertion.
    let assertion = collect_assertion(block)?;

    // 4. Decide each (method × row) independently — PHPUnit treats every provider row
    //    as a SEPARATE test. Aggregate per-METHOD: True iff EVERY row decides True;
    //    False iff at least one row is provably False AND no row is Unknown; otherwise
    //    Unknown (any undecided row poisons a False claim — fail-closed). This mirrors
    //    "the whole parametrized method passes iff all its rows pass".
    let mut any_false = false;
    for row in &rows {
        match decide_one(&cat, &rules, &params, row, &assertion)? {
            Decision::True => {}
            Decision::False => any_false = true,
            Decision::Unknown => return Some(Decision::Unknown),
        }
    }
    if any_false {
        Some(Decision::False)
    } else {
        Some(Decision::True)
    }
}

/// Decide ONE assertion under ONE provider row. Binds the test's parameters to the
/// row's integer leaves, binds any `$var = …` prefix locals, builds both operands
/// into one e-graph, saturates with the derived rules + ground folding, and decides
/// by the assertion's comparison kind. `None` propagates a hard bail (an operand that
/// escapes the fragment); the caller maps that to Unknown for the whole method.
fn decide_one(
    cat: &ClassCatalogue,
    rules: &[Rewrite<PhpL, GroundEval>],
    params: &[Vec<u8>],
    row: &[Option<PhpValue>],
    assertion: &Assertion,
) -> Option<Decision> {
    let mut egraph: EGraph<PhpL, GroundEval> = EGraph::default();
    let mut vars: HashMap<Vec<u8>, Id> = HashMap::new();

    // Bind parameters positionally to their row's concrete Given. PHPUnit binds row
    // columns to params by position; SURPLUS columns are ignored, and a row SHORTER than
    // the param list cannot satisfy the call → bail. A scalar column binds to its leaf; an
    // array/absent column leaves the param UNBOUND, so any use of it bails the row to
    // Unknown (fail-closed, no false fusion).
    if row.len() < params.len() {
        return None;
    }
    for (name, col) in params.iter().zip(row.iter()) {
        if let Some(op) = col.as_ref().and_then(phpvalue_scalar_op) {
            let id = egraph.add(SymbolLang::leaf(op));
            vars.insert(name.clone(), id);
        }
    }

    // Bind any `$r = <expr>;` prefix locals (built over the param leaves).
    for (name, expr) in &assertion.bindings {
        let id = {
            let mut b = ExprBuilder {
                egraph: &mut egraph,
                cat,
                vars: &vars,
            };
            b.build(expr)?
        };
        vars.insert(name.clone(), id);
    }

    let l_id = {
        let mut b = ExprBuilder {
            egraph: &mut egraph,
            cat,
            vars: &vars,
        };
        b.build(assertion.lhs)?
    };
    let r_id = {
        let mut b = ExprBuilder {
            egraph: &mut egraph,
            cat,
            vars: &vars,
        };
        b.build(assertion.rhs)?
    };

    let runner = Runner::default().with_egraph(egraph).run(rules);
    let egraph = runner.egraph;

    let raw = decide_pair(&egraph, l_id, r_id, assertion.kind);
    Some(if assertion.invert { invert(raw) } else { raw })
}

/// Decide a single operand pair after saturation. For BOTH `Same` (`===`) and
/// `Equals` (`==`) over the concrete-integer fragment the verdict is the same: True
/// when the two operands share an e-class (congruence) — which for concrete integers
/// means equal values — and a definitive False when both reduce to DISTINCT known
/// constants. Anything else (a side still symbolic / object-bearing / un-folded) is
/// Unknown. The distinction between the two kinds is preserved for future scalar
/// widening, where loose `==` would diverge from strict `===` on cross-type pairs;
/// no such pair can arise here because only integers enter the graph.
fn decide_pair(
    egraph: &EGraph<PhpL, GroundEval>,
    l_id: Id,
    r_id: Id,
    _kind: AssertKind,
) -> Decision {
    if egraph.find(l_id) == egraph.find(r_id) {
        return Decision::True;
    }
    if let (Some(a), Some(b)) = (egraph[l_id].data, egraph[r_id].data) {
        if a != b {
            return Decision::False;
        }
    }
    Decision::Unknown
}

/// Flip a verdict for a `Not*` assertion. True↔False; Unknown is preserved (an
/// undecided test stays undecided whether or not it is negated).
fn invert(d: Decision) -> Decision {
    match d {
        Decision::True => Decision::False,
        Decision::False => Decision::True,
        Decision::Unknown => Decision::Unknown,
    }
}

/// How a recognised assertion compares its operands, and whether the True/False
/// verdict is inverted (the `Not*` family).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssertKind {
    /// `assertSame` — `===`, structural identity (exact for concrete integers).
    Same,
    /// `assertEquals` — `==`, value equality. For the concrete-integer fragment this
    /// coincides with `Same` (both decide by the shared e-class / ground constant),
    /// but the kinds stay distinct so future scalar widening keeps the semantics
    /// honest (cross-type `==` is not modelled here — only integers flow through).
    Equals,
}

/// The two operand expressions of a recognised binary assertion, the kind, an
/// `invert` flag (the `Not*` family flips True↔False, Unknown stays), plus the
/// `$var = <expr>` locals bound before it (each as name → its rhs expr).
struct Assertion<'a> {
    lhs: &'a Expression<'a>,
    rhs: &'a Expression<'a>,
    kind: AssertKind,
    invert: bool,
    bindings: Vec<(Vec<u8>, &'a Expression<'a>)>,
}

/// Walk a test body's pure prefix: `$var = <expr>;` assignments bind locals, and a
/// final recognised assertion (`assertSame`/`assertEquals`/`assertNotSame`/
/// `assertNotEquals`, in method, static or bare-function form) yields its two
/// argument expressions, the comparison kind, the inversion flag and the collected
/// bindings. Anything else bails (fail-closed).
fn collect_assertion<'a>(
    block: &'a mago_syntax::ast::ast::block::Block<'a>,
) -> Option<Assertion<'a>> {
    use mago_syntax::ast::ast::assignment::AssignmentOperator;
    let mut bindings: Vec<(Vec<u8>, &Expression)> = Vec::new();
    for stmt in block.statements.iter() {
        let Statement::Expression(es) = stmt else {
            return None;
        };
        // A recognised binary assertion ends the prefix.
        if let Some((lhs, rhs, kind, invert)) = binary_assertion_args(es.expression) {
            return Some(Assertion {
                lhs,
                rhs,
                kind,
                invert,
                bindings,
            });
        }
        // Else a simple `$var = <expr>;` assignment.
        let Expression::Assignment(a) = es.expression else {
            return None;
        };
        if !matches!(a.operator, AssignmentOperator::Assign(_)) {
            return None;
        }
        let Expression::Variable(Variable::Direct(target)) = a.lhs else {
            return None;
        };
        bindings.push((strip_dollar(target.name), a.rhs));
    }
    None
}

/// If `expr` is a recognised two-operand assertion (`assertSame`/`assertEquals`/
/// `assertNotSame`/`assertNotEquals`, as `$this->…(...)`, `self::…(...)`, or the bare
/// function), its two argument expressions, the comparison kind and whether it is a
/// `Not*` (inverting) variant. A trailing message arg is allowed and ignored.
fn binary_assertion_args<'a>(
    expr: &'a Expression<'a>,
) -> Option<(&'a Expression<'a>, &'a Expression<'a>, AssertKind, bool)> {
    let (name, args) = call_name_and_args(expr)?;
    let (kind, invert) = if name.eq_ignore_ascii_case(b"assertSame") {
        (AssertKind::Same, false)
    } else if name.eq_ignore_ascii_case(b"assertNotSame") {
        (AssertKind::Same, true)
    } else if name.eq_ignore_ascii_case(b"assertEquals") {
        (AssertKind::Equals, false)
    } else if name.eq_ignore_ascii_case(b"assertNotEquals") {
        (AssertKind::Equals, true)
    } else {
        return None;
    };
    let exprs = positional_args(args)?;
    if exprs.len() < 2 {
        return None;
    }
    Some((exprs[0], exprs[1], kind, invert))
}

/// The callee name and argument list of a call expression, in `$this->m(...)`,
/// `self::m(...)` / `C::m(...)`, or bare `m(...)` form.
fn call_name_and_args<'a>(expr: &'a Expression<'a>) -> Option<(&'a [u8], &'a ArgumentList<'a>)> {
    let Expression::Call(call) = expr else {
        return None;
    };
    match call {
        Call::Method(m) => {
            let ClassLikeMemberSelector::Identifier(id) = &m.method else {
                return None;
            };
            Some((id.value, &m.argument_list))
        }
        Call::StaticMethod(sm) => {
            let ClassLikeMemberSelector::Identifier(id) = &sm.method else {
                return None;
            };
            Some((id.value, &sm.argument_list))
        }
        Call::Function(fc) => {
            use mago_syntax::ast::ast::identifier::Identifier;
            let Expression::Identifier(id) = fc.function else {
                return None;
            };
            let n = match id {
                Identifier::Local(l) => l.value,
                Identifier::Qualified(q) => q.value,
                Identifier::FullyQualified(f) => f.value,
            };
            Some((n, &fc.argument_list))
        }
        _ => None,
    }
}

/// The positional argument expressions of a call, bailing on a spread or a named
/// argument (unmodelled).
fn positional_args<'a>(args: &'a ArgumentList<'a>) -> Option<Vec<&'a Expression<'a>>> {
    let mut exprs = Vec::new();
    for arg in args.arguments.iter() {
        match arg {
            Argument::Positional(p) => {
                if p.ellipsis.is_some() {
                    return None;
                }
                exprs.push(p.value);
            }
            Argument::Named(_) => return None,
        }
    }
    Some(exprs)
}

// ─── Exception-test decision (arithmetic) ───────────────────────────────────────

/// Decide an `expectException(...)` test whose subject throws an ARITHMETIC exception the
/// engine can prove statically — currently DIVISION BY ZERO. `Some(True)` when EVERY row's
/// subject provably divides by a folded-`0` divisor (the expected exception is thrown),
/// `Some(False)` when a row provably does NOT throw (a concrete non-zero divisor → the
/// expected exception never fires → the test fails), and `None` (→ caller falls through to
/// the value path, then Unknown) for anything not provable: not an exception test, a
/// message/code constraint we cannot reproduce, a non-div-by-zero type, no recognised
/// division subject, or a divisor that does not fold to a concrete integer. Fail-closed.
fn decide_exception_rows(
    cat: &ClassCatalogue,
    rules: &[Rewrite<PhpL, GroundEval>],
    params: &[Vec<u8>],
    rows: &[Vec<Option<PhpValue>>],
    block: &mago_syntax::ast::ast::block::Block,
) -> Option<Decision> {
    use mago_syntax::ast::ast::assignment::AssignmentOperator;
    let mut expected: Option<Vec<u8>> = None;
    let mut bindings: Vec<(Vec<u8>, &Expression)> = Vec::new();
    let mut divisor: Option<&Expression> = None;
    for stmt in block.statements.iter() {
        let Statement::Expression(es) = stmt else {
            return None;
        };
        let e = es.expression;
        if let Some(t) = expect_exception_class(e) {
            expected = Some(t);
            continue;
        }
        // A message/code/object constraint — we cannot reproduce the exact message:
        // fail-closed (the runner executes it).
        if is_expect_constraint(e) {
            return None;
        }
        // A `$var = <expr>;` prefix local.
        if let Expression::Assignment(a) = e {
            if matches!(a.operator, AssignmentOperator::Assign(_)) {
                if let Expression::Variable(Variable::Direct(target)) = a.lhs {
                    bindings.push((strip_dollar(target.name), a.rhs));
                    continue;
                }
            }
            return None;
        }
        // The subject: an expression containing a division whose divisor we will fold.
        if let Some(d) = find_division_divisor(e) {
            divisor = Some(d);
            continue;
        }
        return None;
    }
    let expected = expected?;
    if !is_division_by_zero_exception(&expected) {
        return None;
    }
    let divisor = divisor?;

    // Every row must provably divide by zero (throws) for the method to pass.
    let mut any_false = false;
    for row in rows {
        match eval_expr_int(cat, rules, params, row, &bindings, divisor) {
            Some(0) => {}                // divisor folds to 0 → throws as expected
            Some(_) => any_false = true, // a concrete non-zero divisor → no throw → fail
            None => return None,         // divisor does not fold → Unknown
        }
    }
    Some(if any_false {
        Decision::False
    } else {
        Decision::True
    })
}

/// The class named by `$this->expectException(T::class)` (its raw identifier bytes), or
/// `None` if `expr` is not such a call (or its argument is not a `::class` constant).
fn expect_exception_class(expr: &Expression) -> Option<Vec<u8>> {
    let (name, args) = call_name_and_args(expr)?;
    if !name.eq_ignore_ascii_case(b"expectException") {
        return None;
    }
    let exprs = positional_args(args)?;
    let first = *exprs.first()?;
    let Expression::Access(Access::ClassConstant(cc)) = first else {
        return None;
    };
    identifier_class_name(cc.class)
}

/// A constraint narrowing an expected exception beyond its TYPE — a message / code /
/// object matcher (`expectExceptionMessage*`, `expectExceptionCode`, …). We cannot
/// reproduce these statically, so they fail-closed.
fn is_expect_constraint(expr: &Expression) -> bool {
    let Some((name, _)) = call_name_and_args(expr) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.starts_with(b"expectexception") && lower.len() > b"expectexception".len()
}

/// The simple name (last `\`-separated segment) contains `DivisionByZero` — matches both
/// Brick's `DivisionByZeroException` and PHP's `DivisionByZeroError`.
fn is_division_by_zero_exception(name: &[u8]) -> bool {
    let simple = name.rsplit(|&b| b == b'\\').next().unwrap_or(name);
    let needle = b"DivisionByZero";
    simple.windows(needle.len()).any(|w| w == needle)
}

/// The divisor expression of the FIRST division in `expr` (a `/` or `%` binary, an
/// `intdiv`/`fmod`/`fdiv` call, or a `dividedBy`/`div`/`mod`/`modulo`/`remainder`/`divide`
/// method), searched through arithmetic and call sub-expressions only — NOT through
/// conditionals, so a found division is unconditionally reached (its throw is real).
fn find_division_divisor<'a>(expr: &'a Expression<'a>) -> Option<&'a Expression<'a>> {
    match expr {
        Expression::Binary(b) => {
            if matches!(
                b.operator,
                BinaryOperator::Division(_) | BinaryOperator::Modulo(_)
            ) {
                return Some(b.rhs);
            }
            find_division_divisor(b.lhs).or_else(|| find_division_divisor(b.rhs))
        }
        Expression::Call(Call::Method(mc)) => {
            if let ClassLikeMemberSelector::Identifier(id) = &mc.method {
                if is_division_method(id.value) {
                    if let Some(args) = positional_args(&mc.argument_list) {
                        if let Some(&first) = args.first() {
                            return Some(first);
                        }
                    }
                }
            }
            find_division_divisor(mc.object).or_else(|| find_division_in_args(&mc.argument_list))
        }
        Expression::Call(Call::StaticMethod(sm)) => find_division_in_args(&sm.argument_list),
        Expression::Call(Call::Function(fc)) => {
            if let Some(n) = function_call_name(fc.function) {
                if matches!(n.as_slice(), b"intdiv" | b"fmod" | b"fdiv") {
                    if let Some(args) = positional_args(&fc.argument_list) {
                        if args.len() >= 2 {
                            return Some(args[1]);
                        }
                    }
                }
            }
            find_division_in_args(&fc.argument_list)
        }
        _ => None,
    }
}

fn find_division_in_args<'a>(args: &'a ArgumentList<'a>) -> Option<&'a Expression<'a>> {
    let exprs = positional_args(args)?;
    exprs.into_iter().find_map(find_division_divisor)
}

/// Methods that divide their receiver by their first argument (bignum / decimal APIs).
fn is_division_method(name: &[u8]) -> bool {
    matches!(
        name.to_ascii_lowercase().as_slice(),
        b"dividedby" | b"divide" | b"div" | b"mod" | b"modulo" | b"remainder"
    )
}

/// Fold `expr` to a concrete integer under one provider row: bind params to the row's
/// integer leaves and any `$var = …` prefix locals, build, saturate, and read the ground
/// constant. `None` when the build bails or the value does not fold to an integer.
fn eval_expr_int(
    cat: &ClassCatalogue,
    rules: &[Rewrite<PhpL, GroundEval>],
    params: &[Vec<u8>],
    row: &[Option<PhpValue>],
    bindings: &[(Vec<u8>, &Expression)],
    expr: &Expression,
) -> Option<i64> {
    if row.len() < params.len() {
        return None;
    }
    let mut egraph: EGraph<PhpL, GroundEval> = EGraph::default();
    let mut vars: HashMap<Vec<u8>, Id> = HashMap::new();
    for (name, col) in params.iter().zip(row.iter()) {
        if let Some(op) = col.as_ref().and_then(phpvalue_scalar_op) {
            let id = egraph.add(SymbolLang::leaf(op));
            vars.insert(name.clone(), id);
        }
    }
    for (name, e) in bindings {
        let id = {
            let mut b = ExprBuilder {
                egraph: &mut egraph,
                cat,
                vars: &vars,
            };
            b.build(e)?
        };
        vars.insert(name.clone(), id);
    }
    let id = {
        let mut b = ExprBuilder {
            egraph: &mut egraph,
            cat,
            vars: &vars,
        };
        b.build(expr)?
    };
    let runner = Runner::default().with_egraph(egraph).run(rules);
    let egraph = runner.egraph;
    egraph[id].data
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `MagoProject` from one PHP source in a tempdir and decide a test
    /// (mirrors the bridge's test harness).
    fn decide(src: &str, class: &str, method: &str) -> Decision {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Code.php"), src).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        decide_test_egraph(&project, class, method)
    }

    /// AUTOMATON BEYOND INTEGERS: a STRING value object (store the Given, forward it) is
    /// decided by the SAME congruence as integer VOs — the layout/derivation were already
    /// value-agnostic; only the literal encoder was i64-gated. `value(Slug('hello'))`
    /// rewrites to `'hello'`, congruent with the asserted `'hello'`.
    #[test]
    fn string_value_object_decides_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Slug {
    public function __construct(private string $v) {}
    public function value(): string { return $this->v; }
}
final class StrVoTest extends TestCase {
    public function testRaw(): void {
        self::assertSame('hello', (new Slug('hello'))->value());
    }
}
"#;
        assert_eq!(decide(src, "StrVoTest", "testRaw"), Decision::True);
    }

    /// SOUNDNESS: a string VO whose stored value DIFFERS from the asserted one does not
    /// falsely decide — distinct string leaves never fuse (and strings carry no ground
    /// constant to prove a definitive False), so the verdict is Unknown (fail-closed).
    #[test]
    fn string_vo_mismatch_stays_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Slug {
    public function __construct(private string $v) {}
    public function value(): string { return $this->v; }
}
final class StrMismatchTest extends TestCase {
    public function testRaw(): void {
        self::assertSame('hello', (new Slug('world'))->value());
    }
}
"#;
        assert_eq!(decide(src, "StrMismatchTest", "testRaw"), Decision::Unknown);
    }

    /// A BOOL value object decides too — `true`/`false`/`null` flow through the same
    /// literal encoding as integers and strings.
    #[test]
    fn bool_value_object_decides_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Flag {
    public function __construct(private bool $b) {}
    public function get(): bool { return $this->b; }
}
final class BoolVoTest extends TestCase {
    public function testGet(): void {
        self::assertSame(true, (new Flag(true))->get());
    }
}
"#;
        assert_eq!(decide(src, "BoolVoTest", "testGet"), Decision::True);
    }

    /// THE parametrized reach: a `#[DataProvider]` of STRING rows now binds each Given as a
    /// concrete leaf, so `assertSame($s, (new Slug($s))->value())` decides True on EVERY
    /// row — the shape of real value-object suites, previously fail-closed to Unknown
    /// because the row was not a pure integer tuple.
    #[test]
    fn parametrized_string_vo_decides_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class Slug {
    public function __construct(private string $v) {}
    public function value(): string { return $this->v; }
}
final class StrProvTest extends TestCase {
    public static function rows(): array {
        return [['alpha'], ['beta']];
    }
    #[DataProvider('rows')]
    public function testRaw(string $s): void {
        self::assertSame($s, (new Slug($s))->value());
    }
}
"#;
        assert_eq!(decide(src, "StrProvTest", "testRaw"), Decision::True);
    }

    /// VALIDATING CTOR, valid Given: a ctor with a leading `if ($d < 1) throw;` guard is
    /// admitted — under the concrete Given `2`, the guard `2 < 1` is provably false, so the
    /// construction is valid and `getDenominator()` reduces. The IF is in the AST and now
    /// evaluated, not bailed.
    #[test]
    fn guarded_ctor_valid_given_decides_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Fraction {
    public function __construct(private int $numerator, private int $denominator) {
        if ($denominator < 1) {
            throw new \InvalidArgumentException('Denominator must be > 0');
        }
    }
    public function getDenominator(): int { return $this->denominator; }
}
final class GuardOkTest extends TestCase {
    public function testDen(): void {
        self::assertSame(2, (new Fraction(1, 2))->getDenominator());
    }
}
"#;
        assert_eq!(decide(src, "GuardOkTest", "testDen"), Decision::True);
    }

    /// SOUNDNESS: the SAME ctor with an INVALID Given (`0`) makes the guard `0 < 1` true →
    /// the construction throws → the getter rule's condition fails → it does NOT fire → the
    /// test stays Unknown (the runner executes it and observes the throw). NEVER a false
    /// `True` for a value that the construction never actually produces.
    #[test]
    fn guarded_ctor_throwing_given_stays_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Fraction {
    public function __construct(private int $numerator, private int $denominator) {
        if ($denominator < 1) {
            throw new \InvalidArgumentException('Denominator must be > 0');
        }
    }
    public function getDenominator(): int { return $this->denominator; }
}
final class GuardThrowTest extends TestCase {
    public function testDen(): void {
        self::assertSame(0, (new Fraction(1, 0))->getDenominator());
    }
}
"#;
        assert_eq!(decide(src, "GuardThrowTest", "testDen"), Decision::Unknown);
    }

    /// GATE 1 — conditional NORMALISE-and-return branch. The fraction-shaped ctor has both a
    /// throw guard AND a `if (0 == $n) { …; return; }` normalisation branch. Under the Given
    /// (3, 4) both conditions are provably not taken (`4 < 1` false, `0 == 3` false), so the
    /// pass-through holds and `getNumerator()` reduces to 3 → True.
    #[test]
    fn conditional_return_branch_valid_given_decides() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Fraction {
    public int $numerator;
    public int $denominator;
    public function __construct(int $numerator, int $denominator) {
        if ($denominator < 1) {
            throw new \InvalidArgumentException('bad');
        }
        if (0 == $numerator) {
            $this->numerator = 0;
            $this->denominator = 1;
            return;
        }
        $this->numerator = $numerator;
        $this->denominator = $denominator;
    }
    public function getNumerator(): int { return $this->numerator; }
}
final class FracBranchTest extends TestCase {
    public function testNum(): void {
        self::assertSame(3, (new Fraction(3, 4))->getNumerator());
    }
}
"#;
        assert_eq!(decide(src, "FracBranchTest", "testNum"), Decision::True);
    }

    /// SOUNDNESS: when the normalisation branch IS taken (`0 == 0` true), the pass-through
    /// rule does not fire → Unknown. We do not model the branch's rewritten fields, so we
    /// conservatively decline (never a false verdict for the normalised path).
    #[test]
    fn conditional_return_branch_taken_stays_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Fraction {
    public int $numerator;
    public int $denominator;
    public function __construct(int $numerator, int $denominator) {
        if ($denominator < 1) {
            throw new \InvalidArgumentException('bad');
        }
        if (0 == $numerator) {
            $this->numerator = 0;
            $this->denominator = 1;
            return;
        }
        $this->numerator = $numerator;
        $this->denominator = $denominator;
    }
    public function getNumerator(): int { return $this->numerator; }
}
final class FracTakenTest extends TestCase {
    public function testNum(): void {
        self::assertSame(0, (new Fraction(0, 4))->getNumerator());
    }
}
"#;
        assert_eq!(decide(src, "FracTakenTest", "testNum"), Decision::Unknown);
    }

    /// REFINEMENT (3): an `expectException(DivisionByZero…)` whose subject divides by a
    /// literal `0` is DECIDED True statically (the throw is proven), without executing.
    #[test]
    fn exception_div_by_zero_decides_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class DivZeroTest extends TestCase {
    public function testThrows(): void {
        $this->expectException(\Brick\Math\Exception\DivisionByZeroException::class);
        Num::of(10)->dividedBy(0);
    }
}
"#;
        assert_eq!(decide(src, "DivZeroTest", "testThrows"), Decision::True);
    }

    /// A provider whose divisor column is `0` on EVERY row decides True (every row throws).
    #[test]
    fn exception_div_by_zero_provider_all_zero_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class DivProvTest extends TestCase {
    public static function rows(): array {
        return [[0], [0]];
    }
    #[DataProvider('rows')]
    public function testThrows(int $d): void {
        $this->expectException(\DivisionByZeroError::class);
        Num::of(10)->dividedBy($d);
    }
}
"#;
        assert_eq!(decide(src, "DivProvTest", "testThrows"), Decision::True);
    }

    /// A row with a concrete NON-zero divisor does not throw → that row fails → the method
    /// is provably False (fail-closed only on UNfoldable divisors, not on this).
    #[test]
    fn exception_div_by_zero_provider_nonzero_row_false() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class DivMixTest extends TestCase {
    public static function rows(): array {
        return [[0], [7]];
    }
    #[DataProvider('rows')]
    public function testThrows(int $d): void {
        $this->expectException(\DivisionByZeroError::class);
        Num::of(10)->dividedBy($d);
    }
}
"#;
        assert_eq!(decide(src, "DivMixTest", "testThrows"), Decision::False);
    }

    /// FAIL-CLOSED: a message constraint (`expectExceptionMessage`) cannot be reproduced
    /// statically → Unknown (the runner executes it).
    #[test]
    fn exception_message_constraint_fail_closed() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class DivMsgTest extends TestCase {
    public function testThrows(): void {
        $this->expectException(\DivisionByZeroException::class);
        $this->expectExceptionMessage('Division by zero.');
        Num::of(10)->dividedBy(0);
    }
}
"#;
        assert_eq!(decide(src, "DivMsgTest", "testThrows"), Decision::Unknown);
    }

    /// FAIL-CLOSED: a non-div-by-zero expected type is not proven by a zero divisor → we do
    /// NOT claim the throw; Unknown.
    #[test]
    fn exception_non_div_by_zero_type_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class DivOtherTest extends TestCase {
    public function testThrows(): void {
        $this->expectException(\InvalidArgumentException::class);
        Num::of(10)->dividedBy(0);
    }
}
"#;
        assert_eq!(decide(src, "DivOtherTest", "testThrows"), Decision::Unknown);
    }

    /// Collect the derived-rule descriptions for a source (for white-box assertions
    /// on WHICH equations were derived) — through the v4 cross-file closure, seeded
    /// with every class declared in the source (so all their rules are derived).
    fn derived_descriptions(src: &str) -> Vec<String> {
        use mago_syntax::ast::ast::statement::Statement;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Code.php"), src).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        let seed = project
            .with_program("Code.php", |program, _src, _names| {
                program
                    .statements
                    .iter()
                    .filter_map(|s| match s {
                        Statement::Class(c) => Some(c.name.value.to_vec()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap();
        let (_rules, _cat, descs) = derive_closure(&project, seed);
        descs
    }

    /// Build a multi-file `MagoProject` (a real composer.json + vendor PHPUnit stub +
    /// the given `(relative-path, source)` files) and decide a test method against it.
    /// This loads classes that live in SEPARATE files (`src/Point.php`) from the test
    /// (`tests/PointTest.php`) — the PSR-4 layout EVERY real library uses — so it
    /// exercises the cross-file rule-derivation path, unlike the single-file `decide`.
    fn decide_in_project(files: &[(&str, &str)], class: &str, method: &str) -> Decision {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("composer.json"), "{\n}\n").unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/phpunit/phpunit/src/Framework")).unwrap();
        std::fs::write(
            dir.path()
                .join("vendor/phpunit/phpunit/src/Framework/TestCase.php"),
            "<?php namespace PHPUnit\\Framework; abstract class TestCase {}",
        )
        .unwrap();
        for (name, src) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, src).unwrap();
        }
        let project = MagoProject::load_excluding_vendor(dir.path()).unwrap();
        decide_test_egraph(&project, class, method)
    }

    // ─── Suite-wide compression (share, don't decide) ──────────────────────────

    /// Build the suite-wide compression stats for one single-file source: every
    /// `public function test*` of `class` is extracted into ONE shared e-graph. Mirrors
    /// the harness, but single-file and test-scoped.
    fn compress(src: &str, class: &str) -> CompressionStats {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Code.php"), src).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        project
            .with_program("Code.php", |program, src, _names| {
                let source_text = String::from_utf8_lossy(&src.contents);
                // Collect every test method of `class` (a `test*` instance method).
                let class_ast = find_class_ast(program, class.as_bytes())
                    .expect("test class present in program");
                let mut pairs: Vec<(Vec<u8>, &Method)> = Vec::new();
                for member in class_ast.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        let name = String::from_utf8_lossy(m.name.value).into_owned();
                        if name.starts_with("test") {
                            pairs.push((class.as_bytes().to_vec(), m));
                        }
                    }
                }
                build_suite_egraph(&project, program, &source_text, &pairs)
            })
            .unwrap()
    }

    /// THE recentring proof: a suite of OPAQUE, 0%-decidable tests still compresses
    /// HARD. Three tests each build the IDENTICAL `Carbon::create(2024, 1, 15)` and read
    /// a property — none is decidable (Carbon is unmodelled), yet the repeated
    /// construction collapses to ONE e-class, so `classes_struct < n_naive`.
    #[test]
    fn opaque_repetition_compresses() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class OpaqueTest extends TestCase {
    public function testA(): void {
        $d = Carbon::create(2024, 1, 15);
        $this->assertSame(2024, $d->year);
    }
    public function testB(): void {
        $d = Carbon::create(2024, 1, 15);
        $this->assertSame(1, $d->month);
    }
    public function testC(): void {
        $d = Carbon::create(2024, 1, 15);
        $this->assertSame(15, $d->day);
    }
}
"#;
        let s = compress(src, "OpaqueTest");
        assert_eq!(s.tests, 3, "all three test bodies contributed");
        // Sharing must have happened: fewer e-classes than naive nodes.
        assert!(
            s.classes_struct < s.n_naive,
            "structural sharing must reduce classes below naive: {s:?}"
        );
        // The shared `Carbon::create(2024,1,15)` (and its 3 literal children) appears in
        // all 3 tests but is ONE e-class each — so the ratio is comfortably > 1.
        assert!(
            s.ratio_struct() > 1.2,
            "repeated opaque construction must compress: ratio_struct={:.2}",
            s.ratio_struct()
        );
    }

    /// COST-WEIGHTED: the same three-test opaque suite. The shared `Carbon::create`
    /// (built 3×, ONE class) is a CALL (cost 1) shared at multiplicity 3, and each
    /// `$d->prop` getter is a CALL too. The dominant TOP target must be `Carbon::create`
    /// at multiplicity 3, and `cost_compression > 1` because the heavy calls are shared.
    #[test]
    fn cost_weighted_top_target_is_the_shared_constructor() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class OpaqueTest extends TestCase {
    public function testA(): void {
        $d = Carbon::create(2024, 1, 15);
        $this->assertSame(2024, $d->year);
    }
    public function testB(): void {
        $d = Carbon::create(2024, 1, 15);
        $this->assertSame(1, $d->month);
    }
    public function testC(): void {
        $d = Carbon::create(2024, 1, 15);
        $this->assertSame(15, $d->day);
    }
}
"#;
        let s = compress(src, "OpaqueTest");
        // cost_naive counts every call insertion with repetition; cost_shared counts each
        // distinct call class once. The shared constructor makes cost_naive > cost_shared.
        assert!(
            s.cost_naive > s.cost_shared && s.cost_shared > 0,
            "shared calls must lift cost_naive above cost_shared: {s:?}"
        );
        assert!(
            s.cost_compression() > 1.0,
            "cost_compression must exceed 1 when calls are shared: {:.2}",
            s.cost_compression()
        );
        // The single most valuable memoisation target is the thrice-built constructor.
        let top = s.top_targets.first().expect("a top target exists");
        assert_eq!(
            top.op, "Carbon::create",
            "dominant target is the constructor"
        );
        assert_eq!(top.mult, 3, "the constructor is shared across all 3 tests");
        assert_eq!(top.cost, 1, "a constructor is one call-unit");
        assert_eq!(
            top.saved(),
            2,
            "memoising it removes 2 of the 3 evaluations"
        );
    }

    /// COST-WEIGHTED, control: a literal/arithmetic-only suite has ZERO call-cost — the
    /// node compression can be > 1 (literals share) while cost_compression is 0.0,
    /// proving the weight isolates CALL work from cheap structural sharing.
    #[test]
    fn cost_weighted_pure_arithmetic_has_no_call_work() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class ArithTest extends TestCase {
    public function testA(): void {
        $this->assertSame(5, 2 + 3);
    }
    public function testB(): void {
        $this->assertSame(5, 2 + 3);
    }
}
"#;
        let s = compress(src, "ArithTest");
        // `$this->assertSame(...)` is a test DIRECTIVE (a per-test sink) weighted 0, and the
        // arithmetic `2 + 3` and its literals are free too — so the whole suite has ZERO
        // call-work and NO memoisation targets, even though literals share structurally.
        assert_eq!(
            s.cost_naive, 0,
            "directives + arithmetic + literals are all cost 0: {:?}",
            s.top_targets
        );
        assert!(
            s.top_targets.is_empty(),
            "no call-work means no memoisation targets: {:?}",
            s.top_targets
        );
        assert!(
            !s.top_targets.iter().any(|t| t.op == "+"),
            "arithmetic must never appear as a cost target: {:?}",
            s.top_targets
        );
    }

    /// REFINEMENT (AST-aware cost model): test DIRECTIVES (`assert*`/`expect*`) are per-test
    /// sinks weighted 0 and are NEVER memoisation targets — only the REAL shared computation
    /// they wrap is. Three tests each `expectException(...)` then build the same
    /// `Carbon::create(...)`: the constructor is the dominant target; the directives
    /// contribute no call-work and never surface as targets.
    #[test]
    fn directives_are_zero_cost_only_real_calls_are_targets() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class DirTest extends TestCase {
    public function testA(): void {
        $this->expectException(\RuntimeException::class);
        self::assertSame(2024, Carbon::create(2024, 1, 15)->year);
    }
    public function testB(): void {
        $this->expectException(\RuntimeException::class);
        self::assertSame(2024, Carbon::create(2024, 1, 15)->year);
    }
}
"#;
        let s = compress(src, "DirTest");
        // Neither the `$this->expectException` nor the `self::assertSame` directive may ever
        // appear as a memoisation target — they are sinks, not shareable work.
        assert!(
            s.top_targets
                .iter()
                .all(|t| !t.op.starts_with("expect") && !t.op.starts_with("assert")),
            "directives must never be memoisation targets: {:?}",
            s.top_targets
        );
        // The REAL shared computation (the constructor) IS the dominant target.
        assert!(
            s.top_targets.iter().any(|t| t.op == "Carbon::create"),
            "the real shared call is a target: {:?}",
            s.top_targets
        );
    }

    /// REFINEMENT (array provider substitution): array-valued data-provider columns
    /// (matrices, coordinate lists) are materialised as STRUCTURAL leaves, so the SAME
    /// array reused across rows fuses into one e-class — the cross-row sharing array
    /// providers carry, previously missed (arrays were left as free salted params).
    #[test]
    fn array_provider_columns_substitute_and_share() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class ArrTest extends TestCase {
    public static function provider(): array {
        return [
            [[[1, 2], [3, 4]]],
            [[[1, 2], [3, 4]]],
        ];
    }
    #[DataProvider('provider')]
    public function testSum(array $m): void {
        self::assertSame(10, Matrix::of($m)->sum());
    }
}
"#;
        let s = compress(src, "ArrTest");
        // Both rows pass the IDENTICAL matrix, so `Matrix::of(<that matrix>)` is ONE shared
        // class at multiplicity 2 — provable only because the array Given is now a concrete
        // structural leaf, not a free salted param.
        assert!(
            s.top_targets
                .iter()
                .any(|t| t.op == "Matrix::of" && t.mult >= 2),
            "the array-built construction shares across rows: {:?}",
            s.top_targets
        );
    }

    /// Soundness: two SYNTACTICALLY DIFFERENT opaque calls must NOT fuse. Distinct
    /// `Carbon::create(...)` argument tuples are distinct computations → distinct
    /// e-classes; only the shared literal leaves (`2024`) may coincide.
    #[test]
    fn distinct_opaque_calls_do_not_fuse() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class DistinctTest extends TestCase {
    public function testA(): void {
        $d = Carbon::create(2024, 1, 15);
        $this->assertSame(2024, $d->year);
    }
    public function testB(): void {
        $d = Carbon::create(1999, 12, 31);
        $this->assertSame(1999, $d->year);
    }
}
"#;
        let s = compress(src, "DistinctTest");
        assert_eq!(s.tests, 2);
        // The two Carbon::create(...) nodes differ in every argument, so they are two
        // distinct e-classes; the suite still has >1 class (nothing collapsed to a
        // single point). A coarse sanity bound: at least the two create-calls + two
        // year-props + distinct literals remain separate.
        assert!(
            s.classes_struct >= 6,
            "distinct opaque calls must not over-fuse: {s:?}"
        );
    }

    /// A modelled integer-VO suite compresses AND its derived rules fuse further: a
    /// repeated `Num::of(5)` shares structurally, and saturation folds the value chain
    /// so `classes_sat <= classes_struct` (rules only ever MERGE, never split).
    #[test]
    fn modelled_suite_saturation_never_increases_classes() {
        let s = compress(NUM_SRC, "NumTest");
        assert!(s.tests >= 1);
        assert!(
            s.classes_sat <= s.classes_struct,
            "saturation (rules + ground fold) only merges e-classes: {s:?}"
        );
        assert!(s.n_naive >= s.classes_struct, "naive >= shared: {s:?}");
    }

    // ─── v4: cross-file rule derivation (resolve VO classes in src/) ───────────

    /// THE control. A whole integer value-object `Point` lives in `src/Point.php`,
    /// SEPARATE from `tests/PointTest.php` — the PSR-4 layout of every real library.
    /// Before v4 the rules were derived only from the test file, so `Point` was
    /// Unknown (0%); v4 resolves `Point` through the codex, reparses `src/Point.php`,
    /// and derives `x`/`y` getters + the promoted-ctor layout → `True`.
    #[test]
    fn split_file_point_decides_true() {
        let point = r#"<?php
final class Point {
    public function __construct(private int $x, private int $y) {}
    public function x(): int { return $this->x; }
    public function y(): int { return $this->y; }
}
"#;
        let test = r#"<?php
use PHPUnit\Framework\TestCase;
final class PointTest extends TestCase {
    public function testX(): void {
        $this->assertSame(3, (new Point(3, 4))->x());
    }
}
"#;
        assert_eq!(
            decide_in_project(
                &[("src/Point.php", point), ("tests/PointTest.php", test)],
                "PointTest",
                "testX",
            ),
            Decision::True
        );
    }

    /// Non-regression: the SAME Point, in-file (the v1/v2/v3 single-file path), still
    /// decides True through the unified cross-file machinery.
    #[test]
    fn same_file_still_works() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Point {
    public function __construct(private int $x, private int $y) {}
    public function x(): int { return $this->x; }
    public function y(): int { return $this->y; }
}
final class PointTest extends TestCase {
    public function testX(): void {
        $this->assertSame(3, (new Point(3, 4))->x());
    }
}
"#;
        assert_eq!(decide(src, "PointTest", "testX"), Decision::True);
    }

    /// A value-object whose constructor VALIDATES with a leading guard `if ($d === 0)
    /// throw;` is now ADMITTED: under the concrete Given `4`, the guard `4 === 0` is
    /// provably false, so the construction is valid and `num()` reduces → True (sound: in
    /// real PHP `new Frac(3, 4)` constructs and `num()` returns 3). Cross-file (`Frac` in
    /// `src/`). The throwing case is covered by `guarded_ctor_throwing_given_stays_unknown`.
    #[test]
    fn guarded_ctor_decides_cross_file() {
        let frac = r#"<?php
final class Frac {
    public int $n;
    public int $d;
    public function __construct(int $n, int $d) {
        if ($d === 0) { throw new \InvalidArgumentException('zero denominator'); }
        $this->n = $n;
        $this->d = $d;
    }
    public function num(): int { return $this->n; }
}
"#;
        let test = r#"<?php
use PHPUnit\Framework\TestCase;
final class FracTest extends TestCase {
    public function testNum(): void {
        $this->assertSame(3, (new Frac(3, 4))->num());
    }
}
"#;
        assert_eq!(
            decide_in_project(
                &[("src/Frac.php", frac), ("tests/FracTest.php", test)],
                "FracTest",
                "testNum",
            ),
            Decision::True
        );
    }

    /// The transitive closure: `Frac::of(..)->times(Frac::of(..))->num()` where `Frac`
    /// is in `src/`. `of` returns `new Frac(...)`, `times` returns `new Frac(...)` —
    /// the closure must reparse `src/Frac.php`, derive `of`/`times`/`num` AND follow
    /// the `new Frac` references inside those bodies to keep `Frac` in the catalogue.
    #[test]
    fn cross_file_transform_chain() {
        let frac = r#"<?php
final class Frac {
    public function __construct(private int $num, private int $den) {}
    public static function of(int $n, int $d): self { return new Frac($n, $d); }
    public function times(Frac $o): Frac { return new Frac($this->num * $o->num, $this->den * $o->den); }
    public function num(): int { return $this->num; }
}
"#;
        let test = r#"<?php
use PHPUnit\Framework\TestCase;
final class FracTest extends TestCase {
    public function testTimesNum(): void {
        $this->assertSame(15, Frac::of(3, 4)->times(Frac::of(5, 7))->num());
    }
}
"#;
        assert_eq!(
            decide_in_project(
                &[("src/Frac.php", frac), ("tests/FracTest.php", test)],
                "FracTest",
                "testTimesNum",
            ),
            Decision::True
        );
    }

    const NUM_SRC: &str = r#"<?php
use PHPUnit\Framework\TestCase;
final class Num {
    public function __construct(private int $v) {}
    public static function of(int $v): self { return new Num($v); }
    public function plus(Num $o): Num { return new Num($this->v + $o->v); }
    public function value(): int { return $this->v; }
}
final class NumTest extends TestCase {
    public function testStaticFactoryPlus(): void {
        $r = Num::of(5)->plus(Num::of(3));
        $this->assertSame(8, $r->value());
    }
}
"#;

    // ─── v2: the decisive static-factory fixture ──────────────────────────────

    /// THE target: `assertSame(8, Num::of(5)->plus(Num::of(3))->value())` decided
    /// TRUE by congruence over the DERIVED rules + ground fold. No execution, no
    /// object built. The real PHPUnit php8.4 gold-gate of this test PASSES.
    #[test]
    fn static_factory_plus_decides_true() {
        assert_eq!(
            decide(NUM_SRC, "NumTest", "testStaticFactoryPlus"),
            Decision::True
        );
    }

    /// White-box: the three equations are actually derived, in the expected shapes.
    #[test]
    fn derives_the_three_expected_rules() {
        let descs = derived_descriptions(NUM_SRC);
        // `Num::of(?v) => (Num ?v)`
        assert!(
            descs.iter().any(|d| d == "(Num::of ?v) => (Num ?v)"),
            "factory rule missing; got {descs:?}"
        );
        // `plus((Num ?a),(Num ?b)) => (Num (+ ?a ?b))`
        assert!(
            descs
                .iter()
                .any(|d| d == "(plus (Num ?this_v) (Num ?o_v)) => (Num (+ ?this_v ?o_v))"),
            "plus rule missing; got {descs:?}"
        );
        // `value((Num ?a)) => ?a`
        assert!(
            descs
                .iter()
                .any(|d| d == "(value (Num ?this_v)) => ?this_v"),
            "value rule missing; got {descs:?}"
        );
    }

    /// Non-fusion control: `assertSame(7, …->value())` where the term reduces to 8
    /// is NEVER True (and is a sound definitive False — distinct known constants).
    #[test]
    fn non_fusion_control() {
        let src = NUM_SRC.replace("assertSame(8,", "assertSame(7,");
        assert_eq!(
            decide(&src, "NumTest", "testStaticFactoryPlus"),
            Decision::False
        );
    }

    /// `new Num(5)` instead of the factory still decides True — proof that `new` and
    /// a static factory normalise onto the SAME `(Num …)` constructor node.
    #[test]
    fn new_based_equivalent() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Num {
    public function __construct(private int $v) {}
    public static function of(int $v): self { return new Num($v); }
    public function plus(Num $o): Num { return new Num($this->v + $o->v); }
    public function value(): int { return $this->v; }
}
final class NumTest extends TestCase {
    public function testNewPlus(): void {
        $r = (new Num(5))->plus(new Num(3));
        $this->assertSame(8, $r->value());
    }
}
"#;
        assert_eq!(decide(src, "NumTest", "testNewPlus"), Decision::True);
    }

    /// An opaque method (a non-`{return expr}` body, here an `if`) derives NO rule,
    /// so the congruence cannot fire → Unknown (fail-closed).
    #[test]
    fn opaque_method_stays_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Num {
    public function __construct(private int $v) {}
    public static function of(int $v): self { return new Num($v); }
    public function weird(): int { if ($this->v > 0) { return $this->v; } return 0; }
}
final class NumTest extends TestCase {
    public function testWeird(): void {
        $this->assertSame(5, Num::of(5)->weird());
    }
}
"#;
        assert_eq!(decide(src, "NumTest", "testWeird"), Decision::Unknown);
    }

    /// Overflow is fail-closed: `Num::of(PHP_INT_MAX)->plus(Num::of(1))->value()`
    /// vs a literal does not fold (checked) → the congruence never reaches a
    /// concrete literal → Unknown.
    #[test]
    fn overflow_fail_closed() {
        let max = i64::MAX;
        let src = format!(
            r#"<?php
use PHPUnit\Framework\TestCase;
final class Num {{
    public function __construct(private int $v) {{}}
    public static function of(int $v): self {{ return new Num($v); }}
    public function plus(Num $o): Num {{ return new Num($this->v + $o->v); }}
    public function value(): int {{ return $this->v; }}
}}
final class NumTest extends TestCase {{
    public function testOverflow(): void {{
        $r = Num::of({max})->plus(Num::of(1));
        $this->assertSame({max}, $r->value());
    }}
}}
"#
        );
        assert_eq!(decide(&src, "NumTest", "testOverflow"), Decision::Unknown);
    }

    // ─── v3: data-provider substitution + div/assert routing ──────────────────

    /// THE v3 target: a parametrized integer value-object `Frac`, exercised through a
    /// `#[DataProvider]` whose every row is `[a, b, c, d, expNum]`. Each row binds
    /// the test params to integer leaves; the body `Frac::of(a,b)->times(Frac::of(c,d))
    /// ->num()` reduces (derived `of`, `times`, `num` + fold) to `a*c`, compared to
    /// `expNum`. Rows: 1*3=3, 2*5=10, 0*9=0 — all True → method True. No execution.
    /// The real php8.4 PHPUnit gold-gate of `testTimesNum` (its 3 rows) PASSES.
    const FRAC_SRC: &str = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class Frac {
    public function __construct(private int $num, private int $den) {}
    public static function of(int $n, int $d): self { return new Frac($n, $d); }
    public function times(Frac $o): Frac { return new Frac($this->num * $o->num, $this->den * $o->den); }
    public function num(): int { return $this->num; }
    public function den(): int { return $this->den; }
}
final class FracTest extends TestCase {
    #[DataProvider('cases')]
    public function testTimesNum(int $a, int $b, int $c, int $d, int $expNum): void {
        $this->assertSame($expNum, Frac::of($a, $b)->times(Frac::of($c, $d))->num());
    }
    public static function cases(): array { return [[1, 2, 3, 4, 3], [2, 3, 5, 7, 10], [0, 1, 9, 9, 0]]; }
}
"#;

    #[test]
    fn frac_provider_times_decides_true() {
        assert_eq!(decide(FRAC_SRC, "FracTest", "testTimesNum"), Decision::True);
    }

    /// A provider whose LAST row expects the wrong product (`999` instead of `10`):
    /// that row decides False, the others True → the method is NEVER True. With every
    /// row decided (two True, one False, no Unknown) the aggregate is a coherent False.
    #[test]
    fn provider_with_a_false_row_decides_false_or_unknown() {
        let src = FRAC_SRC.replace("[2, 3, 5, 7, 10]", "[2, 3, 5, 7, 999]");
        let d = decide(&src, "FracTest", "testTimesNum");
        assert_ne!(d, Decision::True, "a false row must never decide True");
        assert_eq!(
            d,
            Decision::False,
            "all rows decided, one provably false → coherent False"
        );
    }

    /// SOUNDNESS: string Givens now BIND as leaves, but integer arithmetic over them does
    /// not fold (no ground constant) → the method stays fail-closed Unknown, never a false
    /// decision. (Before scalar provider rows this bailed earlier, at the provider; the
    /// verdict is unchanged — only the reason moved.)
    #[test]
    fn non_integer_provider_bails() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class Frac {
    public function __construct(private int $num, private int $den) {}
    public static function of(int $n, int $d): self { return new Frac($n, $d); }
    public function times(Frac $o): Frac { return new Frac($this->num * $o->num, $this->den * $o->den); }
    public function num(): int { return $this->num; }
}
final class FracTest extends TestCase {
    #[DataProvider('cases')]
    public function testTimesNum(int $a, int $b, int $c, int $d, int $expNum): void {
        $this->assertSame($expNum, Frac::of($a, $b)->times(Frac::of($c, $d))->num());
    }
    public static function cases(): array { return [["x", "y", "z", "w", "v"]]; }
}
"#;
        assert_eq!(decide(src, "FracTest", "testTimesNum"), Decision::Unknown);
    }

    /// A provider whose rows are computed by a `yield`/loop rather than a single
    /// array literal is not statically foldable → fail-closed Unknown.
    #[test]
    fn yield_provider_bails() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class Frac {
    public function __construct(private int $num, private int $den) {}
    public static function of(int $n, int $d): self { return new Frac($n, $d); }
    public function times(Frac $o): Frac { return new Frac($this->num * $o->num, $this->den * $o->den); }
    public function num(): int { return $this->num; }
}
final class FracTest extends TestCase {
    #[DataProvider('cases')]
    public function testTimesNum(int $a, int $b, int $c, int $d, int $expNum): void {
        $this->assertSame($expNum, Frac::of($a, $b)->times(Frac::of($c, $d))->num());
    }
    public static function cases(): array { yield [1, 2, 3, 4, 3]; }
}
"#;
        assert_eq!(decide(src, "FracTest", "testTimesNum"), Decision::Unknown);
    }

    const DIV_SRC: &str = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class Ratio {
    public function __construct(private int $v) {}
    public static function of(int $v): self { return new Ratio($v); }
    public function div(Ratio $o): Ratio { return new Ratio($this->v / $o->v); }
    public function value(): int { return $this->v; }
}
final class RatioTest extends TestCase {
    #[DataProvider('cases')]
    public function testDiv(int $a, int $b, int $exp): void {
        $this->assertSame($exp, Ratio::of($a)->div(Ratio::of($b))->value());
    }
    public static function cases(): array { return [[6, 2, 3], [10, 5, 2]]; }
}
"#;

    /// Integer division that is EXACT in every row (`6/2=3`, `10/5=2`) folds → True.
    #[test]
    fn division_exact_folds() {
        assert_eq!(decide(DIV_SRC, "RatioTest", "testDiv"), Decision::True);
    }

    /// A row whose division is INEXACT (`7/2` → PHP float `3.5`, unmodelled) does not
    /// fold → fail-closed Unknown (NOT a wrong True/False).
    #[test]
    fn division_inexact_fail_closed() {
        // Replace the exact rows with one inexact one (`7/2`).
        let src = DIV_SRC.replace("[[6, 2, 3], [10, 5, 2]]", "[[7, 2, 3]]");
        assert_eq!(decide(&src, "RatioTest", "testDiv"), Decision::Unknown);
    }

    /// Division by zero is fail-closed too (PHP raises, the fragment does not model
    /// it) → Unknown, never a fold.
    #[test]
    fn division_by_zero_fail_closed() {
        let src = DIV_SRC.replace("[[6, 2, 3], [10, 5, 2]]", "[[6, 0, 3]]");
        assert_eq!(decide(&src, "RatioTest", "testDiv"), Decision::Unknown);
    }

    /// `assertEquals(expNum, …)` on concrete integers routes exactly like
    /// `assertSame` (loose `==` coincides with i64 equality for concrete ints).
    #[test]
    fn assert_equals_int_routes() {
        let src = FRAC_SRC.replace("assertSame(", "assertEquals(");
        assert_eq!(decide(&src, "FracTest", "testTimesNum"), Decision::True);
    }

    /// `assertNotSame` INVERTS the verdict: a body that would be True under
    /// `assertSame` becomes False under `assertNotSame`.
    #[test]
    fn assert_not_same_inverts() {
        let src = FRAC_SRC.replace("assertSame(", "assertNotSame(");
        assert_eq!(decide(&src, "FracTest", "testTimesNum"), Decision::False);
    }

    /// A single inline `#[TestWith([...])]` row is bound and decided exactly like a
    /// one-row provider.
    #[test]
    fn testwith_inline_row() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\TestWith;
final class Frac {
    public function __construct(private int $num, private int $den) {}
    public static function of(int $n, int $d): self { return new Frac($n, $d); }
    public function times(Frac $o): Frac { return new Frac($this->num * $o->num, $this->den * $o->den); }
    public function num(): int { return $this->num; }
}
final class FracTest extends TestCase {
    #[TestWith([2, 3, 5, 7, 10])]
    public function testTimesNum(int $a, int $b, int $c, int $d, int $expNum): void {
        $this->assertSame($expNum, Frac::of($a, $b)->times(Frac::of($c, $d))->num());
    }
}
"#;
        assert_eq!(decide(src, "FracTest", "testTimesNum"), Decision::True);
    }

    /// A parametrized method WITHOUT any provider stays Unknown (PHPUnit would raise
    /// ArgumentCountError; the engine never invents rows).
    #[test]
    fn parametrized_without_provider_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Frac {
    public function __construct(private int $num, private int $den) {}
    public static function of(int $n, int $d): self { return new Frac($n, $d); }
    public function num(): int { return $this->num; }
}
final class FracTest extends TestCase {
    public function testTimesNum(int $a, int $b): void {
        $this->assertSame($a, Frac::of($a, $b)->num());
    }
}
"#;
        assert_eq!(decide(src, "FracTest", "testTimesNum"), Decision::Unknown);
    }

    /// The legacy `/** @dataProvider cases */` docblock is honoured exactly like the
    /// attribute form.
    #[test]
    fn docblock_data_provider_decides_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class Frac {
    public function __construct(private int $num, private int $den) {}
    public static function of(int $n, int $d): self { return new Frac($n, $d); }
    public function times(Frac $o): Frac { return new Frac($this->num * $o->num, $this->den * $o->den); }
    public function num(): int { return $this->num; }
}
final class FracTest extends TestCase {
    /**
     * @dataProvider cases
     */
    public function testTimesNum(int $a, int $b, int $c, int $d, int $expNum): void {
        $this->assertSame($expNum, Frac::of($a, $b)->times(Frac::of($c, $d))->num());
    }
    public static function cases(): array { return [[1, 2, 3, 4, 3], [2, 3, 5, 7, 10]]; }
}
"#;
        assert_eq!(decide(src, "FracTest", "testTimesNum"), Decision::True);
    }

    // ─── v1 mechanics, re-proved on the OPEN signature ────────────────────────
    //
    // The original v1 tests built a closed `define_language!` enum and hand-wrote
    // three rules. v2 has no such enum; these re-express the SAME five proofs over
    // the open `SymbolLang` signature, driving GroundEval + a programmatic rule set
    // directly, so the mechanics that v1 covered stay covered.

    /// A small programmatic rule set equivalent to v1's `Money` fragment, built the
    /// v2 way (motifs → patterns → `Rewrite::new`).
    fn money_rules() -> Vec<Rewrite<PhpL, GroundEval>> {
        let mk = |name: &str, lhs: &str, rhs: &str| {
            let l: Pattern<PhpL> = lhs.parse().unwrap();
            let r: Pattern<PhpL> = rhs.parse().unwrap();
            Rewrite::new(name, l, r).unwrap()
        };
        vec![
            mk("getamount-money", "(getAmount (Money ?a))", "?a"),
            mk("plus-money", "(plus (Money ?a) ?b)", "(Money (+ ?a ?b))"),
            mk("comm-add", "(+ ?a ?b)", "(+ ?b ?a)"),
        ]
    }

    fn saturate(exprs: &[&str]) -> (EGraph<PhpL, GroundEval>, Vec<Id>) {
        let mut egraph: EGraph<PhpL, GroundEval> = EGraph::default();
        let roots: Vec<Id> = exprs
            .iter()
            .map(|s| {
                let e: egg::RecExpr<PhpL> = s.parse().unwrap();
                egraph.add_expr(&e)
            })
            .collect();
        let runner = Runner::default().with_egraph(egraph).run(&money_rules());
        let egraph = runner.egraph;
        let roots = roots.iter().map(|id| egraph.find(*id)).collect();
        (egraph, roots)
    }

    /// T1 (v1): `8` ≡ `(getAmount (plus (Money 5) 3))` by congruence.
    #[test]
    fn t1_congruent() {
        let (g, r) = saturate(&["8", "(getAmount (plus (Money 5) 3))"]);
        assert_eq!(g.find(r[0]), g.find(r[1]));
    }

    /// T2 (v1): `8` ≡ `(getAmount (Money 8))` (accessor body only).
    #[test]
    fn t2_congruent() {
        let (g, r) = saturate(&["8", "(getAmount (Money 8))"]);
        assert_eq!(g.find(r[0]), g.find(r[1]));
    }

    /// Cache/minimisation (v1): the PRODUCED `Money(8)` and the CONSTRUCTED one
    /// share one e-class.
    #[test]
    fn partage_money8() {
        let (g, r) = saturate(&[
            "(getAmount (plus (Money 5) 3))",
            "(getAmount (Money 8))",
            "(Money (+ 5 3))",
            "(Money 8)",
        ]);
        assert_eq!(g.find(r[2]), g.find(r[3]));
    }

    /// Soundness (v1): congruence is not abusive fusion — `7` and `9` are NOT
    /// congruent to a term reducing to `8`.
    #[test]
    fn non_fusion_sound() {
        let (g, r) = saturate(&["7", "(getAmount (plus (Money 5) 3))", "9"]);
        assert_ne!(g.find(r[0]), g.find(r[1]));
        assert_ne!(g.find(r[2]), g.find(r[1]));
    }

    /// Fail-closed totality (v1): `(+ i64::MAX 1)` overflows → no folded constant,
    /// and the surrounding term is not congruent to any concrete literal.
    #[test]
    fn overflow_pas_de_fold() {
        let max = i64::MAX;
        let (g, r) = saturate(&[&format!("(+ {max} 1)")]);
        assert_eq!(g[r[0]].data, None);

        let (g2, r2) = saturate(&[
            &format!("(getAmount (Money (+ {max} 1)))"),
            &format!("{max}"),
        ]);
        assert_ne!(g2.find(r2[0]), g2.find(r2[1]));
    }

    // ─── Data-provider row substitution in the COMPRESSION path ─────────────────

    /// THE provider-substitution proof: ONE parametrized method whose data provider
    /// has THREE rows that all bind `$n = 0` builds `BigInteger::of(0)` three times —
    /// with the param SUBSTITUTED to the concrete leaf `0`, those three constructions
    /// collapse to ONE e-class. Before substitution `$n` was a per-test salted free
    /// leaf, so the three `of($n)` could not share. `n_naive` counts the body once per
    /// row (3×); the shared `BigInteger::of 0` is the dominant cost target at mult 3.
    #[test]
    fn provider_rows_substitute_and_share_across_rows() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class ProvTest extends TestCase {
    public static function zeros(): array {
        return [[0], [0], [0]];
    }
    #[DataProvider('zeros')]
    public function testOf(int $n): void {
        $this->assertSame('0', BigInteger::of($n)->__toString());
    }
}
"#;
        let s = compress(src, "ProvTest");
        // The body materialises once per provider row.
        assert!(
            s.tests >= 1 && s.n_naive > 0,
            "the parametrized body contributed nodes: {s:?}"
        );
        // The thrice-built `BigInteger::of 0` is shared into ONE class → it tops the
        // targets at multiplicity 3 (substitution made `$n` concrete `0`, fusing them).
        let of_target = s
            .top_targets
            .iter()
            .find(|t| t.op == "BigInteger::of")
            .expect("the substituted constructor is a cost target");
        assert_eq!(
            of_target.mult, 3,
            "all 3 rows' `BigInteger::of(0)` share one class: {:?}",
            s.top_targets
        );
        assert!(
            s.cost_compression() > 1.0,
            "substituted shared calls lift cost_compression: {:.2}",
            s.cost_compression()
        );
    }

    /// Inter-METHOD sharing through providers: two DIFFERENT parametrized methods whose
    /// providers each yield a row binding the same `0` both build `BigInteger::of(0)`.
    /// With substitution the two constructions (in different test bodies) fuse into ONE
    /// e-class — the cross-test sharing the integer-only path could never see.
    #[test]
    fn provider_substitution_shares_across_methods() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class TwoProvTest extends TestCase {
    public static function zero(): array { return [[0]]; }
    #[DataProvider('zero')]
    public function testA(int $n): void {
        $this->assertSame('0', BigInteger::of($n)->__toString());
    }
    #[DataProvider('zero')]
    public function testB(int $n): void {
        $this->assertTrue(BigInteger::of($n)->isZero());
    }
}
"#;
        let s = compress(src, "TwoProvTest");
        let of_target = s
            .top_targets
            .iter()
            .find(|t| t.op == "BigInteger::of")
            .expect("the shared constructor is a cost target");
        assert_eq!(
            of_target.mult, 2,
            "both methods' `BigInteger::of(0)` share one class: {:?}",
            s.top_targets
        );
    }

    /// Mixed-type Givens share too: a `#[TestWith]` carrying a STRING `'0'` and another
    /// carrying `'0'` build `BigInteger::of('0')` that fuses on the `str:'0'` leaf —
    /// proving substitution handles ANY literal (here string), not just integers.
    #[test]
    fn testwith_string_givens_share() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\TestWith;
final class StrProvTest extends TestCase {
    #[TestWith(['0'])]
    #[TestWith(['0'])]
    public function testOf(string $s): void {
        $this->assertSame('0', BigInteger::of($s)->__toString());
    }
}
"#;
        let s = compress(src, "StrProvTest");
        let of_target = s
            .top_targets
            .iter()
            .find(|t| t.op == "BigInteger::of")
            .expect("the string-substituted constructor is a cost target");
        assert_eq!(
            of_target.mult, 2,
            "both `'0'` rows' constructor share one class: {:?}",
            s.top_targets
        );
    }

    /// Soundness: substituting DISTINCT provider Givens must NOT fuse. Two rows binding
    /// `1` and `2` build `BigInteger::of(1)` and `BigInteger::of(2)` — distinct leaves,
    /// distinct constructions, so the constructor class is NOT shared at multiplicity 2.
    #[test]
    fn distinct_provider_givens_do_not_fuse() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class DistinctProvTest extends TestCase {
    public static function ones_and_twos(): array { return [[1], [2]]; }
    #[DataProvider('ones_and_twos')]
    public function testOf(int $n): void {
        $this->assertSame((string) $n, BigInteger::of($n)->__toString());
    }
}
"#;
        let s = compress(src, "DistinctProvTest");
        // Each distinct Given builds its own `BigInteger::of` class — neither reaches
        // multiplicity 2 (no abusive fusion of different concrete arguments).
        let max_of_mult = s
            .top_targets
            .iter()
            .filter(|t| t.op == "BigInteger::of")
            .map(|t| t.mult)
            .max()
            .unwrap_or(0);
        assert!(
            max_of_mult < 2,
            "distinct Givens must keep distinct constructor classes: {:?}",
            s.top_targets
        );
    }

    /// Fail-safe: a provider row that is NOT a substitutable literal (a nested array
    /// column) leaves that param salted for the row — no panic, no false fusion, and
    /// the rest of the suite still measures. Here the single column is an array, so the
    /// param stays free; the body still contributes (the measure does not bail).
    #[test]
    fn non_literal_provider_column_stays_salted() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use PHPUnit\Framework\Attributes\DataProvider;
final class ArrProvTest extends TestCase {
    public static function arrays(): array { return [[[1, 2]], [[3, 4]]]; }
    #[DataProvider('arrays')]
    public function testOf(array $xs): void {
        $this->assertNotNull(BigInteger::of($xs));
    }
}
"#;
        let s = compress(src, "ArrProvTest");
        // The measure did not bail: the body contributed nodes across the 2 rows.
        assert!(
            s.tests >= 1 && s.n_naive > 0,
            "non-literal column must not bail the measure: {s:?}"
        );
    }
}
