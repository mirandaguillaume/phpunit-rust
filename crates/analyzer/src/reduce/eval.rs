//! The native evaluator — the CENTER of the reducer.
//!
//! A test, given its complete Givens (data-provider row + fixtures), is a trivial
//! deterministic computation. This module performs that computation NATIVELY: it
//! evaluates the test-method body over the concrete Given inputs, running the few
//! trivial ops a test actually touches — arithmetic (with PHP overflow→float),
//! concat, comparisons (via [`super::value`]), array ops, control flow, and the
//! assertion intrinsics — folding the whole thing to [`Outcome`].
//!
//! Every PHP-semantic op is gold-tested against host `php -r` (expectations
//! transcribed, never guessed). Anything outside the modelled set returns
//! [`Outcome::Bailed`] / a [`BailReason`] — fail-closed (spec §5). The standing
//! differential (reduce vs the real runner) is the soundness backstop and is
//! driven separately.
//!
//! Substitution of user-function calls (inlining their bodies) is layered on in
//! Task 5 via a resolver hook; this module evaluates a single body with the
//! resolver pluggable.

use std::collections::HashMap;

use mago_syntax::ast::ast::array::{Array, ArrayElement, LegacyArray};
use mago_syntax::ast::ast::binary::BinaryOperator;
use mago_syntax::ast::ast::block::Block;
use mago_syntax::ast::ast::call::Call;
use mago_syntax::ast::ast::expression::Expression;
use mago_syntax::ast::ast::literal::Literal;
use mago_syntax::ast::ast::statement::Statement;
use mago_syntax::ast::ast::unary::UnaryPrefixOperator;
use mago_syntax::ast::ast::variable::Variable;

use mago_span::HasSpan;

use super::value::{ArrayKey, ClosureRef, Value};

// ─── Outcome + bail taxonomy ──────────────────────────────────────────────────

/// The reduced result of one test invocation (one provider row).
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Every assertion passed (or the body ran to completion with no failing
    /// assertion).
    Pass,
    /// An assertion failed; the string is the failed-assertion description.
    Fail(String),
    /// The reducer could not model some construct/value — the test must run for
    /// real. NEVER a guess.
    Bailed(BailReason),
}

/// Why the reducer abstained. Structured so the driver can histogram bails.
#[derive(Debug, Clone, PartialEq)]
pub enum BailReason {
    /// An AST construct outside the modelled subset (the payload names it).
    UnsupportedConstruct(String),
    /// A reference to an unbound variable (a hidden input — incomplete Givens).
    UnboundVariable(String),
    /// A function/method call the reducer does not model (and §5 substitution
    /// did not resolve).
    UnknownCall(String),
    /// A type error the reducer refuses to model (e.g. arithmetic on an array).
    TypeError(String),
    /// `assertEquals` with a delta / other float-epsilon assertion (spec §12.2).
    FloatDelta,
    /// Division or modulo by zero (PHP throws `DivisionByZeroError`; we abstain
    /// rather than model the exception path).
    DivisionByZero,
    /// The step budget was exhausted (runaway loop) — abstain.
    StepBudget,
    /// Anything else unmodelled.
    Other(String),
}

impl BailReason {
    /// A short stable tag for histogramming.
    pub fn tag(&self) -> &'static str {
        match self {
            BailReason::UnsupportedConstruct(_) => "unsupported_construct",
            BailReason::UnboundVariable(_) => "unbound_variable",
            BailReason::UnknownCall(_) => "unknown_call",
            BailReason::TypeError(_) => "type_error",
            BailReason::FloatDelta => "float_delta",
            BailReason::DivisionByZero => "division_by_zero",
            BailReason::StepBudget => "step_budget",
            BailReason::Other(_) => "other",
        }
    }
}

// ─── A resolver hook for user-function substitution (filled in Task 5) ────────

/// Resolves a user function/method call to a concrete [`Value`] by inlining its
/// body (substitution; spec §12.3). Task 5 supplies a real implementation backed
/// by the mago bridge; Task 4 uses [`NoResolver`] (every user call bails).
pub trait CallResolver {
    /// Attempt to resolve a **free function** call `name(args)`. `Ok(None)` means
    /// "not a user function I can resolve" (→ caller treats as unknown call);
    /// `Err` propagates a bail.
    fn resolve_function(&self, name: &[u8], args: &[Value]) -> Result<Option<Value>, BailReason>;

    /// Attempt to inline an **instance method** call `$obj->method(args)`. The
    /// receiver `this` is the concrete runtime [`Value::Object`] (its `class` gives
    /// the exact type — never a static type, spec §13). `Ok(None)` → not a method
    /// the resolver can inline (caller bails); `Err` propagates a bail.
    ///
    /// Increment-2 default: no instance-method inlining (the [`NoResolver`] path).
    fn resolve_instance_method(
        &self,
        _this: &Value,
        _method: &[u8],
        _args: &[Value],
    ) -> Result<Option<Value>, BailReason> {
        Ok(None)
    }

    /// Attempt to inline a **static method** call `Class::method(args)`. `class` is
    /// the resolved FQCN (`self`/`parent`/`static` are bailed by the caller — no
    /// enclosing-class context in the [`Scope`]).
    fn resolve_static_method(
        &self,
        _class: &[u8],
        _method: &[u8],
        _args: &[Value],
    ) -> Result<Option<Value>, BailReason> {
        Ok(None)
    }

    /// Attempt to construct `new Class(args)`: inline the constructor over a fresh
    /// `$this` record (promoted params seed props; plain literal property defaults
    /// are read off the class AST) and return the populated [`Value::Object`].
    fn construct(&self, _class: &[u8], _args: &[Value]) -> Result<Option<Value>, BailReason> {
        Ok(None)
    }
}

/// A resolver that resolves nothing (all user calls bail). Used by the pure-eval
/// unit tests; the real substitution is [`super::subst::BridgeResolver`].
pub struct NoResolver;

impl CallResolver for NoResolver {
    fn resolve_function(&self, _name: &[u8], _args: &[Value]) -> Result<Option<Value>, BailReason> {
        Ok(None)
    }
}

// ─── Evaluation scope ─────────────────────────────────────────────────────────

/// Lexical scope: by-value variable bindings + a step budget. Any PHP reference
/// would have made the frontend bail before reaching here, so by-value is exact.
pub struct Scope<'r> {
    vars: HashMap<Vec<u8>, Value>,
    steps: u64,
    max_steps: u64,
    resolver: &'r dyn CallResolver,
    /// The resolved-names table for the CURRENT body's file (Inc-3): maps each
    /// name occurrence (by source position) to its fully-qualified name, so an
    /// unqualified / `use`-aliased `new ClassName` / `ClassName::m()` resolves to
    /// the real FQCN. `None` (unit tests / a body whose names weren't threaded)
    /// falls back to the raw identifier. Each inlined body carries the names of
    /// ITS declaring file (set when re-entering the evaluator via `with_program`).
    names: Option<&'r mago_names::ResolvedNames<'r>>,
    /// The raw source bytes of the file whose body is being evaluated. Used ONLY to
    /// slice a closure expression's span into owned bytes at creation
    /// (`make_closure`/`make_arrow`, Inc-4 Task 1) so the closure carries no arena
    /// borrow. `None` in scopes that never create a closure from this file (e.g. a
    /// re-parsed closure body's inner scope, where any nested closure literal is
    /// itself re-sliced from the inner snippet).
    source: Option<&'r [u8]>,
    /// Whether `$this->prop = ...` writes are permitted (true ONLY while a
    /// constructor body is being inlined to seed props). A write to `$this->prop`
    /// in any non-constructor body is a MUTATOR — fail-closed BAIL (frontier §2),
    /// because the by-value scope model would get aliasing wrong.
    allow_this_write: bool,
}

impl<'r> Scope<'r> {
    /// A fresh scope with the given initial variable bindings (the Givens).
    pub fn new(vars: HashMap<Vec<u8>, Value>, resolver: &'r dyn CallResolver) -> Self {
        Self {
            vars,
            steps: 0,
            // 10M steps — a runaway loop bails rather than hanging (spec §6).
            max_steps: 10_000_000,
            resolver,
            names: None,
            source: None,
            allow_this_write: false,
        }
    }

    /// Attach the resolved-names table for the body about to be evaluated, so
    /// class-name resolution at `new`/static-call sites can produce a FQCN.
    pub fn with_names(mut self, names: &'r mago_names::ResolvedNames<'r>) -> Self {
        self.names = Some(names);
        self
    }

    /// Attach the file source so a closure literal can own its source bytes
    /// (sliced by span) at creation — see [`Value::Closure`] (Inc-4 Task 1).
    pub fn with_source(mut self, source: &'r [u8]) -> Self {
        self.source = Some(source);
        self
    }

    /// Resolve a class identifier at `position` to its FQCN via the names table,
    /// if one is attached and has an entry. Returns `None` to fall back to the raw
    /// identifier (self/parent/static keywords are handled by the caller).
    fn resolve_name_at<T: mago_span::HasPosition>(&self, position: &T) -> Option<Vec<u8>> {
        self.names
            .and_then(|n| n.resolve(position))
            .map(|b| b.to_vec())
    }

    /// Permit `$this->prop` writes in this scope (constructor seeding only).
    pub fn allow_this_writes(&mut self) {
        self.allow_this_write = true;
    }

    fn tick(&mut self) -> Result<(), BailReason> {
        self.steps += 1;
        if self.steps > self.max_steps {
            return Err(BailReason::StepBudget);
        }
        Ok(())
    }
}

// ─── Control-flow signal ──────────────────────────────────────────────────────

/// What executing a statement/block produced.
enum Flow {
    /// Fell through to the next statement.
    Normal,
    /// Hit a `return <value>;`.
    Returned(Value),
    /// An assertion produced a terminal outcome (first failing assertion → Fail).
    Asserted(Outcome),
}

// ─── Public entry: run a method body over the Givens ──────────────────────────

/// Evaluate a test-method body (`block`) over the Given variable bindings,
/// folding to an [`Outcome`]. The first failing assertion yields `Fail`; running
/// to the end with no failing assertion yields `Pass`; any unmodelled
/// construct/value yields `Bailed`.
pub fn run_method_body(
    block: &Block,
    givens: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
) -> Outcome {
    run_method_body_inner(block, givens, resolver, None, None)
}

/// Like [`run_method_body`] but with the body file's resolved-names table attached
/// so `new ClassName` / `ClassName::m()` resolve to a FQCN (Inc-3).
pub fn run_method_body_with_names(
    block: &Block,
    givens: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
    names: &mago_names::ResolvedNames,
    source: &[u8],
) -> Outcome {
    run_method_body_inner(block, givens, resolver, Some(names), Some(source))
}

fn run_method_body_inner(
    block: &Block,
    givens: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
    names: Option<&mago_names::ResolvedNames>,
    source: Option<&[u8]>,
) -> Outcome {
    let mut scope = Scope::new(givens, resolver);
    if let Some(n) = names {
        scope = scope.with_names(n);
    }
    if let Some(s) = source {
        scope = scope.with_source(s);
    }
    match exec_statements(block.statements.iter(), &mut scope) {
        Ok(Flow::Asserted(outcome)) => outcome,
        // Body completed (or returned) with no failing assertion → Pass.
        Ok(Flow::Normal) | Ok(Flow::Returned(_)) => Outcome::Pass,
        Err(reason) => Outcome::Bailed(reason),
    }
}

/// Evaluate a (non-test) function/method body over its bound parameters and
/// return its `return`ed [`Value`]. This is the substitution primitive used by
/// the [`CallResolver`] (Task 5) to inline a user function: a body with no
/// `return` yields `Null` (PHP semantics). An assertion inside such a body is not
/// expected; if one appears, it is treated as a normal expression by the caller.
pub fn run_body_returning(
    block: &Block,
    bindings: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
) -> Result<Value, BailReason> {
    run_body_returning_inner(block, bindings, resolver, None, None)
}

/// Like [`run_body_returning`] but with the body file's resolved-names table
/// attached (Inc-3 class-name resolution inside an inlined body) plus the body
/// file's source (so a closure literal RETURNED from this body owns its bytes).
pub fn run_body_returning_with_names(
    block: &Block,
    bindings: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
    names: &mago_names::ResolvedNames,
    source: &[u8],
) -> Result<Value, BailReason> {
    run_body_returning_inner(block, bindings, resolver, Some(names), Some(source))
}

fn run_body_returning_inner(
    block: &Block,
    bindings: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
    names: Option<&mago_names::ResolvedNames>,
    source: Option<&[u8]>,
) -> Result<Value, BailReason> {
    let mut scope = Scope::new(bindings, resolver);
    if let Some(n) = names {
        scope = scope.with_names(n);
    }
    if let Some(s) = source {
        scope = scope.with_source(s);
    }
    match exec_statements(block.statements.iter(), &mut scope)? {
        Flow::Returned(v) => Ok(v),
        // Fell off the end with no `return` → PHP returns null.
        Flow::Normal => Ok(Value::Null),
        // An assertion inside an inlined helper is not modelled here.
        Flow::Asserted(_) => Err(BailReason::UnsupportedConstruct(
            "assertion inside an inlined function body".into(),
        )),
    }
}

/// Evaluate a single expression to a [`Value`] in the given scope. Used by the
/// substitution layer to compute a parameter's default-value expression.
pub fn eval_default(expr: &Expression, scope: &mut Scope) -> Result<Value, BailReason> {
    eval_expr(expr, scope)
}

/// PHP coerces an inlined body's `return` value to the declared SCALAR return type
/// (`: string`/`int`/`float`/`bool`) under weak typing — e.g. `function f(): string
/// { return true; }` returns `"1"`, not `true`. The reducer does not model this
/// coercion, so when a bare scalar return hint does not already match the returned
/// value's type, BAIL (fail-closed) rather than return the un-coerced value — that
/// was the symfony `LazyString::resolve(): string` false-FAIL. A non-scalar hint
/// (class, void, mixed, …) needs no coercion and is left alone; `?scalar` and
/// `scalar|…` unions are unwrapped so the wrapped scalar member still bails.
pub fn bail_if_scalar_return_coerces(
    hint: Option<&mago_syntax::ast::ast::function_like::r#return::FunctionLikeReturnTypeHint>,
    value: &Value,
) -> Result<(), BailReason> {
    let Some(rt) = hint else {
        return Ok(());
    };
    bail_if_scalar_hint_coerces(&rt.hint, value, "return")
}

