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
use egg::{Language, SymbolLang, Var};

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
use crate::concrete::{compute, Context, PhpValue};
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
    let Some(ctor) = find_method_in_class(class, b"__construct") else {
        return Some(Vec::new());
    };
    // The body's pure pass-throughs: source-param → field. ANY other statement
    // (throw, if, normalisation, a non-`$this->f=$param` assignment) ⇒ opaque.
    let mut passthrough: HashMap<Vec<u8>, String> = HashMap::new();
    if let MethodBody::Concrete(block) = &ctor.body {
        for stmt in block.statements.iter() {
            let (field, src_param) = pure_passthrough_assignment(stmt)?;
            // A field assigned twice, or two fields from one param, breaks the 1:1
            // positional mapping ⇒ opaque (fail-closed).
            if passthrough.values().any(|f| *f == field) || passthrough.contains_key(&src_param) {
                return None;
            }
            passthrough.insert(src_param, field);
        }
    }
    // Build the layout in PARAMETER order, so slot K ≡ construction arg K.
    let mut layout: FieldLayout = Vec::new();
    let mut used = 0usize;
    for p in ctor.parameter_list.parameters.iter() {
        // Variadic / by-ref ctor params are not modelled positionally ⇒ opaque.
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
            // A ctor param that flows into NO field cannot be a pure seed ⇒ opaque.
            return None;
        };
        layout.push(field);
    }
    // Every body pass-through must have been consumed by a param (no stray write).
    if used != passthrough.len() {
        return None;
    }
    Some(layout)
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
    for member in class.members.iter() {
        let ClassLikeMember::Method(m) = member else {
            continue;
        };
        let method_name = String::from_utf8_lossy(m.name.value).into_owned();
        // The constructor is captured by the field layout, not as a rewrite.
        if method_name.eq_ignore_ascii_case("__construct") {
            continue;
        }
        if let Some(rule) = derive_method_rule(&class_name, m, cat) {
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
fn derive_method_rule(class_name: &str, m: &Method, cat: &ClassCatalogue) -> Option<DerivedRule> {
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
    let rewrite = Rewrite::new(rule_name, lhs_pat, rhs_pat).ok()?;
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
fn literal_motif(lit: &Literal) -> Option<Motif> {
    match lit {
        Literal::Integer(i) => i.value.map(|v| Motif::leaf((v as i64).to_string())),
        _ => None,
    }
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

// ─── Shared AST helpers ────────────────────────────────────────────────────────

fn instantiation_class_name(inst: &Instantiation) -> Option<Vec<u8>> {
    identifier_class_name(inst.class)
}

fn static_call_class_name(class: &Expression) -> Option<Vec<u8>> {
    identifier_class_name(class)
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

// ─── Data-provider substitution: rows of integer leaves ────────────────────────

/// The statically-evaluated provider rows for a PARAMETRIZED test method, each a
/// tuple of integer leaves bound positionally to the method's parameters. Returns
/// `None` (fail-closed) when the method has no derivable provider, the provider is
/// not a pure integer-array literal (a `yield`/loop/string/computed row, an external
/// provider, …), or any leaf escapes the integer fragment — the runner then executes
/// the method for real. PHPUnit treats each row as a separate test; we decide the
/// method by aggregating across rows.
///
/// Three provider shapes are honoured, exactly as real php8.4 PHPUnit binds them:
///   * `#[DataProvider('name')]` — the static method `name()` whose single `return`
///     is an `array` of integer-array rows;
///   * `#[TestWith([..])]` — one inline integer row per attribute;
///   * the legacy `/** @dataProvider name */` docblock — same as the attribute.
fn provider_rows(
    program: &Program,
    source_text: &str,
    class_fqcn: &[u8],
    method: &Method,
) -> Option<Vec<Vec<i64>>> {
    // `#[TestWith([..])]` rows live ON the method — collect them first.
    let test_with = test_with_rows(method);
    if !test_with.is_empty() {
        return Some(test_with);
    }
    // Otherwise a `#[DataProvider('name')]` attribute or a `@dataProvider name`
    // docblock names a sibling static provider method.
    let provider_name = data_provider_name(source_text, method)?;
    let provider = find_class_method(program, class_fqcn, provider_name.as_bytes())?;
    static_provider_rows(provider)
}

/// The `#[TestWith([..])]` rows declared directly on `method`: each `TestWith`
/// attribute carries one positional array literal → one integer row. A non-integer or
/// non-array-literal `TestWith` makes the WHOLE method fail-closed (we cannot soundly
/// model a partial set), so a present-but-unmodellable row returns an empty Vec via
/// `None`-coalescing the row, then the caller's emptiness check sends it to the
/// provider path; to keep that unambiguous a malformed `TestWith` simply contributes
/// no row and the method falls through to Unknown if no rows remain.
fn test_with_rows(method: &Method) -> Vec<Vec<i64>> {
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
            if let Some(row) = int_row_from_expr(p.value) {
                rows.push(row);
            }
        }
    }
    rows
}

/// Evaluate a static data-provider method (`{ return [ [..], [..] ]; }`) to integer
/// rows. The body must be a single `return` of an `array` LITERAL whose every element
/// is itself an integer-array literal (positional or `key => [..]`). Anything else —
/// `yield`, a computed expression, strings, nested non-integers — bails (`None`).
fn static_provider_rows(provider: &Method) -> Option<Vec<Vec<i64>>> {
    let MethodBody::Concrete(block) = &provider.body else {
        return None;
    };
    let ret = single_return_expr(block)?;
    rows_from_array_literal(ret)
}

/// A `[ [..], [..] ]` (or `[ 'k' => [..] ]`) outer array literal → its rows, each an
/// integer-array literal. `None` if `expr` is not an array literal, or any row is not
/// an integer-array literal.
fn rows_from_array_literal(expr: &Expression) -> Option<Vec<Vec<i64>>> {
    let mut ctx = Context::new();
    let value = compute(expr, &mut ctx).ok()?;
    let PhpValue::Array(outer) = value else {
        return None;
    };
    let mut rows = Vec::new();
    for (_key, row_val) in outer {
        rows.push(int_row_from_phpvalue(&row_val)?);
    }
    Some(rows)
}

/// One row literal (`[1, 2, 3]`) → its integer columns. `None` if `expr` does not
/// concretely evaluate to an array of integers.
fn int_row_from_expr(expr: &Expression) -> Option<Vec<i64>> {
    let mut ctx = Context::new();
    let value = compute(expr, &mut ctx).ok()?;
    int_row_from_phpvalue(&value)
}

/// A concretely-evaluated `PhpValue` row → its integer columns. Every element must be
/// a (`PhpValue::Int`); a string/float/bool/null/nested array fails the whole row
/// (fail-closed — this fragment models only integer value-objects).
fn int_row_from_phpvalue(value: &PhpValue) -> Option<Vec<i64>> {
    let PhpValue::Array(map) = value else {
        return None;
    };
    let mut cols = Vec::new();
    for v in map.values() {
        match v {
            PhpValue::Int(i) => cols.push(*i),
            _ => return None,
        }
    }
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

    // 2. The test method's parameters and its final assertion.
    let MethodBody::Concrete(block) = &m.body else {
        return None;
    };
    let assertion = collect_assertion(block)?;

    // 3. Build the substitution rows. A zero-param method has exactly ONE empty row
    //    (the v2 static-factory path). A parametrized method binds its params,
    //    positionally, from a STATICALLY-evaluated data provider — every row a tuple
    //    of integer leaves. A parametrized method with no derivable provider, or a
    //    provider that is not a pure integer-array literal, yields no rows → the whole
    //    method is fail-closed Unknown (the runner then executes it for real).
    let params: Vec<Vec<u8>> = m
        .parameter_list
        .parameters
        .iter()
        .map(|p| strip_dollar(p.variable.name))
        .collect();
    let rows: Vec<Vec<i64>> = if params.is_empty() {
        vec![Vec::new()]
    } else {
        provider_rows(program, source_text, class_fqcn, m)?
    };

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
    row: &[i64],
    assertion: &Assertion,
) -> Option<Decision> {
    let mut egraph: EGraph<PhpL, GroundEval> = EGraph::default();
    let mut vars: HashMap<Vec<u8>, Id> = HashMap::new();

    // Bind parameters positionally to integer leaves. PHPUnit binds row columns to
    // params by position; SURPLUS columns are ignored (not an error in PHPUnit), and
    // a row SHORTER than the param list cannot satisfy the call → bail.
    if row.len() < params.len() {
        return None;
    }
    for (name, value) in params.iter().zip(row.iter()) {
        let id = egraph.add(SymbolLang::leaf(value.to_string()));
        vars.insert(name.clone(), id);
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

    /// A value-object whose constructor VALIDATES/NORMALISES (throws on a bad arg) is
    /// NOT a simple promoted-param/`$this->x=<pure>` seed, so NO construction rule is
    /// derivable → the class stays opaque → Unknown (fail-closed). We must NEVER force
    /// a `(C a b)` node when the ctor could reject or rewrite its inputs.
    #[test]
    fn validating_ctor_stays_unknown() {
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
            Decision::Unknown
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

    /// A provider that is not a pure integer-array literal (strings here) leaves the
    /// modelled fragment → the whole method is fail-closed Unknown.
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
}
