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

/// Collect the field layout of every class declared at the top level of `program`
/// (descending one level of namespaces, like the bridge's class finder). A class
/// whose ctor cannot be modelled (no ctor = empty layout; a non-`$this->x=` ctor
/// body just contributes its promoted params) still gets an entry — the layout is
/// only used to expand `$this`/typed-class params, and a method referencing an
/// unmapped class simply fails to derive (fail-closed).
fn collect_class_catalogue(program: &Program) -> ClassCatalogue {
    let mut cat = ClassCatalogue::new();
    collect_from_statements(program.statements.iter(), &mut cat);
    cat
}

fn collect_from_statements<'a, I>(stmts: I, cat: &mut ClassCatalogue)
where
    I: Iterator<Item = &'a Statement<'a>>,
{
    for stmt in stmts {
        match stmt {
            Statement::Class(class) => {
                let name = String::from_utf8_lossy(class.name.value).to_ascii_lowercase();
                cat.insert(name, field_layout_of(class));
            }
            Statement::Namespace(ns) => {
                collect_from_statements(ns.statements().iter(), cat);
            }
            _ => {}
        }
    }
}

/// The field layout of a class: promoted ctor params (in order), then any field
/// first written by a `$this->x = …` statement in the ctor body (in order, deduped).
fn field_layout_of(class: &Class) -> FieldLayout {
    let mut layout: FieldLayout = Vec::new();
    let Some(ctor) = find_method_in_class(class, b"__construct") else {
        return layout;
    };
    for p in ctor.parameter_list.parameters.iter() {
        if p.is_promoted_property() {
            let name = String::from_utf8_lossy(&strip_dollar(p.variable.name)).into_owned();
            push_unique(&mut layout, name);
        }
    }
    if let MethodBody::Concrete(block) = &ctor.body {
        for stmt in block.statements.iter() {
            if let Some(field) = ctor_assignment_field(stmt) {
                push_unique(&mut layout, field);
            }
        }
    }
    layout
}

/// If `stmt` is a `$this->field = <expr>;` statement, the field name.
fn ctor_assignment_field(stmt: &Statement) -> Option<String> {
    let Statement::Expression(es) = stmt else {
        return None;
    };
    let Expression::Assignment(a) = es.expression else {
        return None;
    };
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
    Some(String::from_utf8_lossy(prop_id.value).into_owned())
}

fn push_unique(layout: &mut FieldLayout, name: String) {
    if !layout.contains(&name) {
        layout.push(name);
    }
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

/// Derive the full rule set from the test file: walk every class, and for each pure
/// `{ return <expr>; }` method/factory emit one oriented rewrite. A method that is
/// not a single return, or whose return expression leaves the modelled fragment,
/// contributes NO rule (it stays an opaque symbol → congruence cannot fire).
///
/// Returns the rules AND, for diagnostics/tests, the human-readable `(lhs) => (rhs)`
/// of each derived rule.
fn derive_rules(program: &Program) -> (Vec<Rewrite<PhpL, GroundEval>>, Vec<String>) {
    let cat = collect_class_catalogue(program);
    let mut rules = Vec::new();
    let mut descriptions = Vec::new();
    derive_from_statements(
        program.statements.iter(),
        &cat,
        &mut rules,
        &mut descriptions,
    );
    (rules, descriptions)
}

fn derive_from_statements<'a, I>(
    stmts: I,
    cat: &ClassCatalogue,
    rules: &mut Vec<Rewrite<PhpL, GroundEval>>,
    descriptions: &mut Vec<String>,
) where
    I: Iterator<Item = &'a Statement<'a>>,
{
    for stmt in stmts {
        match stmt {
            Statement::Class(class) => derive_from_class(class, cat, rules, descriptions),
            Statement::Namespace(ns) => {
                derive_from_statements(ns.statements().iter(), cat, rules, descriptions)
            }
            _ => {}
        }
    }
}

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

    project.with_program(&logical, |program, _src, _names| {
        decide_with_program(program, &class_fqcn, method)
    })?
}