/// Shared scalar-coercion guard for a declared type `hint` against a runtime
/// `value`, used by both the inlined-return path and the parameter-binding path
/// (`site` names which, for the bail message). PHP coerces a scalar value to a
/// declared scalar type at these boundaries (weak mode) or throws `TypeError`
/// (strict) — neither is modelled, so any mismatch BAILS (fail-closed, correct in
/// both modes). The check unwraps `?scalar` (nullable) and descends `scalar|…`
/// unions / `scalar&…` intersections so a wrapped scalar member is still enforced.
pub fn bail_if_scalar_hint_coerces(
    hint: &mago_syntax::ast::ast::type_hint::Hint,
    value: &Value,
    site: &str,
) -> Result<(), BailReason> {
    use mago_syntax::ast::ast::type_hint::Hint;

    match hint {
        // `?T`: a genuine null needs no coercion; otherwise apply the check to `T`.
        Hint::Nullable(n) => {
            if matches!(value, Value::Null) {
                Ok(())
            } else {
                bail_if_scalar_hint_coerces(n.hint, value, site)
            }
        }
        // A union / intersection coerces a SCALAR value unless it already matches a
        // member exactly. PHP only scalar-coerces a scalar value, so a non-scalar
        // value (object/array/closure) never undergoes scalar coercion here → Ok.
        // For a scalar value: if ANY member is a bare scalar and the value matches
        // NONE of the members exactly, PHP would coerce → BAIL. A value that already
        // equals a member — a bare scalar (`int`), OR a literal type
        // (`false`/`true`/`null`, e.g. `array_search(): int|string|false` returning
        // `false`) — needs no coercion. If no member is a bare scalar, no scalar
        // coercion is possible → leave alone.
        Hint::Union(_) | Hint::Intersection(_) => {
            if !is_scalar_value(value) {
                return Ok(());
            }
            let mut saw_scalar_member = false;
            let mut matched = false;
            for_each_hint_leaf(hint, &mut |leaf| {
                if bare_scalar_hint_matches(leaf, value) == Some(false) {
                    saw_scalar_member = true;
                }
                if value_matches_hint_member(leaf, value) {
                    matched = true;
                }
            });
            if saw_scalar_member && !matched {
                Err(BailReason::UnsupportedConstruct(format!(
                    "scalar {site}-type coercion ({} on a {} value) not modelled",
                    scalar_hint_name(hint),
                    value.type_name(),
                )))
            } else {
                Ok(())
            }
        }
        // A bare scalar hint: bail unless the value's type already matches exactly.
        _ => match bare_scalar_hint_matches(hint, value) {
            Some(true) | None => Ok(()),
            Some(false) => Err(BailReason::UnsupportedConstruct(format!(
                "scalar {site}-type coercion ({} on a {} value) not modelled",
                scalar_hint_name(hint),
                value.type_name(),
            ))),
        },
    }
}

/// For a BARE scalar hint, `Some(true)` if it matches `value`'s type exactly,
/// `Some(false)` if it is a bare scalar that does NOT match; `None` if `hint` is
/// not a bare scalar (no scalar coercion is implied by it).
fn bare_scalar_hint_matches(
    hint: &mago_syntax::ast::ast::type_hint::Hint,
    value: &Value,
) -> Option<bool> {
    use mago_syntax::ast::ast::type_hint::Hint;
    Some(match hint {
        Hint::String(_) => matches!(value, Value::Str(_)),
        Hint::Integer(_) => matches!(value, Value::Int(_)),
        Hint::Float(_) => matches!(value, Value::Float(_)),
        Hint::Bool(_) => matches!(value, Value::Bool(_)),
        _ => return None,
    })
}

/// Whether a SCALAR `value` ALREADY satisfies a single union/intersection member
/// `leaf` with no coercion. Covers bare scalars (`int`→`Int`) and the literal types
/// `true`/`false`/`null` (so `array_search(): int|string|false` returning `false`
/// is a clean member match, not a coercion). `mixed` accepts anything. Any other
/// member (class, array, callable, object, …) cannot be matched BY A SCALAR value,
/// so it never counts as a match here — the caller only reaches this with a scalar
/// value, and the bail decision still requires a non-matching bare-scalar member.
fn value_matches_hint_member(leaf: &mago_syntax::ast::ast::type_hint::Hint, value: &Value) -> bool {
    use mago_syntax::ast::ast::type_hint::Hint;
    match leaf {
        Hint::String(_) | Hint::Integer(_) | Hint::Float(_) | Hint::Bool(_) => {
            bare_scalar_hint_matches(leaf, value) == Some(true)
        }
        Hint::True(_) => matches!(value, Value::Bool(true)),
        Hint::False(_) => matches!(value, Value::Bool(false)),
        Hint::Null(_) => matches!(value, Value::Null),
        // `mixed` accepts any value, including a scalar → a clean match, no coercion.
        Hint::Mixed(_) => true,
        // Any other member (class / array / callable / object / iterable / …) cannot
        // be satisfied by a scalar value → not a match (fail-closed for the bail).
        _ => false,
    }
}

/// Whether `value` is a PHP scalar (the only values PHP scalar-coerces). Object,
/// closure and array values are never the subject of scalar coercion.
fn is_scalar_value(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_)
    )
}

/// Visit every leaf hint of a (possibly nested) union / intersection tree, calling
/// `f` on each non-composite leaf. Nullable wrappers are descended too.
fn for_each_hint_leaf<'a>(
    hint: &'a mago_syntax::ast::ast::type_hint::Hint,
    f: &mut impl FnMut(&'a mago_syntax::ast::ast::type_hint::Hint),
) {
    use mago_syntax::ast::ast::type_hint::Hint;
    match hint {
        Hint::Union(u) => {
            for_each_hint_leaf(u.left, f);
            for_each_hint_leaf(u.right, f);
        }
        Hint::Intersection(i) => {
            for_each_hint_leaf(i.left, f);
            for_each_hint_leaf(i.right, f);
        }
        Hint::Nullable(n) => for_each_hint_leaf(n.hint, f),
        Hint::Parenthesized(p) => for_each_hint_leaf(p.hint, f),
        leaf => f(leaf),
    }
}

/// Display name for a scalar coercion bail message. For a composite hint it names
/// the kind; for a bare scalar it names the scalar type.
fn scalar_hint_name(hint: &mago_syntax::ast::ast::type_hint::Hint) -> &'static str {
    use mago_syntax::ast::ast::type_hint::Hint;
    match hint {
        Hint::String(_) => "string",
        Hint::Integer(_) => "int",
        Hint::Float(_) => "float",
        Hint::Bool(_) => "bool",
        Hint::Nullable(_) => "nullable scalar",
        Hint::Union(_) => "scalar union",
        Hint::Intersection(_) => "scalar intersection",
        _ => "scalar",
    }
}

/// Inline a **constructor** body to seed a fresh `$this` record (Task B). The
/// `bindings` carry `this` (the partially-seeded object: promoted params + plain
/// literal defaults already filled) plus the constructor's parameters. Property
/// writes are PERMITTED here (seeding); the body runs and the mutated `$this`
/// record is returned. A `return` inside a constructor is ignored by PHP, so we
/// always return the `$this` record.
pub fn run_ctor_body(
    block: &Block,
    bindings: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
) -> Result<Value, BailReason> {
    run_ctor_body_inner(block, bindings, resolver, None, None)
}

/// Like [`run_ctor_body`] but with the body file's resolved-names table attached
/// (so a constructor/setUp body that does `new Other(...)` resolves the FQCN) plus
/// the body file's source (so a closure literal stored into `$this` here owns its
/// bytes — Inc-4 Task 1).
pub fn run_ctor_body_with_names(
    block: &Block,
    bindings: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
    names: &mago_names::ResolvedNames,
    source: &[u8],
) -> Result<Value, BailReason> {
    run_ctor_body_inner(block, bindings, resolver, Some(names), Some(source))
}

fn run_ctor_body_inner(
    block: &Block,
    bindings: HashMap<Vec<u8>, Value>,
    resolver: &dyn CallResolver,
    names: Option<&mago_names::ResolvedNames>,
    source: Option<&[u8]>,
) -> Result<Value, BailReason> {
    let mut scope = Scope::new(bindings, resolver);
    if let Some(n) = names {
        scope = scope.with_names(n);
    }
    if let Some(s) = source {
        scope = scope.with_source(s);
    }
    scope.allow_this_writes();
    match exec_statements(block.statements.iter(), &mut scope)? {
        // Whatever the body did (returned early or fell through), the constructed
        // value is the (possibly mutated) `$this` record.
        Flow::Normal | Flow::Returned(_) => scope
            .vars
            .remove(b"this".as_slice())
            .ok_or_else(|| BailReason::Other("constructor lost its \\$this".into())),
        Flow::Asserted(_) => Err(BailReason::UnsupportedConstruct(
            "assertion inside a constructor body".into(),
        )),
    }
}

/// Build a `Value::Object` from a class name and seed props directly (no body) —
/// used when a class has promoted params + an empty constructor body, or no
/// constructor at all.
pub fn make_object(class: Vec<u8>, props: Vec<(Vec<u8>, Value)>) -> Value {
    Value::Object { class, props }
}

// ─── Statement execution ──────────────────────────────────────────────────────

fn exec_statements<'a, I>(stmts: I, scope: &mut Scope) -> Result<Flow, BailReason>
where
    I: IntoIterator<Item = &'a Statement<'a>>,
{
    for stmt in stmts {
        match exec_stmt(stmt, scope)? {
            Flow::Normal => {}
            other => return Ok(other),
        }
    }
    Ok(Flow::Normal)
}

fn exec_stmt(stmt: &Statement, scope: &mut Scope) -> Result<Flow, BailReason> {
    scope.tick()?;
    match stmt {
        Statement::Expression(es) => {
            // An expression statement may BE an assertion call. A PASSING
            // assertion falls through to the next statement; only a FAILING
            // assertion is terminal (first failure wins).
            if let Some(outcome) = try_assertion(es.expression, scope)? {
                return Ok(match outcome {
                    Outcome::Pass => Flow::Normal,
                    fail_or_bail => Flow::Asserted(fail_or_bail),
                });
            }
            eval_expr(es.expression, scope)?;
            Ok(Flow::Normal)
        }
        Statement::Return(ret) => {
            let v = match ret.value {
                Some(e) => eval_expr(e, scope)?,
                None => Value::Null,
            };
            Ok(Flow::Returned(v))
        }
        Statement::Block(b) => exec_statements(b.statements.iter(), scope),
        Statement::If(if_stmt) => exec_if(if_stmt, scope),
        Statement::While(w) => exec_while(w, scope),
        Statement::For(f) => exec_for(f, scope),
        Statement::Foreach(f) => exec_foreach(f, scope),
        other => Err(BailReason::UnsupportedConstruct(format!(
            "statement: {}",
            stmt_kind(other)
        ))),
    }
}

fn stmt_kind(s: &Statement) -> &'static str {
    match s {
        Statement::Switch(_) => "switch",
        Statement::Echo(_) => "echo",
        Statement::Try(_) => "try",
        _ => "other",
    }
}

fn exec_if(
    if_stmt: &mago_syntax::ast::ast::control_flow::r#if::If,
    scope: &mut Scope,
) -> Result<Flow, BailReason> {
    use mago_syntax::ast::ast::control_flow::r#if::IfBody;
    let cond = eval_expr(if_stmt.condition, scope)?;
    match &if_stmt.body {
        IfBody::Statement(body) => {
            if cond.to_bool() {
                return exec_stmt(body.statement, scope);
            }
            for clause in body.else_if_clauses.iter() {
                let c = eval_expr(clause.condition, scope)?;
                if c.to_bool() {
                    return exec_stmt(clause.statement, scope);
                }
            }
            if let Some(else_clause) = &body.else_clause {
                return exec_stmt(else_clause.statement, scope);
            }
            Ok(Flow::Normal)
        }
        IfBody::ColonDelimited(_) => Err(BailReason::UnsupportedConstruct(
            "alternative (colon) if syntax".into(),
        )),
    }
}

fn exec_while(
    w: &mago_syntax::ast::ast::r#loop::r#while::While,
    scope: &mut Scope,
) -> Result<Flow, BailReason> {
    use mago_syntax::ast::ast::r#loop::r#while::WhileBody;
    loop {
        scope.tick()?;
        let cond = eval_expr(w.condition, scope)?;
        if !cond.to_bool() {
            break;
        }
        let flow = match &w.body {
            WhileBody::Statement(s) => exec_stmt(s, scope)?,
            WhileBody::ColonDelimited(_) => {
                return Err(BailReason::UnsupportedConstruct(
                    "alternative (colon) while syntax".into(),
                ))
            }
        };
        match flow {
            Flow::Normal => {}
            // `break`/`continue` are not modelled; a return/assert propagates.
            other => return Ok(other),
        }
    }
    Ok(Flow::Normal)
}

/// C-style `for (init; cond; step) body`. Same step-budget guard as `while` →
/// a runaway loop bails (never hangs). PHP allows comma-separated init/step
/// expressions and a comma-separated condition list whose LAST element is the
/// truthiness test (gold-tested vs `php -r`); an EMPTY condition is always-true.
/// `break`/`continue` inside the body are not modelled — a `return`/assertion
/// propagates, but a `break`-relying loop would mis-run, so the loop's body is
/// executed via `exec_stmt` whose unmodelled `break` bails (fail-closed).
fn exec_for(
    f: &mago_syntax::ast::ast::r#loop::r#for::For,
    scope: &mut Scope,
) -> Result<Flow, BailReason> {
    use mago_syntax::ast::ast::r#loop::r#for::ForBody;

    // init: evaluate every initialization expression once, in order.
    for init in f.initializations.iter() {
        eval_expr(init, scope)?;
    }

    loop {
        scope.tick()?;
        // condition: PHP evaluates ALL condition expressions; the loop continues
        // iff the LAST one is truthy. An empty condition list is always-true.
        let mut keep_going = true;
        let mut last: Option<Value> = None;
        for cond in f.conditions.iter() {
            last = Some(eval_expr(cond, scope)?);
        }
        if let Some(v) = last {
            keep_going = v.to_bool();
        }
        if !keep_going {
            break;
        }

        let flow = match &f.body {
            ForBody::Statement(s) => exec_stmt(s, scope)?,
            ForBody::ColonDelimited(body) => {
                let mut acc = Flow::Normal;
                for s in body.statements.iter() {
                    match exec_stmt(s, scope)? {
                        Flow::Normal => {}
                        other => {
                            acc = other;
                            break;
                        }
                    }
                }
                acc
            }
        };
        match flow {
            Flow::Normal => {}
            // `break`/`continue` are not modelled; a return/assert propagates.
            other => return Ok(other),
        }

        // step: evaluate every increment expression once, in order.
        for step in f.increments.iter() {
            eval_expr(step, scope)?;
        }
    }
    Ok(Flow::Normal)
}

fn exec_foreach(
    f: &mago_syntax::ast::ast::r#loop::foreach::Foreach,
    scope: &mut Scope,
) -> Result<Flow, BailReason> {
    use mago_syntax::ast::ast::r#loop::foreach::{ForeachBody, ForeachTarget};

    let subject = eval_expr(f.expression, scope)?;
    let Value::Arr(items) = subject else {
        return Err(BailReason::TypeError(format!(
            "foreach over non-array ({})",
            subject.type_name()
        )));
    };

    for (key, val) in items {
        scope.tick()?;
        match &f.target {
            ForeachTarget::Value(t) => {
                bind_lvalue(t.value, val, scope)?;
            }
            ForeachTarget::KeyValue(t) => {
                let key_val = match key {
                    ArrayKey::Int(i) => Value::Int(i),
                    ArrayKey::Str(s) => Value::Str(s),
                };
                bind_lvalue(t.key, key_val, scope)?;
                bind_lvalue(t.value, val, scope)?;
            }
        }
        let flow = match &f.body {
            ForeachBody::Statement(s) => exec_stmt(s, scope)?,
            ForeachBody::ColonDelimited(_) => {
                return Err(BailReason::UnsupportedConstruct(
                    "alternative (colon) foreach syntax".into(),
                ))
            }
        };
        match flow {
            Flow::Normal => {}
            other => return Ok(other),
        }
    }
    Ok(Flow::Normal)
}

/// Bind a value to a simple `$var` lvalue (foreach targets / assignment targets).
fn bind_lvalue(target: &Expression, val: Value, scope: &mut Scope) -> Result<(), BailReason> {
    match target {
        Expression::Variable(Variable::Direct(v)) => {
            scope.vars.insert(var_name(v.name), val);
            Ok(())
        }
        _ => Err(BailReason::UnsupportedConstruct(
            "non-simple lvalue (only $var assignment is modelled)".into(),
        )),
    }
}

