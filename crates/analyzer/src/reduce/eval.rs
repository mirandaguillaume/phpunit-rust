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

use super::value::{ArrayKey, Value};

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
            allow_this_write: false,
        }
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
    let mut scope = Scope::new(givens, resolver);
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
    let mut scope = Scope::new(bindings, resolver);
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
    let mut scope = Scope::new(bindings, resolver);
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
        Statement::Foreach(f) => exec_foreach(f, scope),
        other => Err(BailReason::UnsupportedConstruct(format!(
            "statement: {}",
            stmt_kind(other)
        ))),
    }
}

fn stmt_kind(s: &Statement) -> &'static str {
    match s {
        Statement::For(_) => "for",
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
            pass(assert_equals(e, a), "assertEquals")
        }
        b"assertNotEquals" => {
            let (e, a) = two_args(args)?;
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
    if matches!(a, Value::Object { .. }) || matches!(b, Value::Object { .. }) {
        return Err(BailReason::UnsupportedConstruct(
            "=== / assertSame on an object (reference identity, no heap model)".into(),
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
    let class = resolve_class_name(inst.class)?;
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

/// `$obj->prop` read (Task D). The receiver must evaluate to a [`Value::Object`];
/// the property name must be a static identifier. A missing property, a non-object
/// receiver, static-property / class-constant / null-safe access all BAIL.
fn eval_access(
    access: &mago_syntax::ast::ast::access::Access,
    scope: &mut Scope,
) -> Result<Value, BailReason> {
    use mago_syntax::ast::ast::access::Access;
    use mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector;
    match access {
        Access::Property(pa) => {
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
        Value::Object { .. } => Err(BailReason::TypeError("negate object".into())),
    }
}

fn php_unary_plus(v: Value) -> Result<Value, BailReason> {
    match v {
        Value::Int(_) | Value::Float(_) => Ok(v),
        Value::Str(_) | Value::Bool(_) | Value::Null => Ok(coerce_number(&v)),
        Value::Arr(_) => Err(BailReason::TypeError("unary + on array".into())),
        Value::Object { .. } => Err(BailReason::TypeError("unary + on object".into())),
    }
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
            let l = eval_expr(lhs, scope)?;
            return Ok(if matches!(l, Value::Null) {
                eval_expr(rhs, scope)?
            } else {
                l
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
        BinaryOperator::Equal(_) => Ok(Value::Bool(l.php_loose_eq(&r))),
        BinaryOperator::NotEqual(_) | BinaryOperator::AngledNotEqual(_) => {
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
        // Arrays/objects never reach here: every arithmetic/unary path excludes
        // them and bails first (`php_add`, `arithmetic_operand`,
        // `php_negate`/`php_unary_plus`).
        Value::Arr(_) | Value::Object { .. } => v.clone(),
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
        // An object in arithmetic is a PHP TypeError (no __toString/numeric route
        // in v2) — bail fail-closed (frontier §6).
        Value::Object { .. } => Err(BailReason::TypeError("object in arithmetic".into())),
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

fn eval_call(call: &Call, scope: &mut Scope) -> Result<Value, BailReason> {
    use mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector;
    match call {
        Call::Function(fc) => {
            let Some(name) = identifier_name(fc.function) else {
                return Err(BailReason::UnsupportedConstruct(
                    "dynamic function call".into(),
                ));
            };
            let args = eval_arguments(&fc.argument_list, scope)?;
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
            let class = resolve_class_name(sm.class)?;
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

/// Resolve a class-name expression to a concrete FQCN (bytes). Only a plain
/// `Identifier` (a real class name) resolves; `self`/`parent`/`static`, `$var`,
/// and any dynamic class expression BAIL (frontier §3 — the reducer has no
/// enclosing-class context and cannot pin a dynamic class soundly).
fn resolve_class_name(expr: &Expression) -> Result<Vec<u8>, BailReason> {
    match identifier_name(expr) {
        Some(name) => {
            // self/parent/static are parsed as identifiers in some positions; reject.
            if name.eq_ignore_ascii_case(b"self")
                || name.eq_ignore_ascii_case(b"parent")
                || name.eq_ignore_ascii_case(b"static")
            {
                return Err(BailReason::UnsupportedConstruct(
                    "self/parent/static class reference (no enclosing-class context)".into(),
                ));
            }
            Ok(name.to_vec())
        }
        None => Err(BailReason::UnsupportedConstruct(
            "dynamic/unresolvable class name (new \\$var / static::)".into(),
        )),
    }
}

/// A tiny pure-builtin whitelist (spec §9 — ≈6 fns). Each is exact PHP-8 byte
/// semantics. `Ok(None)` = not a builtin (try the resolver next).
fn call_pure_builtin(name: &[u8], args: &[Value]) -> Result<Option<Value>, BailReason> {
    let v = match (name, args) {
        (b"strlen", [Value::Str(s)]) => Value::Int(s.len() as i64),
        (b"count", [Value::Arr(a)]) => Value::Int(a.len() as i64),
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
        run_method_body(block, givens, &NoResolver)
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
        let mut scope = Scope::new(HashMap::new(), &NoResolver);
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
    fn compound_assignment() {
        let outcome = run_body(
            "$x = 10; $x += 5; $x *= 2; $this->assertSame(30, $x);",
            vec![],
        );
        assert_eq!(outcome, Outcome::Pass);
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