fn decide_with_program(program: &Program, class_fqcn: &[u8], method: &str) -> Option<Decision> {
    // 1. Derive the equations from every class in the file.
    let (rules, _descs) = derive_rules(program);

    // 2. Locate the test method and its final `assertSame(L, R)` arguments.
    let m = find_class_method(program, class_fqcn, method.as_bytes())?;
    let MethodBody::Concrete(block) = &m.body else {
        return None;
    };
    // A parametrized test would need provider rows; this v2 engine targets concrete
    // zero-param assertions (the static-factory showcase), so a param list bails.
    if !m.parameter_list.parameters.is_empty() {
        return None;
    }
    let (lhs_expr, rhs_expr, bindings) = collect_assertion(block)?;

    // 3. Build both sides into ONE e-graph and bind any `$r = …` locals.
    let cat = collect_class_catalogue(program);
    let mut egraph: EGraph<PhpL, GroundEval> = EGraph::default();
    let mut vars: HashMap<Vec<u8>, Id> = HashMap::new();
    for (name, expr) in &bindings {
        let id = {
            let mut b = ExprBuilder {
                egraph: &mut egraph,
                cat: &cat,
                vars: &vars,
            };
            b.build(expr)?
        };
        vars.insert(name.clone(), id);
    }
    let l_id = {
        let mut b = ExprBuilder {
            egraph: &mut egraph,
            cat: &cat,
            vars: &vars,
        };
        b.build(lhs_expr)?
    };
    let r_id = {
        let mut b = ExprBuilder {
            egraph: &mut egraph,
            cat: &cat,
            vars: &vars,
        };
        b.build(rhs_expr)?
    };

    // 4. Saturate with the derived rules + ground folding, decide by congruence.
    let runner = Runner::default().with_egraph(egraph).run(&rules);
    let egraph = runner.egraph;
    if egraph.find(l_id) == egraph.find(r_id) {
        return Some(Decision::True);
    }
    // A sound definitive False: both sides are DISTINCT known constants.
    if let (Some(a), Some(b)) = (egraph[l_id].data, egraph[r_id].data) {
        if a != b {
            return Some(Decision::False);
        }
    }
    Some(Decision::Unknown)
}

/// The arguments of a test body's `assertSame(L, R)`: the two operand expressions
/// plus the `$var = <expr>` locals bound before it (each as name → its rhs expr).
type Assertion<'a> = (
    &'a Expression<'a>,
    &'a Expression<'a>,
    Vec<(Vec<u8>, &'a Expression<'a>)>,
);

/// Walk a test body's pure prefix: `$var = <expr>;` assignments bind locals, and a
/// final `assertSame(L, R)` (method or bare-function form) yields its two argument
/// expressions plus the collected bindings. Anything else bails.
fn collect_assertion<'a>(
    block: &'a mago_syntax::ast::ast::block::Block<'a>,
) -> Option<Assertion<'a>> {
    use mago_syntax::ast::ast::assignment::AssignmentOperator;
    let mut bindings: Vec<(Vec<u8>, &Expression)> = Vec::new();
    for stmt in block.statements.iter() {
        let Statement::Expression(es) = stmt else {
            return None;
        };
        // An assertSame call?
        if let Some((l, r)) = assert_same_args(es.expression) {
            return Some((l, r, bindings));
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

/// If `expr` is `assertSame(L, R)` (as `$this->assertSame(...)`, `self::assertSame`,
/// or the bare function), the two argument expressions. A trailing message arg is
/// allowed and ignored.
fn assert_same_args<'a>(
    expr: &'a Expression<'a>,
) -> Option<(&'a Expression<'a>, &'a Expression<'a>)> {
    let Expression::Call(call) = expr else {
        return None;
    };
    let (name, args) = match call {
        Call::Method(m) => {
            let ClassLikeMemberSelector::Identifier(id) = &m.method else {
                return None;
            };
            (id.value, &m.argument_list)
        }
        Call::StaticMethod(sm) => {
            let ClassLikeMemberSelector::Identifier(id) = &sm.method else {
                return None;
            };
            (id.value, &sm.argument_list)
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
            (n, &fc.argument_list)
        }
        _ => return None,
    };
    if !name.eq_ignore_ascii_case(b"assertSame") {
        return None;
    }
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
    if exprs.len() < 2 {
        return None;
    }
    Some((exprs[0], exprs[1]))
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
    /// on WHICH equations were derived).
    fn derived_descriptions(src: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Code.php"), src).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        let logical = "Code.php";
        project
            .with_program(logical, |program, _src, _names| {
                let (_rules, descs) = derive_rules(program);
                descs
            })
            .unwrap()
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