/// Strip a leading `$` from a variable token to get the bare name.
fn var_name(name: &[u8]) -> Vec<u8> {
    name.strip_prefix(b"$").unwrap_or(name).to_vec()
}

// ─── Assertion intrinsics ─────────────────────────────────────────────────────

/// If `expr` is a recognized assertion call (`$this->assertSame(...)` or a bare
/// `assertSame(...)`), evaluate it and return its [`Outcome`]. Returns `Ok(None)`
/// when `expr` is not an assertion (the caller then evaluates it as a normal
/// expression).
fn try_assertion(expr: &Expression, scope: &mut Scope) -> Result<Option<Outcome>, BailReason> {
    let Expression::Call(call) = expr else {
        return Ok(None);
    };
    let (name, args) = match call {
        // `$this->assertSame(...)` — the common PHPUnit form.
        Call::Method(m) => {
            let mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector::Identifier(id) =
                &m.method
            else {
                return Ok(None);
            };
            (id.value, &m.argument_list)
        }
        // Bare `assertSame(...)` (e.g. via function import).
        Call::Function(fc) => match identifier_name(fc.function) {
            Some(n) => (n, &fc.argument_list),
            None => return Ok(None),
        },
        // `self::assertSame(...)` / `static::` / `parent::` — the static PHPUnit
        // form (doctrine/collections uses this). Only a self/static/parent receiver
        // is intercepted: inside a test method these unambiguously target the
        // test case, so an assertion-named static call is a real assertion. A
        // static call through a concrete `Foo::` class name is NOT intercepted
        // (it could be a user static method) and falls through to dispatch.
        Call::StaticMethod(sm) => {
            let mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector::Identifier(id) =
                &sm.method
            else {
                return Ok(None);
            };
            if !is_self_parent_static(sm.class) {
                return Ok(None);
            }
            (id.value, &sm.argument_list)
        }
        _ => return Ok(None),
    };

    if !is_assertion_name(name) {
        return Ok(None);
    }

    let arg_values = eval_arguments(args, scope)?;
    Ok(Some(run_assertion(name, &arg_values)?))
}

/// True when a static-call class expression is `self`, `static`, or `parent` —
/// the only receivers for which an assertion-named static call is unambiguously a
/// PHPUnit assertion (inside a test method). A concrete class name is not matched.
///
/// mago parses these keywords as their own `Expression` variants (not an
/// `Identifier`), but in some positions a bare `self` may also arrive as a local
/// identifier — handle both shapes.
fn is_self_parent_static(class: &Expression) -> bool {
    if matches!(
        class,
        Expression::Self_(_) | Expression::Static(_) | Expression::Parent(_)
    ) {
        return true;
    }
    match identifier_name(class) {
        Some(n) => {
            n.eq_ignore_ascii_case(b"self")
                || n.eq_ignore_ascii_case(b"static")
                || n.eq_ignore_ascii_case(b"parent")
        }
        None => false,
    }
}

fn is_assertion_name(name: &[u8]) -> bool {
    matches!(
        name,
        b"assertSame"
            | b"assertEquals"
            | b"assertNotSame"
            | b"assertNotEquals"
            | b"assertTrue"
            | b"assertFalse"
            | b"assertNull"
            | b"assertNotNull"
            | b"assertCount"
            | b"assertNotCount"
            | b"assertIsArray"
    )
}

/// Evaluate a recognized assertion over its concrete argument values.
fn run_assertion(name: &[u8], args: &[Value]) -> Result<Outcome, BailReason> {
    use super::value::{assert_equals, assert_same};

    // A 3rd argument to assertEquals/assertSame is a message (fine) — but a delta
    // overload (`assertEqualsWithDelta`, or assertEquals with a float delta) is a
    // float-epsilon assertion we must NOT model.
    let pass = |ok: bool, desc: &str| {
        if ok {
            Outcome::Pass
        } else {
            Outcome::Fail(desc.to_string())
        }
    };

    Ok(match name {
        b"assertSame" => {
            let (e, a) = two_args(args)?;
            // assertSame on objects is REFERENCE identity (e.g. a static singleton):
            // the reducer has no heap/identity model → BAIL, never guess (frontier §1).
            bail_if_object_operand(e, a)?;
            pass(assert_same(e, a), "assertSame")
        }
        b"assertNotSame" => {
            let (e, a) = two_args(args)?;
            bail_if_object_operand(e, a)?;
            pass(!assert_same(e, a), "assertNotSame")
        }
        b"assertEquals" => {
            let (e, a) = two_args(args)?;
            // assertEquals on a closure is reference identity (no heap model) → bail
            // rather than let php_loose_eq short-circuit to a false-green result.
            bail_if_closure_operand(e, a)?;
            pass(assert_equals(e, a), "assertEquals")
        }
        b"assertNotEquals" => {
            let (e, a) = two_args(args)?;
            bail_if_closure_operand(e, a)?;
            pass(!assert_equals(e, a), "assertNotEquals")
        }
        b"assertTrue" => {
            let a = one_arg(args)?;
            // assertTrue requires the value to be strictly `true` (bool), not truthy.
            pass(matches!(a, Value::Bool(true)), "assertTrue")
        }
        b"assertFalse" => {
            let a = one_arg(args)?;
            pass(matches!(a, Value::Bool(false)), "assertFalse")
        }
        b"assertNull" => {
            let a = one_arg(args)?;
            pass(matches!(a, Value::Null), "assertNull")
        }
        b"assertNotNull" => {
            let a = one_arg(args)?;
            pass(!matches!(a, Value::Null), "assertNotNull")
        }
        b"assertCount" => {
            let (expected, haystack) = two_args(args)?;
            pass(count_matches(expected, haystack)?, "assertCount")
        }
        b"assertNotCount" => {
            let (expected, haystack) = two_args(args)?;
            pass(!count_matches(expected, haystack)?, "assertNotCount")
        }
        b"assertIsArray" => {
            let a = one_arg(args)?;
            pass(matches!(a, Value::Arr(_)), "assertIsArray")
        }
        _ => {
            return Err(BailReason::UnknownCall(
                String::from_utf8_lossy(name).into_owned(),
            ))
        }
    })
}

/// `assertSame`/`assertNotSame`/`===` over objects is REFERENCE identity — the
/// reducer models structure, not heap identity, so it MUST abstain when either
/// operand is an object (frontier §1). `assertEquals`/`==` stays modelable.
fn bail_if_object_operand(a: &Value, b: &Value) -> Result<(), BailReason> {
    if matches!(a, Value::Object { .. } | Value::Closure(_))
        || matches!(b, Value::Object { .. } | Value::Closure(_))
    {
        return Err(BailReason::UnsupportedConstruct(
            "=== / assertSame on an object (reference identity, no heap model)".into(),
        ));
    }
    Ok(())
}

/// `==`/`!=`/`assertEquals`/`assertNotEquals` on a `Value::Closure` is reference
/// identity in PHP (two closures are `==` iff they are the SAME instance). The
/// reducer models no heap identity, so it MUST abstain when either operand is a
/// closure — otherwise `php_loose_eq` short-circuits to `false` and produces a
/// false-green Pass (Inc-4 Task 4). Objects stay modelable (gate on Closure only).
fn bail_if_closure_operand(a: &Value, b: &Value) -> Result<(), BailReason> {
    if matches!(a, Value::Closure(_)) || matches!(b, Value::Closure(_)) {
        return Err(BailReason::UnsupportedConstruct(
            "== / != / assertEquals on a closure (reference identity, no heap model)".into(),
        ));
    }
    Ok(())
}

/// Two-argument assertions: exactly `(expected, actual)`. A 3rd *string* arg
/// (message) is allowed; any 3rd non-string arg (a delta) → bail (FloatDelta).
fn two_args(args: &[Value]) -> Result<(&Value, &Value), BailReason> {
    match args {
        [e, a] => Ok((e, a)),
        [e, a, Value::Str(_)] => Ok((e, a)), // trailing message string is fine
        [_, _, _, ..] => Err(BailReason::FloatDelta),
        _ => Err(BailReason::TypeError(
            "assertion expects 2 arguments".into(),
        )),
    }
}

/// `assertCount($expected, $haystack)`: true iff `count($haystack) === $expected`.
/// Only an array haystack is modelled — a `Countable` object's count depends on a
/// user `count()` method (or an iterator), so an object/non-array haystack BAILS
/// (fail-closed). The expected value must be an int (a non-int is a PHP TypeError).
fn count_matches(expected: &Value, haystack: &Value) -> Result<bool, BailReason> {
    let Value::Arr(items) = haystack else {
        return Err(BailReason::UnsupportedConstruct(
            "assertCount over a non-array (Countable/iterator) haystack".into(),
        ));
    };
    let Value::Int(n) = expected else {
        return Err(BailReason::TypeError(
            "assertCount expected count is not an int".into(),
        ));
    };
    Ok(items.len() as i64 == *n)
}

fn one_arg(args: &[Value]) -> Result<&Value, BailReason> {
    match args {
        [a] => Ok(a),
        [a, Value::Str(_)] => Ok(a), // trailing message string
        _ => Err(BailReason::TypeError("assertion expects 1 argument".into())),
    }
}

// ─── Expression evaluation ────────────────────────────────────────────────────

fn eval_expr(expr: &Expression, scope: &mut Scope) -> Result<Value, BailReason> {
    scope.tick()?;
    match expr {
        Expression::Literal(lit) => eval_literal(lit),
        Expression::Parenthesized(p) => eval_expr(p.expression, scope),
        Expression::Variable(Variable::Direct(v)) => {
            let key = var_name(v.name);
            scope.vars.get(&key).cloned().ok_or_else(|| {
                BailReason::UnboundVariable(String::from_utf8_lossy(&key).into_owned())
            })
        }
        Expression::Variable(_) => Err(BailReason::UnsupportedConstruct(
            "indirect/nested variable".into(),
        )),
        Expression::UnaryPrefix(u) => eval_unary(&u.operator, u.operand, scope),
        Expression::UnaryPostfix(u) => eval_postfix(u.operand, &u.operator, scope),
        Expression::Binary(b) => eval_binary(b.lhs, &b.operator, b.rhs, scope),
        Expression::Assignment(a) => eval_assignment(a, scope),
        Expression::Conditional(c) => eval_conditional(c, scope),
        Expression::Array(arr) => eval_array(arr, scope),
        Expression::LegacyArray(arr) => eval_legacy_array(arr, scope),
        Expression::Call(call) => eval_call(call, scope),
        // `new C(args)` (Task B) — resolve C's FQCN and inline its constructor.
        Expression::Instantiation(inst) => eval_instantiation(inst, scope),
        // Property/const access. Only `$obj->prop` (read) is modelled (Task D);
        // static-property / class-constant / null-safe access bail.
        Expression::Access(access) => eval_access(access, scope),
        // `$arr[$key]` read over any expression evaluating to an array (Task D).
        // A BARE read of a missing key bails (PHP would warn) — only `?? default`
        // (handled in `eval_binary`'s NullCoalesce) tolerates a missing key.
        Expression::ArrayAccess(aa) => eval_array_access(aa, scope),
        // A closure / arrow function literal → a `Value::Closure` (Task A).
        Expression::Closure(c) => make_closure(c, scope),
        Expression::ArrowFunction(a) => make_arrow(a, scope),
        other => Err(BailReason::UnsupportedConstruct(format!(
            "expression: {}",
            expr_kind(other)
        ))),
    }
}

/// `new C(args)` (Task B): resolve the FQCN (Identifier only — `new $var`/
/// anonymous classes bail) and ask the resolver to construct the record.
fn eval_instantiation(
    inst: &mago_syntax::ast::ast::instantiation::Instantiation,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    let class = resolve_class_name_in_scope(inst.class, scope)?;
    let args = match &inst.argument_list {
        Some(list) => eval_arguments(list, scope)?,
        None => Vec::new(),
    };
    match scope.resolver.construct(&class, &args)? {
        Some(v) => Ok(v),
        None => Err(BailReason::UnknownCall(format!(
            "new {}",
            String::from_utf8_lossy(&class)
        ))),
    }
}

/// `$obj->prop` read. The receiver must evaluate to a [`Value::Object`]; the
/// property name must be a static identifier. A missing property bails (PHP warns
/// then returns null; under `??` the caller swallows this), a non-object receiver
/// type-errors, a dynamic selector bails.
fn eval_property_read(
    pa: &mago_syntax::ast::ast::access::PropertyAccess,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    use mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector;
    let ClassLikeMemberSelector::Identifier(prop_id) = &pa.property else {
        return Err(BailReason::UnsupportedConstruct(
            "dynamic property selector".into(),
        ));
    };
    let receiver = eval_expr(pa.object, scope)?;
    let Value::Object { props, .. } = &receiver else {
        return Err(BailReason::TypeError(format!(
            "property read on non-object ({})",
            receiver.type_name()
        )));
    };
    match props.iter().find(|(k, _)| k.as_slice() == prop_id.value) {
        Some((_, v)) => Ok(v.clone()),
        // PHP would warn + return null for an undefined property; we bail
        // (an unseeded prop usually means a default/hook we did not model).
        None => Err(BailReason::UnsupportedConstruct(format!(
            "read of unset property ${}",
            String::from_utf8_lossy(prop_id.value)
        ))),
    }
}

/// `$obj->prop` read (Task D). The receiver must evaluate to a [`Value::Object`];
/// the property name must be a static identifier. A missing property, a non-object
/// receiver, static-property / class-constant / null-safe access all BAIL.
fn eval_access(
    access: &mago_syntax::ast::ast::access::Access,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    use mago_syntax::ast::ast::access::Access;
    match access {
        Access::Property(pa) => eval_property_read(pa, scope),
        Access::NullSafeProperty(_) => Err(BailReason::UnsupportedConstruct(
            "null-safe property access (?->)".into(),
        )),
        Access::StaticProperty(_) => Err(BailReason::UnsupportedConstruct(
            "static property access".into(),
        )),
        Access::ClassConstant(_) => Err(BailReason::UnsupportedConstruct(
            "class constant access".into(),
        )),
    }
}

/// `$arr[$key]` read (Task D) over ANY expression evaluating to an array. A bare
/// read (not under `??`) of a missing key BAILS (PHP would emit a warning + null;
/// a strict suite could escalate that — fail-closed). Subscripting a non-array
/// (string-offset, ArrayAccess object) bails. Returns `Ok(None)` from the lookup
/// when `coalesce` is set and the key is missing OR its value is null (isset
/// semantics) — the `??` caller then uses the default.
fn eval_array_access(
    aa: &mago_syntax::ast::ast::array::ArrayAccess,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    match array_access_lookup(aa, scope, false)? {
        Some(v) => Ok(v),
        None => Err(BailReason::UnsupportedConstruct(
            "read of a missing array key (PHP warning)".into(),
        )),
    }
}

/// Shared subscript lookup. `coalesce=false`: a present key returns `Some(value)`
/// (even a null value), a missing key returns `None` (caller bails on a bare read).
/// `coalesce=true` (under `??`): a missing key OR a present-but-null value both
/// return `None` (isset semantics — `$a[$k] ?? $d` uses `$d` when unset OR null).
fn array_access_lookup(
    aa: &mago_syntax::ast::ast::array::ArrayAccess,
    scope: &mut Scope,
    coalesce: bool,
) -> Result<Option<Value>, BailReason> {
    let receiver = eval_expr(aa.array, scope)?;
    let Value::Arr(items) = &receiver else {
        return Err(BailReason::TypeError(format!(
            "array subscript on a non-array ({})",
            receiver.type_name()
        )));
    };
    let index = eval_expr(aa.index, scope)?;
    let key = index
        .to_array_key()
        .ok_or_else(|| BailReason::TypeError("array/object used as a subscript key".into()))?;
    match items.iter().find(|(k, _)| *k == key) {
        Some((_, v)) => {
            if coalesce && matches!(v, Value::Null) {
                Ok(None)
            } else {
                Ok(Some(v.clone()))
            }
        }
        None => Ok(None),
    }
}

/// Evaluate the lhs of a `??` with isset semantics: returns `None` when the lhs
/// is unset/missing (a missing array key, an unset object property) OR evaluates
/// to `null`; otherwise `Some(value)`. A subscript/property lhs is looked up in
/// "coalesce mode" so a missing key/prop does NOT bail (the `??` default applies).
fn eval_coalesce_lhs(lhs: &Expression, scope: &mut Scope) -> Result<Option<Value>, BailReason> {
    use mago_syntax::ast::ast::access::Access;
    match lhs {
        // `$arr[$k] ?? d`: a missing key or a null value → use the default.
        Expression::ArrayAccess(aa) => array_access_lookup(aa, scope, true),
        // `$obj->prop ?? d`: an unset (unmodelled) property → use the default
        // rather than bailing; a null-valued property → use the default too.
        Expression::Access(Access::Property(pa)) => {
            match eval_property_read(pa, scope) {
                Ok(v) => Ok(if matches!(v, Value::Null) {
                    None
                } else {
                    Some(v)
                }),
                // An unset property bails in a bare read; under `??` it is "unset"
                // → use the default. Only the unset-property bail is swallowed;
                // any other bail (non-object receiver, dynamic selector) propagates.
                Err(BailReason::UnsupportedConstruct(msg))
                    if msg.starts_with("read of unset") || msg.starts_with("read of a missing") =>
                {
                    Ok(None)
                }
                Err(other) => Err(other),
            }
        }
        // Any other lhs: normal eval, treat null as "use default".
        _ => {
            let v = eval_expr(lhs, scope)?;
            Ok(if matches!(v, Value::Null) {
                None
            } else {
                Some(v)
            })
        }
    }
}

fn expr_kind(e: &Expression) -> &'static str {
    match e {
        Expression::ArrayAccess(_) => "array_access",
        Expression::Access(_) => "property/const_access",
        Expression::Construct(_) => "language_construct",
        Expression::Match(_) => "match",
        _ => "other",
    }
}

fn eval_literal(lit: &Literal) -> Result<Value, BailReason> {
    match lit {
        Literal::Integer(i) => match i.value {
            Some(v) => Ok(Value::Int(v as i64)),
            None => Err(BailReason::UnsupportedConstruct(
                "integer literal overflow (>i64)".into(),
            )),
        },
        Literal::Float(f) => Ok(Value::Float(*f.value)),
        Literal::String(s) => match &s.value {
            Some(v) => Ok(Value::Str(v.to_vec())),
            None => Err(BailReason::UnsupportedConstruct(
                "string literal with unresolved escapes".into(),
            )),
        },
        Literal::True(_) => Ok(Value::Bool(true)),
        Literal::False(_) => Ok(Value::Bool(false)),
        Literal::Null(_) => Ok(Value::Null),
    }
}

fn eval_unary(
    op: &UnaryPrefixOperator,
    operand: &Expression,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    match op {
        UnaryPrefixOperator::Not(_) => Ok(Value::Bool(!eval_expr(operand, scope)?.to_bool())),
        UnaryPrefixOperator::Negation(_) => php_negate(eval_expr(operand, scope)?),
        UnaryPrefixOperator::Plus(_) => php_unary_plus(eval_expr(operand, scope)?),
        // `++$x` / `--$x`: mutate then return the NEW value. Only a simple `$var`
        // numeric lvalue is modelled (the loop-counter case); string/null
        // increment has PHP-specific quirks (perl-style string ++, `null++`→1 but
        // `null--`→null) → bail there, fail-closed.
        UnaryPrefixOperator::PreIncrement(_) => incdec_lvalue(operand, true, true, scope),
        UnaryPrefixOperator::PreDecrement(_) => incdec_lvalue(operand, false, true, scope),
        UnaryPrefixOperator::IntCast(..) | UnaryPrefixOperator::IntegerCast(..) => {
            Ok(Value::Int(eval_expr(operand, scope)?.to_int()))
        }
        UnaryPrefixOperator::FloatCast(..) | UnaryPrefixOperator::DoubleCast(..) => {
            Ok(Value::Float(eval_expr(operand, scope)?.to_float()))
        }
        UnaryPrefixOperator::BoolCast(..) | UnaryPrefixOperator::BooleanCast(..) => {
            Ok(Value::Bool(eval_expr(operand, scope)?.to_bool()))
        }
        UnaryPrefixOperator::StringCast(..) => {
            let v = eval_expr(operand, scope)?;
            v.to_php_string()
                .map(Value::Str)
                .ok_or_else(|| BailReason::TypeError("string cast of array".into()))
        }
        other => Err(BailReason::UnsupportedConstruct(format!(
            "unary prefix {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

/// PHP unary `-`: int → wrapped negate but `-PHP_INT_MIN` overflows to float.
fn php_negate(v: Value) -> Result<Value, BailReason> {
    match v {
        Value::Int(n) => Ok(match n.checked_neg() {
            Some(r) => Value::Int(r),
            None => Value::Float(-(n as f64)), // -PHP_INT_MIN → float
        }),
        Value::Float(f) => Ok(Value::Float(-f)),
        Value::Str(_) | Value::Bool(_) | Value::Null => php_negate(coerce_number(&v)),
        Value::Arr(_) => Err(BailReason::TypeError("negate array".into())),
        Value::Object { .. } | Value::Closure(_) => {
            Err(BailReason::TypeError("negate object".into()))
        }
    }
}

fn php_unary_plus(v: Value) -> Result<Value, BailReason> {
    match v {
        Value::Int(_) | Value::Float(_) => Ok(v),
        Value::Str(_) | Value::Bool(_) | Value::Null => Ok(coerce_number(&v)),
        Value::Arr(_) => Err(BailReason::TypeError("unary + on array".into())),
        Value::Object { .. } | Value::Closure(_) => {
            Err(BailReason::TypeError("unary + on object".into()))
        }
    }
}

/// `$x++` / `$x--`: mutate then return the OLD value (postfix).
fn eval_postfix(
    operand: &Expression,
    op: &mago_syntax::ast::ast::unary::UnaryPostfixOperator,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    use mago_syntax::ast::ast::unary::UnaryPostfixOperator as Op;
    match op {
        Op::PostIncrement(_) => incdec_lvalue(operand, true, false, scope),
        Op::PostDecrement(_) => incdec_lvalue(operand, false, false, scope),
    }
}

/// Shared `++`/`--` on a simple `$var` lvalue. `inc` selects increment vs
/// decrement; `prefix` selects whether the NEW (prefix) or OLD (postfix) value is
/// returned. Only an Int/Float counter is modelled — a string/null/bool/array
/// target bails (PHP's perl-style string `++`, `null--`→null, etc. are quirks we
/// refuse to guess). The target must be a bound `$var`; an unbound one bails.
fn incdec_lvalue(
    operand: &Expression,
    inc: bool,
    prefix: bool,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    let Expression::Variable(Variable::Direct(v)) = operand else {
        return Err(BailReason::UnsupportedConstruct(
            "++/-- on a non-simple lvalue".into(),
        ));
    };
    let key = var_name(v.name);
    let cur =
        scope.vars.get(&key).cloned().ok_or_else(|| {
            BailReason::UnboundVariable(String::from_utf8_lossy(&key).into_owned())
        })?;
    let new = match &cur {
        Value::Int(n) => {
            let stepped = if inc {
                n.checked_add(1)
            } else {
                n.checked_sub(1)
            };
            match stepped {
                Some(r) => Value::Int(r),
                // PHP_INT_MAX++ → float (overflow→float), like arithmetic.
                None => Value::Float(if inc {
                    *n as f64 + 1.0
                } else {
                    *n as f64 - 1.0
                }),
            }
        }
        Value::Float(f) => Value::Float(if inc { f + 1.0 } else { f - 1.0 }),
        // String/null/bool/array ++/-- have PHP-specific semantics we won't guess.
        other => {
            return Err(BailReason::UnsupportedConstruct(format!(
                "++/-- on a {} (only numeric counters modelled)",
                other.type_name()
            )))
        }
    };
    scope.vars.insert(key, new.clone());
    Ok(if prefix { new } else { cur })
}

fn eval_binary(
    lhs: &Expression,
    op: &BinaryOperator,
    rhs: &Expression,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    use std::cmp::Ordering;
    // Short-circuiting logical ops evaluate the rhs lazily.
    match op {
        BinaryOperator::And(_) => {
            let l = eval_expr(lhs, scope)?;
            return Ok(Value::Bool(l.to_bool() && eval_expr(rhs, scope)?.to_bool()));
        }
        BinaryOperator::Or(_) => {
            let l = eval_expr(lhs, scope)?;
            return Ok(Value::Bool(l.to_bool() || eval_expr(rhs, scope)?.to_bool()));
        }
        BinaryOperator::NullCoalesce(_) => {
            // `??` is isset-based: a missing array key / unset property on the lhs
            // yields the default WITHOUT a warning (unlike a bare read). So a
            // subscript/property lhs is evaluated in coalesce mode (missing → None);
            // any other lhs uses normal eval and we test for Null.
            let l = eval_coalesce_lhs(lhs, scope)?;
            return Ok(match l {
                Some(v) => v,
                None => eval_expr(rhs, scope)?,
            });
        }
        _ => {}
    }

    let l = eval_expr(lhs, scope)?;
    let r = eval_expr(rhs, scope)?;
    match op {
        BinaryOperator::Addition(_) => php_add(&l, &r),
        BinaryOperator::Subtraction(_) => php_arith(&l, &r, i64::checked_sub, |a, b| a - b),
        BinaryOperator::Multiplication(_) => php_arith(&l, &r, i64::checked_mul, |a, b| a * b),
        BinaryOperator::Division(_) => php_div(&l, &r),
        BinaryOperator::Modulo(_) => php_mod(&l, &r),
        BinaryOperator::Exponentiation(_) => php_pow(&l, &r),
        BinaryOperator::StringConcat(_) => php_concat(&l, &r),
        BinaryOperator::Equal(_) => {
            // `==` on a closure is reference identity (no heap model) → bail.
            bail_if_closure_operand(&l, &r)?;
            Ok(Value::Bool(l.php_loose_eq(&r)))
        }
        BinaryOperator::NotEqual(_) | BinaryOperator::AngledNotEqual(_) => {
            bail_if_closure_operand(&l, &r)?;
            Ok(Value::Bool(!l.php_loose_eq(&r)))
        }
        // `===`/`!==` over objects is reference identity (frontier §1) → bail.
        BinaryOperator::Identical(_) => {
            bail_if_object_operand(&l, &r)?;
            Ok(Value::Bool(l.php_strict_eq(&r)))
        }
        BinaryOperator::NotIdentical(_) => {
            bail_if_object_operand(&l, &r)?;
            Ok(Value::Bool(!l.php_strict_eq(&r)))
        }
        // Ordering on objects is uncomparable in PHP (and our model would guess) →
        // bail when either operand is an object.
        BinaryOperator::LessThan(_) => {
            bail_if_object_operand(&l, &r)?;
            Ok(Value::Bool(l.php_compare(&r) == Ordering::Less))
        }
        BinaryOperator::GreaterThan(_) => {
            bail_if_object_operand(&l, &r)?;
            Ok(Value::Bool(l.php_compare(&r) == Ordering::Greater))
        }
        BinaryOperator::LessThanOrEqual(_) => {
            bail_if_object_operand(&l, &r)?;
            Ok(Value::Bool(l.php_compare(&r) != Ordering::Greater))
        }
        BinaryOperator::GreaterThanOrEqual(_) => {
            bail_if_object_operand(&l, &r)?;
            Ok(Value::Bool(l.php_compare(&r) != Ordering::Less))
        }
        BinaryOperator::Spaceship(_) => {
            bail_if_object_operand(&l, &r)?;
            Ok(Value::Int(match l.php_compare(&r) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }))
        }
        other => Err(BailReason::UnsupportedConstruct(format!(
            "binary operator {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

// ─── PHP arithmetic with overflow→float (gold-tested) ─────────────────────────

/// Coerce a non-numeric scalar to its PHP numeric value (int or float).
fn coerce_number(v: &Value) -> Value {
    match v {
        Value::Int(_) | Value::Float(_) => v.clone(),
        Value::Bool(b) => Value::Int(*b as i64),
        Value::Null => Value::Int(0),
        Value::Str(s) => match super::value::full_numeric(s) {
            Some(super::value::NumericString::Int(i)) => Value::Int(i),
            Some(super::value::NumericString::Float(f)) => Value::Float(f),
            // PHP 8: a non-numeric string in arithmetic is a TypeError; we abstain.
            None => Value::Int(v.to_int()),
        },
        // Arrays/objects/closures never reach here: every arithmetic/unary path
        // excludes them and bails first (`php_add`, `arithmetic_operand`,
        // `php_negate`/`php_unary_plus`).
        Value::Arr(_) | Value::Object { .. } | Value::Closure(_) => v.clone(),
    }
}

/// Returns `true` if a string is a *leading*-numeric but not fully numeric value
/// — PHP 8 throws on such in arithmetic, so we must bail rather than coerce.
fn arithmetic_operand(v: &Value) -> Result<Value, BailReason> {
    match v {
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null => Ok(coerce_number(v)),
        Value::Str(s) => match super::value::full_numeric(s) {
            Some(super::value::NumericString::Int(i)) => Ok(Value::Int(i)),
            Some(super::value::NumericString::Float(f)) => Ok(Value::Float(f)),
            None => Err(BailReason::TypeError(
                "non-numeric string in arithmetic (PHP 8 TypeError)".into(),
            )),
        },
        Value::Arr(_) => Err(BailReason::TypeError("array in arithmetic".into())),
        // An object/closure in arithmetic is a PHP TypeError (no __toString/numeric
        // route in v2) — bail fail-closed (frontier §6).
        Value::Object { .. } | Value::Closure(_) => {
            Err(BailReason::TypeError("object in arithmetic".into()))
        }
    }
}

/// `+` (numeric; array+array is union — not modelled, bails).
fn php_add(l: &Value, r: &Value) -> Result<Value, BailReason> {
    if matches!(l, Value::Arr(_)) || matches!(r, Value::Arr(_)) {
        return Err(BailReason::UnsupportedConstruct("array union (+)".into()));
    }
    php_arith(l, r, i64::checked_add, |a, b| a + b)
}

/// Generic int/float arithmetic: int op when both int AND no overflow; otherwise
/// float. PHP overflow→float is modelled via `checked_*` falling back to `f64`.
fn php_arith(
    l: &Value,
    r: &Value,
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> Result<Value, BailReason> {
    let (a, b) = (arithmetic_operand(l)?, arithmetic_operand(r)?);
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => Ok(match int_op(*x, *y) {
            Some(v) => Value::Int(v),
            None => Value::Float(float_op(*x as f64, *y as f64)), // overflow→float
        }),
        _ => Ok(Value::Float(float_op(a.to_float(), b.to_float()))),
    }
}

/// `/`: int when both int and the result is exact; otherwise float. /0 bails.
fn php_div(l: &Value, r: &Value) -> Result<Value, BailReason> {
    let (a, b) = (arithmetic_operand(l)?, arithmetic_operand(r)?);
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        if *y == 0 {
            return Err(BailReason::DivisionByZero);
        }
        if x % y == 0 {
            return Ok(Value::Int(x / y));
        }
        return Ok(Value::Float(*x as f64 / *y as f64));
    }
    let (af, bf) = (a.to_float(), b.to_float());
    if bf == 0.0 {
        return Err(BailReason::DivisionByZero);
    }
    Ok(Value::Float(af / bf))
}

/// `%`: integer modulo, sign of the dividend (PHP). %0 bails.
fn php_mod(l: &Value, r: &Value) -> Result<Value, BailReason> {
    let (a, b) = (
        arithmetic_operand(l)?.to_int(),
        arithmetic_operand(r)?.to_int(),
    );
    if b == 0 {
        return Err(BailReason::DivisionByZero);
    }
    // i64::MIN % -1 overflows in Rust; PHP yields 0.
    if a == i64::MIN && b == -1 {
        return Ok(Value::Int(0));
    }
    Ok(Value::Int(a % b))
}

/// `**`: int when both int, exponent ≥ 0, and the result fits; otherwise float.
fn php_pow(l: &Value, r: &Value) -> Result<Value, BailReason> {
    let (a, b) = (arithmetic_operand(l)?, arithmetic_operand(r)?);
    if let (Value::Int(base), Value::Int(exp)) = (&a, &b) {
        if *exp >= 0 {
            if let Ok(e) = u32::try_from(*exp) {
                if let Some(v) = base.checked_pow(e) {
                    return Ok(Value::Int(v));
                }
            }
        }
        // negative exponent or overflow → float
    }
    Ok(Value::Float(a.to_float().powf(b.to_float())))
}

/// `.`: string concatenation (byte-exact); both sides coerced to PHP strings.
fn php_concat(l: &Value, r: &Value) -> Result<Value, BailReason> {
    let mut ls = l
        .to_php_string()
        .ok_or_else(|| BailReason::TypeError("concat with array".into()))?;
    let rs = r
        .to_php_string()
        .ok_or_else(|| BailReason::TypeError("concat with array".into()))?;
    ls.extend_from_slice(&rs);
    Ok(Value::Str(ls))
}

// ─── Assignment, ternary, arrays, calls ───────────────────────────────────────

fn eval_assignment(
    a: &mago_syntax::ast::ast::assignment::Assignment,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    use mago_syntax::ast::ast::assignment::AssignmentOperator as Op;

    // `$this->prop = rhs` (or another `$obj->prop`): a property write. Permitted
    // ONLY while seeding a constructor's `$this` (frontier §2 — a mutator in any
    // other body bails, because the by-value model gets aliasing wrong).
    if let Expression::Access(mago_syntax::ast::ast::access::Access::Property(pa)) = a.lhs {
        return eval_property_assignment(a, pa, scope);
    }

    // Only simple `$var <op>= rhs` is modelled (besides the property write above).
    let Expression::Variable(Variable::Direct(target)) = a.lhs else {
        return Err(BailReason::UnsupportedConstruct(
            "assignment to non-simple lvalue".into(),
        ));
    };
    let key = var_name(target.name);
    let rhs = eval_expr(a.rhs, scope)?;

    let new_val = match &a.operator {
        Op::Assign(_) => rhs,
        // Compound assignments reuse the binary ops over the current value.
        Op::Addition(_)
        | Op::Subtraction(_)
        | Op::Multiplication(_)
        | Op::Division(_)
        | Op::Modulo(_)
        | Op::Exponentiation(_)
        | Op::Concat(_) => {
            let cur = scope.vars.get(&key).cloned().ok_or_else(|| {
                BailReason::UnboundVariable(String::from_utf8_lossy(&key).into_owned())
            })?;
            match &a.operator {
                Op::Addition(_) => php_add(&cur, &rhs)?,
                Op::Subtraction(_) => php_arith(&cur, &rhs, i64::checked_sub, |x, y| x - y)?,
                Op::Multiplication(_) => php_arith(&cur, &rhs, i64::checked_mul, |x, y| x * y)?,
                Op::Division(_) => php_div(&cur, &rhs)?,
                Op::Modulo(_) => php_mod(&cur, &rhs)?,
                Op::Exponentiation(_) => php_pow(&cur, &rhs)?,
                Op::Concat(_) => php_concat(&cur, &rhs)?,
                _ => unreachable!(),
            }
        }
        other => {
            return Err(BailReason::UnsupportedConstruct(format!(
                "assignment operator {:?}",
                std::mem::discriminant(other)
            )))
        }
    };

    scope.vars.insert(key, new_val.clone());
    Ok(new_val)
}

/// `$this->prop = rhs` — a property write (constructor seeding only). Frontier §2:
/// permitted only when `scope.allow_this_write` is set; any other body that writes
/// a property is a MUTATOR and BAILS. Only the receiver `$this` and a plain `=`
/// are modelled; a write through any other object reference, or a compound op,
/// bails (the by-value model cannot track aliased writes soundly).
fn eval_property_assignment(
    a: &mago_syntax::ast::ast::assignment::Assignment,
    pa: &mago_syntax::ast::ast::access::PropertyAccess,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    use mago_syntax::ast::ast::assignment::AssignmentOperator as Op;
    use mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector;

    if !scope.allow_this_write {
        return Err(BailReason::UnsupportedConstruct(
            "property write outside a constructor (mutator method)".into(),
        ));
    }
    if !matches!(a.operator, Op::Assign(_)) {
        return Err(BailReason::UnsupportedConstruct(
            "compound property assignment".into(),
        ));
    }
    // Receiver must be `$this`.
    let Expression::Variable(Variable::Direct(recv)) = pa.object else {
        return Err(BailReason::UnsupportedConstruct(
            "property write through a non-$this reference".into(),
        ));
    };
    if var_name(recv.name) != b"this" {
        return Err(BailReason::UnsupportedConstruct(
            "property write through a non-$this reference".into(),
        ));
    }
    let ClassLikeMemberSelector::Identifier(prop_id) = &pa.property else {
        return Err(BailReason::UnsupportedConstruct(
            "dynamic property selector in write".into(),
        ));
    };
    let rhs = eval_expr(a.rhs, scope)?;

    // Functional update of the `$this` record in scope.
    let this = scope.vars.get_mut(b"this".as_slice()).ok_or_else(|| {
        BailReason::UnsupportedConstruct("property write with no bound \\$this".into())
    })?;
    let Value::Object { props, .. } = this else {
        return Err(BailReason::TypeError(
            "property write on a non-object \\$this".into(),
        ));
    };
    let prop_name = prop_id.value.to_vec();
    match props.iter_mut().find(|(k, _)| *k == prop_name) {
        Some(slot) => slot.1 = rhs.clone(),
        None => props.push((prop_name, rhs.clone())),
    }
    Ok(rhs)
}

fn eval_conditional(
    c: &mago_syntax::ast::ast::conditional::Conditional,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    let cond = eval_expr(c.condition, scope)?;
    match c.then {
        // `cond ? then : else`
        Some(then_expr) => {
            if cond.to_bool() {
                eval_expr(then_expr, scope)
            } else {
                eval_expr(c.r#else, scope)
            }
        }
        // `cond ?: else` — returns cond when truthy.
        None => {
            if cond.to_bool() {
                Ok(cond)
            } else {
                eval_expr(c.r#else, scope)
            }
        }
    }
}

fn eval_array(arr: &Array, scope: &mut Scope) -> Result<Value, BailReason> {
    build_array(arr.elements.as_slice(), scope)
}

fn eval_legacy_array(arr: &LegacyArray, scope: &mut Scope) -> Result<Value, BailReason> {
    build_array(arr.elements.as_slice(), scope)
}

fn build_array(elements: &[ArrayElement], scope: &mut Scope) -> Result<Value, BailReason> {
    let mut items: Vec<(ArrayKey, Value)> = Vec::new();
    let mut next_int: i64 = 0;

    for element in elements {
        match element {
            ArrayElement::KeyValue(kv) => {
                let k = eval_expr(kv.key, scope)?;
                let v = eval_expr(kv.value, scope)?;
                let key = k
                    .to_array_key()
                    .ok_or_else(|| BailReason::TypeError("array used as array key".into()))?;
                if let ArrayKey::Int(n) = &key {
                    if *n >= next_int {
                        next_int = n.wrapping_add(1);
                    }
                }
                insert_key(&mut items, key, v);
            }
            ArrayElement::Value(ve) => {
                let v = eval_expr(ve.value, scope)?;
                insert_key(&mut items, ArrayKey::Int(next_int), v);
                next_int += 1;
            }
            ArrayElement::Variadic(_) => {
                return Err(BailReason::UnsupportedConstruct(
                    "array spread (...)".into(),
                ))
            }
            ArrayElement::Missing(_) => {}
        }
    }
    Ok(Value::Arr(items))
}

/// Insert with last-write-wins on a duplicate key (PHP array semantics), keeping
/// the original position of the first occurrence.
fn insert_key(items: &mut Vec<(ArrayKey, Value)>, key: ArrayKey, val: Value) {
    if let Some(slot) = items.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = val;
    } else {
        items.push((key, val));
    }
}

/// Lift an [`ArrayKey`] back to a [`Value`] (for `array_search` / `array_keys`).
fn array_key_to_value(k: &ArrayKey) -> Value {
    match k {
        ArrayKey::Int(i) => Value::Int(*i),
        ArrayKey::Str(s) => Value::Str(s.clone()),
    }
}

// ─── Closures / arrow functions (Inc-4 Task A) ────────────────────────────────

/// Build a `Value::Closure` from a `function (...) use (...) {...}` literal.
/// Capture is BY VALUE from the explicit `use(...)` list (each named variable's
/// current value is copied). `use (&$x)` by-reference capture is impurity → BAIL
/// (frontier). A `static` closure (no `$this`) is fine — and any closure that
/// reads `$this` will bail at the `$this` read anyway (it is not captured here).
fn make_closure(
    c: &mago_syntax::ast::ast::function_like::closure::Closure,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    let mut captured: Vec<(Vec<u8>, Value)> = Vec::new();
    if let Some(use_clause) = &c.use_clause {
        for uv in use_clause.variables.iter() {
            // By-reference capture (`use (&$x)`) cannot be modelled by value → bail.
            if uv.ampersand.is_some() {
                return Err(BailReason::UnsupportedConstruct(
                    "by-reference closure capture (use (&$x))".into(),
                ));
            }
            let name = var_name(uv.variable.name);
            let value = scope.vars.get(&name).cloned().ok_or_else(|| {
                BailReason::UnboundVariable(String::from_utf8_lossy(&name).into_owned())
            })?;
            captured.push((name, value));
        }
    }
    let src = closure_source(c.span(), scope)?;
    Ok(Value::Closure(ClosureRef { src, captured }))
}

/// Slice the owned source bytes of a closure expression from the current file's
/// source (Inc-4 Task 1). Bails (fail-closed) if no source is attached or the span
/// is out of range — never reads dropped/aliased AST memory.
fn closure_source(span: mago_span::Span, scope: &Scope) -> Result<Vec<u8>, BailReason> {
    let source = scope.source.ok_or_else(|| {
        BailReason::UnsupportedConstruct("closure literal without a file source attached".into())
    })?;
    let start = span.start.offset as usize;
    let end = span.end.offset as usize;
    source
        .get(start..end)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| BailReason::Other("closure span out of source range".into()))
}

/// Build a `Value::Closure` from an `fn (...) => expr` arrow function. Arrow
/// functions AUTO-CAPTURE the enclosing scope by value; we copy the WHOLE current
/// variable scope (a sound superset — only the variables the body reads matter,
/// and an unread capture is harmless). `$this` is intentionally NOT copied (an
/// arrow fn that reads `$this` will bail at the read, fail-closed).
fn make_arrow(
    a: &mago_syntax::ast::ast::function_like::arrow_function::ArrowFunction,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    let captured: Vec<(Vec<u8>, Value)> = scope
        .vars
        .iter()
        .filter(|(k, _)| k.as_slice() != b"this")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let src = closure_source(a.span(), scope)?;
    Ok(Value::Closure(ClosureRef { src, captured }))
}

/// Invoke a `Value::Closure` over concrete `args`, returning its result.
///
/// The closure owns its source bytes (`closure.src`, Inc-4 Task 1). We re-parse
/// that source into a FRESH arena that lives for the whole invocation and walk the
/// re-parsed body with the existing evaluator — so a closure whose CREATING arena
/// has dropped (returned from an inlined helper, stored into `$this`) is invoked
/// soundly, no use-after-free. The re-parsed snippet carries NO original-file name
/// table (`names = None`): a pure closure over params + captured values + builtins
/// needs none, and anything that would need the outer file's FQCN table bails
/// fail-closed at the unknown-call/class boundary.
///
/// The bindings are the captured env (by value) overlaid with the bound
/// parameters; the body runs in a FRESH scope (closures do not see the caller's
/// other locals) carrying the same resolver + step budget. For a callback invoked
/// in a loop (`array_map`/`array_filter`/…), parse ONCE via [`ParsedClosure`] and
/// reuse it across iterations rather than re-parsing per element.
fn invoke_closure(
    closure: &ClosureRef,
    args: &[Value],
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    with_parsed_closure(&closure.src, |kind, snippet| {
        invoke_parsed_closure(kind, snippet, &closure.captured, args, scope)
    })
}

/// A re-parsed closure / arrow-function node, borrowing from a local arena that
/// lives only for the duration of [`with_parsed_closure`]'s callback.
enum ParsedClosureKind<'p> {
    Closure(&'p mago_syntax::ast::ast::function_like::closure::Closure<'p>),
    Arrow(&'p mago_syntax::ast::ast::function_like::arrow_function::ArrowFunction<'p>),
}

/// Re-parse `<?php ( <closure-src> );` into a FRESH local arena and call `f` with
/// the single closure / arrow node plus the snippet bytes the node spans into. The
/// arena lives for the whole callback and is dropped after — the borrow never
/// escapes, so this is fully safe (no `unsafe`, no dangling pointer). For a
/// callback invoked in a loop (`array_map`/…), `f` runs the entire loop so the
/// parse happens once. Bails (fail-closed) if the snippet is not exactly one
/// closure expression.
fn with_parsed_closure<R>(
    src: &[u8],
    f: impl FnOnce(&ParsedClosureKind, &[u8]) -> Result<R, BailReason>,
) -> Result<R, BailReason> {
    use mago_database::file::File;
    use mago_syntax::parser::parse_file;

    let arena = bumpalo::Bump::new();
    let mut full: Vec<u8> = Vec::with_capacity(src.len() + 9);
    full.extend_from_slice(b"<?php (");
    full.extend_from_slice(src);
    full.extend_from_slice(b");");
    let file = File::ephemeral(
        std::borrow::Cow::Borrowed(b"closure.php".as_slice()),
        std::borrow::Cow::Owned(full.clone()),
    );
    let program = parse_file(&arena, &file);
    let kind = find_closure_node(program)
        .ok_or_else(|| BailReason::Other("closure source did not re-parse to a closure".into()))?;
    f(&kind, &full)
}

/// Bind `captured` + `args` onto a re-parsed closure node and evaluate its body in
/// a fresh scope. `snippet` is the re-parsed source so a closure literal NESTED in
/// the body can own its own bytes via its span into THIS snippet.
fn invoke_parsed_closure(
    kind: &ParsedClosureKind,
    snippet: &[u8],
    captured: &[(Vec<u8>, Value)],
    args: &[Value],
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    let param_list = match kind {
        ParsedClosureKind::Closure(c) => &c.parameter_list,
        ParsedClosureKind::Arrow(a) => &a.parameter_list,
    };

    let mut bindings: HashMap<Vec<u8>, Value> = HashMap::new();
    // Captured env first; parameters then overlay (a param shadows a capture).
    for (k, v) in captured {
        bindings.insert(k.clone(), v.clone());
    }

    let params: Vec<_> = param_list.parameters.iter().collect();
    if args.len() > params.len() {
        return Err(BailReason::Other(
            "more arguments than closure parameters (variadic call?)".into(),
        ));
    }
    for (i, p) in params.iter().enumerate() {
        if p.ellipsis.is_some() {
            return Err(BailReason::UnsupportedConstruct(
                "variadic closure parameter".into(),
            ));
        }
        if p.ampersand.is_some() {
            return Err(BailReason::UnsupportedConstruct(
                "by-reference closure parameter".into(),
            ));
        }
        let name = var_name(p.variable.name);
        let value = match args.get(i) {
            Some(a) => a.clone(),
            None => match &p.default_value {
                Some(d) => eval_expr(d.value, scope)?,
                None => {
                    return Err(BailReason::UnboundVariable(format!(
                        "closure parameter ${}",
                        String::from_utf8_lossy(&name)
                    )))
                }
            },
        };
        bindings.insert(name, value);
    }

    // Run the body in a fresh scope sharing the resolver/budget (so the step
    // budget stays global and a callback in a loop can still bail on runaway).
    // `source` = the re-parsed snippet (a nested closure literal re-slices from it);
    // no `names`: the snippet has its own (empty) name table, so anything needing
    // the outer file's FQCN table bails fail-closed.
    let mut inner = Scope::new(bindings, scope.resolver).with_source(snippet);
    inner.steps = scope.steps;
    inner.max_steps = scope.max_steps;

    let result = match kind {
        ParsedClosureKind::Closure(c) => {
            match exec_statements(c.body.statements.iter(), &mut inner)? {
                Flow::Returned(v) => v,
                Flow::Normal => Value::Null,
                Flow::Asserted(_) => {
                    return Err(BailReason::UnsupportedConstruct(
                        "assertion inside a closure body".into(),
                    ))
                }
            }
        }
        ParsedClosureKind::Arrow(a) => eval_expr(a.expression, &mut inner)?,
    };
    // Propagate the consumed step budget back to the caller.
    scope.steps = inner.steps;
    Ok(result)
}

/// Find the single closure / arrow-function node in a re-parsed snippet program.
fn find_closure_node<'p>(
    program: &'p mago_syntax::ast::Program<'p>,
) -> Option<ParsedClosureKind<'p>> {
    use mago_syntax::ast::ast::expression::Expression as Expr;

    fn unwrap_expr<'p>(expr: &'p Expr<'p>) -> Option<ParsedClosureKind<'p>> {
        match expr {
            Expr::Closure(c) => Some(ParsedClosureKind::Closure(c)),
            Expr::ArrowFunction(a) => Some(ParsedClosureKind::Arrow(a)),
            Expr::Parenthesized(p) => unwrap_expr(p.expression),
            _ => None,
        }
    }

    for stmt in program.statements.iter() {
        if let Statement::Expression(es) = stmt {
            if let Some(k) = unwrap_expr(es.expression) {
                return Some(k);
            }
        }
    }
    None
}

/// Higher-order builtins that take a closure callback. `Ok(None)` = not one of
/// these (the caller falls through to the scalar builtin path / resolver).
///
/// Modelled, gold-tested vs `php -r` (8.1.33):
/// - `array_map(fn, arr)` (single array): apply `fn($v)` to each value, PRESERVE
///   keys + order. (The multi-array form, where keys are reindexed, is NOT
///   modelled — it bails so we never guess its key handling.)
/// - `array_filter(arr, fn)` (default mode): keep entries where `fn($v)` is
///   truthy, PRESERVE original keys. The `ARRAY_FILTER_USE_KEY/BOTH` modes (a 3rd
///   arg) bail. The no-callback form is the scalar builtin's concern (not here).
/// - `array_reduce(arr, fn, initial?)`: left fold `fn($carry, $v)`; initial
///   defaults to null.
///
/// `usort`/`uasort`/`uksort` are NOT modelled here: they sort the array BY
/// REFERENCE (mutate the caller's variable), which the by-value scope cannot
/// write back soundly → they bail (fail-closed) at the unknown-call boundary.
fn call_closure_builtin(
    name: &[u8],
    args: &[Value],
    scope: &mut Scope,
) -> Result<Option<Value>, BailReason> {
    match (name, args) {
        // array_map(callback, array) — single-array form, keys preserved.
        (b"array_map", [Value::Closure(cl), Value::Arr(items)]) => {
            with_parsed_closure(&cl.src, |kind, snip| {
                let mut out: Vec<(ArrayKey, Value)> = Vec::with_capacity(items.len());
                for (k, v) in items {
                    let mapped = invoke_parsed_closure(
                        kind,
                        snip,
                        &cl.captured,
                        std::slice::from_ref(v),
                        scope,
                    )?;
                    out.push((k.clone(), mapped));
                }
                Ok(Some(Value::Arr(out)))
            })
        }
        // array_map(null, ...) or the multi-array form → bail (not modelled).
        (b"array_map", [Value::Closure(_), _, _, ..]) => Err(BailReason::UnsupportedConstruct(
            "array_map with multiple arrays (key reindexing not modelled)".into(),
        )),
        // array_filter(array, callback) — default mode (callback sees the VALUE),
        // original keys preserved.
        (b"array_filter", [Value::Arr(items), Value::Closure(cl)]) => {
            with_parsed_closure(&cl.src, |kind, snip| {
                let mut out: Vec<(ArrayKey, Value)> = Vec::new();
                for (k, v) in items {
                    let keep = invoke_parsed_closure(
                        kind,
                        snip,
                        &cl.captured,
                        std::slice::from_ref(v),
                        scope,
                    )?;
                    if keep.to_bool() {
                        out.push((k.clone(), v.clone()));
                    }
                }
                Ok(Some(Value::Arr(out)))
            })
        }
        // array_filter with a 3rd `mode` arg (USE_KEY / USE_BOTH) → bail.
        (b"array_filter", [Value::Arr(_), Value::Closure(_), _, ..]) => {
            Err(BailReason::UnsupportedConstruct(
                "array_filter with ARRAY_FILTER_USE_KEY/BOTH mode".into(),
            ))
        }
        // array_reduce(array, callback, initial?) — left fold.
        (b"array_reduce", [Value::Arr(items), Value::Closure(cl)]) => {
            with_parsed_closure(&cl.src, |kind, snip| {
                let mut acc = Value::Null;
                for (_, v) in items {
                    acc =
                        invoke_parsed_closure(kind, snip, &cl.captured, &[acc, v.clone()], scope)?;
                }
                Ok(Some(acc))
            })
        }
        (b"array_reduce", [Value::Arr(items), Value::Closure(cl), initial]) => {
            with_parsed_closure(&cl.src, |kind, snip| {
                let mut acc = initial.clone();
                for (_, v) in items {
                    acc =
                        invoke_parsed_closure(kind, snip, &cl.captured, &[acc, v.clone()], scope)?;
                }
                Ok(Some(acc))
            })
        }
        // PHP 8.4 array predicate/search builtins. Their callback takes (value,
        // key) — value FIRST (RFC: "array_find"/"array_any"/"array_all"). Pure iff
        // the closure is pure. Short-circuit on the first decisive element, exactly
        // as PHP does.
        // array_any(array, fn(value, key)): true iff fn is truthy for ANY element.
        (b"array_any", [Value::Arr(items), Value::Closure(cl)]) => {
            with_parsed_closure(&cl.src, |kind, snip| {
                for (k, v) in items {
                    let args = [v.clone(), array_key_to_value(k)];
                    let hit = invoke_parsed_closure(kind, snip, &cl.captured, &args, scope)?;
                    if hit.to_bool() {
                        return Ok(Some(Value::Bool(true)));
                    }
                }
                Ok(Some(Value::Bool(false)))
            })
        }
        // array_all(array, fn(value, key)): true iff fn is truthy for EVERY element.
        (b"array_all", [Value::Arr(items), Value::Closure(cl)]) => {
            with_parsed_closure(&cl.src, |kind, snip| {
                for (k, v) in items {
                    let args = [v.clone(), array_key_to_value(k)];
                    let hit = invoke_parsed_closure(kind, snip, &cl.captured, &args, scope)?;
                    if !hit.to_bool() {
                        return Ok(Some(Value::Bool(false)));
                    }
                }
                Ok(Some(Value::Bool(true)))
            })
        }
        // array_find(array, fn(value, key)): the first VALUE where fn is truthy,
        // else null.
        (b"array_find", [Value::Arr(items), Value::Closure(cl)]) => {
            with_parsed_closure(&cl.src, |kind, snip| {
                for (k, v) in items {
                    let args = [v.clone(), array_key_to_value(k)];
                    let hit = invoke_parsed_closure(kind, snip, &cl.captured, &args, scope)?;
                    if hit.to_bool() {
                        return Ok(Some(v.clone()));
                    }
                }
                Ok(Some(Value::Null))
            })
        }
        // array_find_key(array, fn(value, key)): the first KEY where fn is truthy,
        // else null.
        (b"array_find_key", [Value::Arr(items), Value::Closure(cl)]) => {
            with_parsed_closure(&cl.src, |kind, snip| {
                for (k, v) in items {
                    let args = [v.clone(), array_key_to_value(k)];
                    let hit = invoke_parsed_closure(kind, snip, &cl.captured, &args, scope)?;
                    if hit.to_bool() {
                        return Ok(Some(array_key_to_value(k)));
                    }
                }
                Ok(Some(Value::Null))
            })
        }
        // usort & friends mutate by reference → bail (fail-closed): a by-value
        // model cannot write the sorted array back to the caller's variable.
        (b"usort" | b"uasort" | b"uksort", _) => Err(BailReason::UnsupportedConstruct(
            "usort/uasort/uksort (sorts an array by reference; not modelled)".into(),
        )),
        _ => Ok(None),
    }
}

fn eval_call(call: &Call, scope: &mut Scope) -> Result<Value, BailReason> {
    use mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector;
    match call {
        Call::Function(fc) => {
            // A non-identifier callee: a variable / expression holding a closure
            // (`$f(args)`). Evaluate it; if it is a Closure, invoke it directly.
            let Some(name) = identifier_name(fc.function) else {
                let callee = eval_expr(fc.function, scope)?;
                if let Value::Closure(cl) = callee {
                    let args = eval_arguments(&fc.argument_list, scope)?;
                    return invoke_closure(&cl, &args, scope);
                }
                return Err(BailReason::UnsupportedConstruct(
                    "dynamic function call (non-closure callee)".into(),
                ));
            };
            let args = eval_arguments(&fc.argument_list, scope)?;
            // Higher-order builtins that take a closure (Task A): these need the
            // scope to invoke the callback, so they are handled here, before the
            // scope-free `call_pure_builtin`. They are pure IFF the closure is pure
            // (a closure body that hits an impurity bails inside `invoke_closure`).
            if let Some(v) = call_closure_builtin(name, &args, scope)? {
                return Ok(v);
            }
            // First try a pure builtin; then the substitution resolver.
            if let Some(v) = call_pure_builtin(name, &args)? {
                return Ok(v);
            }
            if let Some(v) = scope.resolver.resolve_function(name, &args)? {
                return Ok(v);
            }
            Err(BailReason::UnknownCall(
                String::from_utf8_lossy(name).into_owned(),
            ))
        }
        // `$obj->method(args)` — the receiver's RUNTIME class drives dispatch
        // (Task C). Assertions like `$this->assertSame(...)` are intercepted
        // upstream in `try_assertion`; this path is for real instance methods.
        Call::Method(m) => {
            let ClassLikeMemberSelector::Identifier(method_id) = &m.method else {
                return Err(BailReason::UnsupportedConstruct(
                    "dynamic method selector".into(),
                ));
            };
            let receiver = eval_expr(m.object, scope)?;
            if !matches!(receiver, Value::Object { .. }) {
                return Err(BailReason::TypeError(format!(
                    "method call on non-object ({})",
                    receiver.type_name()
                )));
            }
            let args = eval_arguments(&m.argument_list, scope)?;
            match scope
                .resolver
                .resolve_instance_method(&receiver, method_id.value, &args)?
            {
                Some(v) => Ok(v),
                None => Err(BailReason::UnknownCall(format!(
                    "->{}",
                    String::from_utf8_lossy(method_id.value)
                ))),
            }
        }
        // `Class::method(args)` (Task E). self/parent/static → bail (no enclosing
        // class context in the Scope).
        Call::StaticMethod(sm) => {
            let ClassLikeMemberSelector::Identifier(method_id) = &sm.method else {
                return Err(BailReason::UnsupportedConstruct(
                    "dynamic static-method selector".into(),
                ));
            };
            let class = resolve_class_name_in_scope(sm.class, scope)?;
            let args = eval_arguments(&sm.argument_list, scope)?;
            match scope
                .resolver
                .resolve_static_method(&class, method_id.value, &args)?
            {
                Some(v) => Ok(v),
                None => Err(BailReason::UnknownCall(format!(
                    "{}::{}",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(method_id.value)
                ))),
            }
        }
        Call::NullSafeMethod(_) => Err(BailReason::UnsupportedConstruct(
            "null-safe method call (?->)".into(),
        )),
    }
}

/// Resolve a class-name expression to a concrete FQCN, consulting the scope's
/// resolved-names table (Inc-3): an unqualified / `use`-aliased `new ClassName`
/// becomes the real FQCN. `self`/`parent`/`static` still bail (no enclosing-class
/// context). Falls back to the raw identifier when no names table is attached
/// (unit tests) or the identifier is already fully qualified.
fn resolve_class_name_in_scope(expr: &Expression, scope: &Scope) -> Result<Vec<u8>, BailReason> {
    // self/parent/static keyword variants → bail (handled by the raw resolver too,
    // but check here so a names-table hit can never mask them).
    if matches!(
        expr,
        Expression::Self_(_) | Expression::Static(_) | Expression::Parent(_)
    ) {
        return Err(BailReason::UnsupportedConstruct(
            "self/parent/static class reference (no enclosing-class context)".into(),
        ));
    }
    if let Some(name) = identifier_name(expr) {
        if name.eq_ignore_ascii_case(b"self")
            || name.eq_ignore_ascii_case(b"parent")
            || name.eq_ignore_ascii_case(b"static")
        {
            return Err(BailReason::UnsupportedConstruct(
                "self/parent/static class reference (no enclosing-class context)".into(),
            ));
        }
        // Prefer the resolved FQCN for this identifier's position; else the raw name.
        if let Some(fqcn) = scope.resolve_name_at(expr) {
            return Ok(fqcn);
        }
        return Ok(name.to_vec());
    }
    Err(BailReason::UnsupportedConstruct(
        "dynamic/unresolvable class name (new \\$var / static::)".into(),
    ))
}

/// A pure-builtin whitelist (spec §9). Each is exact PHP-8 byte semantics.
/// `Ok(None)` = not a builtin (try the resolver next); `Err` = a builtin the
/// reducer refuses to model (a cursor builtin, or a loose-comparison overload).
fn call_pure_builtin(name: &[u8], args: &[Value]) -> Result<Option<Value>, BailReason> {
    // CURSOR builtins have no observable cursor in our `Arr` model — a test mixing
    // `reset()`/`end()` (which we model as pure first/last) with any of these would
    // diverge, so they HARD-BAIL unconditionally (Task E, inc-2 frontier §7). This
    // makes `reset`/`end` safe to model as pure first/last below.
    if matches!(name, b"key" | b"next" | b"current" | b"prev" | b"each") {
        return Err(BailReason::UnsupportedConstruct(format!(
            "cursor builtin {}() — Arr has no observable internal pointer",
            String::from_utf8_lossy(name)
        )));
    }

    let v = match (name, args) {
        (b"strlen", [Value::Str(s)]) => Value::Int(s.len() as i64),
        (b"count" | b"sizeof", [Value::Arr(a)]) => Value::Int(a.len() as i64),
        (b"abs", [Value::Int(i)]) => match i.checked_abs() {
            Some(v) => Value::Int(v),
            None => Value::Float((*i as f64).abs()), // abs(PHP_INT_MIN) → float
        },
        (b"abs", [Value::Float(f)]) => Value::Float(f.abs()),
        (b"intval", [v]) => Value::Int(v.to_int()),
        (b"is_int" | b"is_integer" | b"is_long", [v]) => Value::Bool(matches!(v, Value::Int(_))),
        (b"is_string", [v]) => Value::Bool(matches!(v, Value::Str(_))),
        (b"is_array", [v]) => Value::Bool(matches!(v, Value::Arr(_))),
        (b"is_bool", [v]) => Value::Bool(matches!(v, Value::Bool(_))),
        (b"is_null", [v]) => Value::Bool(matches!(v, Value::Null)),
        (b"is_float" | b"is_double", [v]) => Value::Bool(matches!(v, Value::Float(_))),

        // ── strict array builtins (Task E) ──
        // reset/end = pure first/last element; empty → false (cursor siblings bail
        // above, so this can never observably diverge).
        (b"reset", [Value::Arr(a)]) => a
            .first()
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Bool(false)),
        (b"end", [Value::Arr(a)]) => a
            .last()
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Bool(false)),

        // in_array(needle, haystack, true) — STRICT only. The loose 2-arg / `false`
        // overload bails (PHP `==` juggling is a divergence risk).
        (b"in_array", [needle, Value::Arr(hay), Value::Bool(true)]) => {
            Value::Bool(hay.iter().any(|(_, v)| v.php_strict_eq(needle)))
        }
        (b"in_array", _) => {
            return Err(BailReason::UnsupportedConstruct(
                "in_array without strict=true (loose == not modelled)".into(),
            ))
        }
        // array_search(needle, haystack, true) — STRICT: returns the KEY or false.
        (b"array_search", [needle, Value::Arr(hay), Value::Bool(true)]) => hay
            .iter()
            .find(|(_, v)| v.php_strict_eq(needle))
            .map(|(k, _)| array_key_to_value(k))
            .unwrap_or(Value::Bool(false)),
        (b"array_search", _) => {
            return Err(BailReason::UnsupportedConstruct(
                "array_search without strict=true (loose == not modelled)".into(),
            ))
        }
        // array_key_exists(key, array): key presence (null value still counts).
        (b"array_key_exists", [k, Value::Arr(a)]) => {
            let key = k
                .to_array_key()
                .ok_or_else(|| BailReason::TypeError("array/object as array key".into()))?;
            Value::Bool(a.iter().any(|(ak, _)| *ak == key))
        }
        // array_keys / array_values: reindex from 0.
        (b"array_keys", [Value::Arr(a)]) => Value::Arr(
            a.iter()
                .enumerate()
                .map(|(i, (k, _))| (ArrayKey::Int(i as i64), array_key_to_value(k)))
                .collect(),
        ),
        (b"array_values", [Value::Arr(a)]) => Value::Arr(
            a.iter()
                .enumerate()
                .map(|(i, (_, v))| (ArrayKey::Int(i as i64), v.clone()))
                .collect(),
        ),
        // array_merge: int keys reindex sequentially; string keys overwrite.
        (b"array_merge", merges) if !merges.is_empty() => {
            let mut out: Vec<(ArrayKey, Value)> = Vec::new();
            let mut next_int: i64 = 0;
            for m in merges {
                let Value::Arr(items) = m else {
                    return Err(BailReason::TypeError("array_merge of a non-array".into()));
                };
                for (k, v) in items {
                    match k {
                        // String/explicit-int keys: int keys APPEND (reindexed),
                        // string keys overwrite-in-place.
                        ArrayKey::Int(_) => {
                            insert_key(&mut out, ArrayKey::Int(next_int), v.clone());
                            next_int += 1;
                        }
                        ArrayKey::Str(_) => insert_key(&mut out, k.clone(), v.clone()),
                    }
                }
            }
            Value::Arr(out)
        }
        _ => return Ok(None),
    };
    Ok(Some(v))
}

/// Evaluate a positional argument list to concrete values. Named/spread args bail.
fn eval_arguments(
    args: &mago_syntax::ast::ast::argument::ArgumentList,
    scope: &mut Scope,
) -> Result<Vec<Value>, BailReason> {
    use mago_syntax::ast::ast::argument::Argument;
    let mut out = Vec::new();
    for arg in args.arguments.iter() {
        match arg {
            Argument::Positional(p) => {
                if p.ellipsis.is_some() {
                    return Err(BailReason::UnsupportedConstruct("argument spread".into()));
                }
                out.push(eval_expr(p.value, scope)?);
            }
            Argument::Named(_) => {
                return Err(BailReason::UnsupportedConstruct("named argument".into()))
            }
        }
    }
    Ok(out)
}

/// The bare name of a call target if it is a plain identifier (`foo`), else None.
fn identifier_name<'a>(expr: &'a Expression<'a>) -> Option<&'a [u8]> {
    use mago_syntax::ast::ast::identifier::Identifier;
    if let Expression::Identifier(id) = expr {
        return Some(match id {
            Identifier::Local(l) => l.value,
            Identifier::Qualified(q) => q.value,
            Identifier::FullyQualified(f) => f.value,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bumpalo::Bump;
    use mago_database::file::File;
    use mago_syntax::ast::ast::statement::Statement;
    use mago_syntax::parser::parse_file;

    /// Parse `<?php <body>` and run `f` against the first method-body-like block
    /// (we wrap the body in a function and grab its block), with the given
    /// initial variable bindings. The arena lives for the closure.
    fn run_body(body: &str, vars: Vec<(&str, Value)>) -> Outcome {
        let full = format!("<?php function __t() {{ {} }}", body);
        let arena = Bump::new();
        let file = File::ephemeral(
            std::borrow::Cow::Borrowed(b"eval.php".as_slice()),
            std::borrow::Cow::Owned(full.into_bytes()),
        );
        let program = parse_file(&arena, &file);
        let block = first_function_block(program).expect("function block");
        let givens: HashMap<Vec<u8>, Value> = vars
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v))
            .collect();
        // Attach the file source so a closure literal in `body` owns its bytes.
        run_method_body_inner(block, givens, &NoResolver, None, Some(&file.contents))
    }

    fn first_function_block<'a>(
        program: &'a mago_syntax::ast::Program<'a>,
    ) -> Option<&'a Block<'a>> {
        for stmt in program.statements.iter() {
            if let Statement::Function(f) = stmt {
                return Some(&f.body);
            }
        }
        None
    }

    /// Evaluate a single expression to a `Value` (via `$__r = <expr>; return $__r;`).
    fn eval_one(expr: &str) -> Result<Value, BailReason> {
        let full = format!("<?php function __t() {{ return {}; }}", expr);
        let arena = Bump::new();
        let file = File::ephemeral(
            std::borrow::Cow::Borrowed(b"e.php".as_slice()),
            std::borrow::Cow::Owned(full.into_bytes()),
        );
        let program = parse_file(&arena, &file);
        let block = first_function_block(program).unwrap();
        let mut scope = Scope::new(HashMap::new(), &NoResolver).with_source(&file.contents);
        // The body is a single `return <expr>;`.
        for stmt in block.statements.iter() {
            if let Statement::Return(ret) = stmt {
                return eval_expr(ret.value.unwrap(), &mut scope);
            }
        }
        panic!("no return");
    }

    // ── arithmetic / overflow (gold vs `php -r 'var_dump(...)'`) ──

    #[test]
    fn int_arithmetic() {
        assert_eq!(eval_one("1 + 2").unwrap(), Value::Int(3));
        assert_eq!(eval_one("10 - 3").unwrap(), Value::Int(7));
        assert_eq!(eval_one("6 * 7").unwrap(), Value::Int(42));
        assert_eq!(eval_one("10 / 2").unwrap(), Value::Int(5));
        assert_eq!(eval_one("6 % 4").unwrap(), Value::Int(2));
        // %: sign of the dividend
        assert_eq!(eval_one("-7 % 3").unwrap(), Value::Int(-1));
        assert_eq!(eval_one("7 % -3").unwrap(), Value::Int(1));
    }

    #[test]
    fn division_to_float() {
        match eval_one("7 / 2").unwrap() {
            Value::Float(f) => assert!((f - 3.5).abs() < 1e-12),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn overflow_promotes_to_float() {
        // php -r 'var_dump(9223372036854775807 + 1);' → float(9.223372036854776E+18)
        match eval_one("9223372036854775807 + 1").unwrap() {
            Value::Float(f) => assert!((f - 9.223372036854776e18).abs() / f.abs() < 1e-12),
            v => panic!("expected float (PHP overflow→float), got {v:?}"),
        }
        // mul overflow
        assert!(matches!(
            eval_one("9223372036854775807 * 2").unwrap(),
            Value::Float(_)
        ));
    }

    #[test]
    fn pow_int_and_float() {
        assert_eq!(eval_one("2 ** 3").unwrap(), Value::Int(8));
        match eval_one("2 ** -1").unwrap() {
            Value::Float(f) => assert!((f - 0.5).abs() < 1e-12),
            v => panic!("{v:?}"),
        }
        assert!(matches!(eval_one("2 ** 63").unwrap(), Value::Float(_))); // overflow
    }

    #[test]
    fn concat_byte_exact() {
        assert_eq!(eval_one("'5' . 3").unwrap(), Value::Str(b"53".to_vec()));
        assert_eq!(eval_one("10 . 20").unwrap(), Value::Str(b"1020".to_vec()));
        assert_eq!(
            eval_one("'a' . true . null").unwrap(),
            Value::Str(b"a1".to_vec())
        );
    }

    #[test]
    fn numeric_string_arithmetic() {
        assert_eq!(eval_one("'5' + 3").unwrap(), Value::Int(8));
        match eval_one("'5.5' + 1").unwrap() {
            Value::Float(f) => assert!((f - 6.5).abs() < 1e-12),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn comparisons_and_logical() {
        assert_eq!(eval_one("1 < 2").unwrap(), Value::Bool(true));
        assert_eq!(eval_one("'1' == '01'").unwrap(), Value::Bool(true));
        assert_eq!(eval_one("0 == 'a'").unwrap(), Value::Bool(false)); // PHP8
        assert_eq!(eval_one("1 === 1.0").unwrap(), Value::Bool(false));
        assert_eq!(eval_one("true && false").unwrap(), Value::Bool(false));
        assert_eq!(eval_one("false || 1").unwrap(), Value::Bool(true));
        assert_eq!(eval_one("(5 <=> 3)").unwrap(), Value::Int(1));
        assert_eq!(eval_one("null ?? 7").unwrap(), Value::Int(7));
    }

    #[test]
    fn casts() {
        assert_eq!(eval_one("(int)'12abc'").unwrap(), Value::Int(12));
        assert_eq!(
            eval_one("(string)1.5").unwrap(),
            Value::Str(b"1.5".to_vec())
        );
        assert_eq!(eval_one("(bool)'0'").unwrap(), Value::Bool(false));
    }

    // ── array subscript read (Task D) ──

    #[test]
    fn subscript_read_over_variable() {
        // $a[$k] on an existing key reads the element (gold: $a=[1,2,3]; $a[1]===2).
        let outcome = run_body("$a = [10, 20, 30]; $this->assertSame(20, $a[1]);", vec![]);
        assert_eq!(outcome, Outcome::Pass);
        // string key
        assert_eq!(
            run_body(
                "$a = ['x' => 'X', 'y' => 'Y']; $this->assertSame('Y', $a['y']);",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn subscript_missing_key_bails_but_coalesce_defaults() {
        // A BARE read of a missing key would warn in PHP → bail (fail-closed).
        assert!(matches!(
            eval_one("[1, 2][5]"),
            Err(BailReason::UnsupportedConstruct(_))
        ));
        // `?? null` over a missing key yields null (no warning).
        assert_eq!(eval_one("([1, 2][5] ?? null)").unwrap(), Value::Null);
        // `?? default` over a missing key yields the default.
        assert_eq!(eval_one("(['a' => 1]['b'] ?? 99)").unwrap(), Value::Int(99));
        // `?? default` over an EXISTING non-null key yields the element.
        assert_eq!(eval_one("(['a' => 1]['a'] ?? 99)").unwrap(), Value::Int(1));
        // `?? default` over an existing NULL-valued key yields the default (isset
        // treats null as unset — gold: ['k'=>null]['k'] ?? 'D' === 'D').
        assert_eq!(
            eval_one("(['k' => null]['k'] ?? 'D')").unwrap(),
            Value::Str(b"D".to_vec())
        );
    }

    // ── strict array builtins (Task E), gold vs `php -r` ──

    #[test]
    fn in_array_strict() {
        // php -r 'var_export(in_array(1,[1],true));' → true; "1" vs 1 → false
        assert_eq!(
            eval_one("in_array(1, [1, 2], true)").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_one("in_array('1', [1, 2], true)").unwrap(),
            Value::Bool(false)
        );
        // strict: 0 !== false
        assert_eq!(
            eval_one("in_array(0, [false], true)").unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_one("in_array(null, [1, null], true)").unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn array_search_strict_returns_key_or_false() {
        // returns the KEY (int or string), or false when absent.
        assert_eq!(
            eval_one("array_search('b', [5 => 'a', 9 => 'b'], true)").unwrap(),
            Value::Int(9)
        );
        assert_eq!(
            eval_one("array_search('x', ['k' => 'a'], true)").unwrap(),
            Value::Bool(false)
        );
        // string key is returned as a string.
        assert_eq!(
            eval_one("array_search(2, ['a' => 1, 'b' => 2], true)").unwrap(),
            Value::Str(b"b".to_vec())
        );
        // strict: array_search(0, [false], true) === false (no match)
        assert_eq!(
            eval_one("array_search(0, [false], true)").unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn loose_in_array_and_search_bail() {
        // The 2-arg (loose `==`) form is NOT modelled — bail rather than risk
        // PHP's juggling surprises.
        assert!(eval_one("in_array(1, [1])").is_err());
        assert!(eval_one("array_search(1, [1])").is_err());
        // Explicit loose flag (false) also bails.
        assert!(eval_one("in_array(1, [1], false)").is_err());
    }

    #[test]
    fn array_key_exists_keys_values_merge() {
        // array_key_exists: true even when the value is null.
        assert_eq!(
            eval_one("array_key_exists('k', ['k' => null])").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_one("array_key_exists('x', ['k' => 1])").unwrap(),
            Value::Bool(false)
        );
        // array_keys / array_values reindex from 0.
        assert_eq!(
            eval_one("array_keys([5 => 'a', 9 => 'b'])").unwrap(),
            Value::Arr(vec![
                (ArrayKey::Int(0), Value::Int(5)),
                (ArrayKey::Int(1), Value::Int(9)),
            ])
        );
        assert_eq!(
            eval_one("array_values([5 => 'a', 9 => 'b'])").unwrap(),
            Value::Arr(vec![
                (ArrayKey::Int(0), Value::Str(b"a".to_vec())),
                (ArrayKey::Int(1), Value::Str(b"b".to_vec())),
            ])
        );
        // array_merge: int keys reindex, string keys overwrite.
        assert_eq!(
            eval_one("array_merge([1, 2], [3, 4])").unwrap(),
            Value::Arr(vec![
                (ArrayKey::Int(0), Value::Int(1)),
                (ArrayKey::Int(1), Value::Int(2)),
                (ArrayKey::Int(2), Value::Int(3)),
                (ArrayKey::Int(3), Value::Int(4)),
            ])
        );
        assert_eq!(
            eval_one("array_merge(['a' => 1], ['a' => 2, 'b' => 3])").unwrap(),
            Value::Arr(vec![
                (ArrayKey::Str(b"a".to_vec()), Value::Int(2)),
                (ArrayKey::Str(b"b".to_vec()), Value::Int(3)),
            ])
        );
    }

    #[test]
    fn reset_end_first_last_pure() {
        // reset = first element, end = last element; empty → false.
        assert_eq!(eval_one("reset([10, 20, 30])").unwrap(), Value::Int(10));
        assert_eq!(eval_one("end([10, 20, 30])").unwrap(), Value::Int(30));
        assert_eq!(eval_one("reset([])").unwrap(), Value::Bool(false));
        assert_eq!(eval_one("end([])").unwrap(), Value::Bool(false));
        // first/last over an associative array (value, not key).
        assert_eq!(
            eval_one("reset(['a' => 1, 'b' => 2])").unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            eval_one("end(['a' => 1, 'b' => 2])").unwrap(),
            Value::Int(2)
        );
    }

    #[test]
    fn cursor_builtins_hard_bail() {
        // Our Arr has no observable cursor; key/next/current/prev/each MUST bail
        // (a test mixing reset()/end() with one of these would diverge — Task E).
        for call in [
            "key([1, 2])",
            "next([1, 2])",
            "current([1, 2])",
            "prev([1, 2])",
            "each([1, 2])",
        ] {
            assert!(
                matches!(eval_one(call), Err(BailReason::UnsupportedConstruct(_))),
                "{call} must hard-bail (no cursor model)"
            );
        }
    }

    #[test]
    fn division_by_zero_bails() {
        assert!(matches!(eval_one("1 / 0"), Err(BailReason::DivisionByZero)));
        assert!(matches!(eval_one("1 % 0"), Err(BailReason::DivisionByZero)));
    }

    #[test]
    fn unbound_variable_bails() {
        assert!(matches!(
            eval_one("$undefined + 1"),
            Err(BailReason::UnboundVariable(_))
        ));
    }

    // ── assertions → Pass/Fail ──

    #[test]
    fn assert_same_pass_and_fail() {
        assert_eq!(
            run_body("$this->assertSame(3, 1 + 2);", vec![]),
            Outcome::Pass
        );
        assert_eq!(
            run_body("$this->assertSame(4, 1 + 2);", vec![]),
            Outcome::Fail("assertSame".into())
        );
        // assertSame is strict: 1 vs 1.0 fails
        assert_eq!(
            run_body("$this->assertSame(1, 1.0);", vec![]),
            Outcome::Fail("assertSame".into())
        );
    }

    #[test]
    fn assert_equals_is_loose() {
        assert_eq!(
            run_body("$this->assertEquals(1, 1.0);", vec![]),
            Outcome::Pass
        );
        assert_eq!(
            run_body("$this->assertEquals('1', 1);", vec![]),
            Outcome::Pass
        );
        assert_eq!(
            run_body("$this->assertEquals(0, 'a');", vec![]),
            Outcome::Fail("assertEquals".into())
        );
    }

    #[test]
    fn assert_true_false_null() {
        assert_eq!(
            run_body("$this->assertTrue(1 === 1);", vec![]),
            Outcome::Pass
        );
        assert_eq!(
            run_body("$this->assertTrue(1);", vec![]),
            Outcome::Fail("assertTrue".into())
        ); // strict: 1 is not true
        assert_eq!(
            run_body("$this->assertFalse(1 > 2);", vec![]),
            Outcome::Pass
        );
        assert_eq!(run_body("$this->assertNull(null);", vec![]), Outcome::Pass);
    }

    #[test]
    fn assert_over_given_args() {
        // The classic provider-driven test: assertSame($expected, $a + $b).
        let outcome = run_body(
            "$this->assertSame($expected, $a + $b);",
            vec![
                ("expected", Value::Int(5)),
                ("a", Value::Int(2)),
                ("b", Value::Int(3)),
            ],
        );
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn first_failing_assertion_wins() {
        let outcome = run_body(
            "$this->assertSame(1, 1); $this->assertSame(2, 3); $this->assertSame(4, 4);",
            vec![],
        );
        assert_eq!(outcome, Outcome::Fail("assertSame".into()));
    }

    // ── control flow ──

    #[test]
    fn if_else_branch() {
        assert_eq!(
            run_body(
                "if ($x > 0) { $this->assertSame('pos', 'pos'); } else { $this->assertSame('neg', 'pos'); }",
                vec![("x", Value::Int(5))]
            ),
            Outcome::Pass
        );
        assert_eq!(
            run_body(
                "if ($x > 0) { $this->assertSame('pos', 'neg'); } else { $this->assertSame('neg', 'neg'); }",
                vec![("x", Value::Int(-1))]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn while_loop_accumulates() {
        // sum 1..3 = 6
        let outcome = run_body(
            "$s = 0; $i = 1; while ($i <= 3) { $s = $s + $i; $i = $i + 1; } $this->assertSame(6, $s);",
            vec![],
        );
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn foreach_sums_array() {
        let outcome = run_body(
            "$s = 0; foreach ([1, 2, 3, 4] as $v) { $s = $s + $v; } $this->assertSame(10, $s);",
            vec![],
        );
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn for_loop_accumulates() {
        // sum 0..4 = 10 (classic C-style for).
        let outcome = run_body(
            "$s = 0; for ($i = 0; $i < 5; $i++) { $s = $s + $i; } $this->assertSame(10, $s);",
            vec![],
        );
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn for_loop_with_break_and_return() {
        // a `return` out of a for propagates; a body assertion failing is terminal.
        assert_eq!(
            run_returning(
                "for ($i = 0; $i < 100; $i = $i + 1) { if ($i == 3) { return $i; } } return -1;",
                vec![]
            )
            .unwrap(),
            Value::Int(3)
        );
    }

    #[test]
    fn for_loop_multi_init_and_step() {
        // PHP allows comma-separated init and step expressions; condition is the
        // LAST condition expression (gold: php -r with $i,$j twin counters).
        let outcome = run_body(
            "$s = 0; for ($i = 0, $j = 10; $i < 3; $i = $i + 1, $j = $j - 1) { $s = $s + $j; } $this->assertSame(27, $s);",
            vec![],
        );
        assert_eq!(outcome, Outcome::Pass);
    }

    #[test]
    fn for_loop_empty_condition_needs_break_or_return() {
        // `for (;;)` with no condition loops forever in PHP; with a return inside it
        // terminates. (Empty condition = always true.)
        assert_eq!(
            run_returning(
                "$i = 0; for (;;) { if ($i >= 4) { return $i; } $i = $i + 1; }",
                vec![]
            )
            .unwrap(),
            Value::Int(4)
        );
    }

    #[test]
    fn for_loop_runaway_bails_on_step_budget() {
        // A genuinely infinite for with no exit must hit the step budget → bail,
        // never hang or guess.
        assert!(matches!(
            run_returning("for (;;) { $x = 1; } return 0;", vec![]),
            Err(BailReason::StepBudget)
        ));
    }

    #[test]
    fn compound_assignment() {
        let outcome = run_body(
            "$x = 10; $x += 5; $x *= 2; $this->assertSame(30, $x);",
            vec![],
        );
        assert_eq!(outcome, Outcome::Pass);
    }

    // ── closures (Inc-4 Task A) ──

    #[test]
    fn direct_arrow_closure_invocation() {
        // $f = fn($x) => $x * 2;  $f(5) === 10
        assert_eq!(
            run_body(
                "$f = fn($x) => $x * 2; $this->assertSame(10, $f(5));",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn direct_closure_block_invocation() {
        // $f = function ($x) { return $x + 1; };  $f(41) === 42
        assert_eq!(
            run_body(
                "$f = function ($x) { return $x + 1; }; $this->assertSame(42, $f(41));",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn closure_use_captures_by_value() {
        // use($n) copies $n at creation; a later reassignment of $n does NOT change
        // the captured value (gold: php -r → 13, not 23).
        assert_eq!(
            run_body(
                "$n = 3; $f = function ($x) use ($n) { return $x + $n; }; $n = 13; $this->assertSame(13, $f(10));",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn arrow_auto_captures_by_value() {
        // fn auto-captures $base by value.
        assert_eq!(
            run_body(
                "$base = 100; $f = fn($x) => $x + $base; $this->assertSame(105, $f(5));",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn array_map_with_closure_is_pure() {
        // array_map(fn, arr): keys preserved, order preserved (gold vs php -r).
        assert_eq!(
            run_body(
                "$r = array_map(fn($x) => $x * $x, [1, 2, 3]); $this->assertSame([1, 4, 9], $r);",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn array_filter_with_closure_preserves_keys() {
        // array_filter keeps ORIGINAL keys (gold: php -r → [1=>2, 3=>4]).
        assert_eq!(
            run_body(
                "$r = array_filter([1, 2, 3, 4], fn($x) => $x % 2 === 0); $this->assertSame([1 => 2, 3 => 4], $r);",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn array_any_all_find_php84_predicates() {
        // PHP 8.4 array_any/array_all/array_find. Callback is (value, key). The
        // semantics are from the RFC (host PHP 8.1 has no these builtins, so the
        // expectations are transcribed from the RFC spec, not php -r).
        // array_any: true if ANY value is even.
        assert_eq!(
            run_body(
                "$this->assertTrue(array_any([1, 3, 4], fn($v, $k) => $v % 2 === 0));",
                vec![]
            ),
            Outcome::Pass
        );
        // array_all: false because not every value is even.
        assert_eq!(
            run_body(
                "$this->assertFalse(array_all([2, 3, 4], fn($v, $k) => $v % 2 === 0));",
                vec![]
            ),
            Outcome::Pass
        );
        // array_find: first value > 2 is 3.
        assert_eq!(
            run_body(
                "$this->assertSame(3, array_find([1, 2, 3, 4], fn($v, $k) => $v > 2));",
                vec![]
            ),
            Outcome::Pass
        );
        // array_find: no match → null.
        assert_eq!(
            run_body(
                "$this->assertNull(array_find([1, 2], fn($v, $k) => $v > 9));",
                vec![]
            ),
            Outcome::Pass
        );
        // The KEY is the SECOND arg (string-keyed array).
        assert_eq!(
            run_body(
                "$this->assertTrue(array_any(['A' => 'a', 'B' => 'b'], fn($v, $k) => $k === 'A' && $v === 'a'));",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn closure_invoking_a_captured_closure() {
        // A closure that captures and INVOKES another closure (the doctrine
        // exists/array_any pattern: an arrow re-dispatches to a captured $p).
        assert_eq!(
            run_body(
                "$p = fn($k, $e) => $k === 1 && $e === 20; $g = fn($v, $key) => (bool) $p($key, $v); $this->assertTrue(array_any([10, 20, 30], $g));",
                vec![]
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn by_reference_use_capture_bails() {
        // use (&$x) by-ref capture is impurity → must bail (frontier).
        assert!(matches!(
            run_body(
                "$n = 0; $f = function () use (&$n) { $n = 5; }; $f(); $this->assertSame(5, $n);",
                vec![]
            ),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn assert_not_equals_on_closures_bails_not_passes() {
        // assertNotEquals($f, $g) on two closures: PHP `==` on closures is reference
        // identity → $f == $g is FALSE → assertNotEquals PASSES in real PHP. The
        // reducer has no heap-identity model, so php_loose_eq short-circuited to
        // false → a false-GREEN Pass. It must BAIL instead (no model, no guess).
        assert!(
            matches!(
                run_body(
                    "$f = fn() => 1; $g = $f; $this->assertNotEquals($f, $g);",
                    vec![]
                ),
                Outcome::Bailed(_)
            ),
            "assertNotEquals on closures must bail, not return a false-green Pass"
        );
    }

    #[test]
    fn assert_equals_on_closures_bails() {
        // assertEquals($f, $g) on closures must bail too (no heap model).
        assert!(matches!(
            run_body(
                "$f = fn() => 1; $g = fn() => 1; $this->assertEquals($f, $g);",
                vec![]
            ),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn closure_loose_eq_operator_bails() {
        // The `==` / `!=` operators on a closure operand must bail (the binary path,
        // not just the assertion helper).
        assert!(matches!(
            run_body(
                "$f = fn() => 1; $r = ($f == $f); $this->assertTrue($r);",
                vec![]
            ),
            Outcome::Bailed(_)
        ));
        assert!(matches!(
            run_body(
                "$f = fn() => 1; $r = ($f != $f); $this->assertFalse($r);",
                vec![]
            ),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn pure_builtin_strlen() {
        assert_eq!(
            run_body("$this->assertSame(5, strlen('hello'));", vec![]),
            Outcome::Pass
        );
        assert_eq!(
            run_body("$this->assertSame(3, count([1, 2, 3]));", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn unknown_call_bails() {
        assert!(matches!(
            run_body("$this->assertSame(1, frobnicate(2));", vec![]),
            Outcome::Bailed(BailReason::UnknownCall(_))
        ));
    }

    #[test]
    fn unmodelled_construct_bails() {
        // `match` is not modelled → bail.
        assert!(matches!(
            run_body("$x = match(1) { 1 => 'a', default => 'b' };", vec![]),
            Outcome::Bailed(_)
        ));
    }

    /// Run a function body to its returned `Value` (the substitution primitive).
    fn run_returning(body: &str, vars: Vec<(&str, Value)>) -> Result<Value, BailReason> {
        let full = format!("<?php function __t() {{ {} }}", body);
        let arena = Bump::new();
        let file = File::ephemeral(
            std::borrow::Cow::Borrowed(b"r.php".as_slice()),
            std::borrow::Cow::Owned(full.into_bytes()),
        );
        let program = parse_file(&arena, &file);
        let block = first_function_block(program).unwrap();
        let bindings: HashMap<Vec<u8>, Value> = vars
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v))
            .collect();
        run_body_returning(block, bindings, &NoResolver)
    }

    #[test]
    fn run_body_returning_reads_return_value() {
        // A pure helper: return $a + $b; over bound params.
        assert_eq!(
            run_returning(
                "return $a + $b;",
                vec![("a", Value::Int(2)), ("b", Value::Int(40))]
            )
            .unwrap(),
            Value::Int(42)
        );
        // No return → null.
        assert_eq!(run_returning("$x = 1;", vec![]).unwrap(), Value::Null);
        // A branch-selected return.
        assert_eq!(
            run_returning(
                "if ($n < 0) { return -$n; } return $n;",
                vec![("n", Value::Int(-7))]
            )
            .unwrap(),
            Value::Int(7)
        );
    }
}
