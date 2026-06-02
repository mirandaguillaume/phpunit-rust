//! Statement and expression walker for the type tracker.
//!
//! Phase 2 implements a forward, flow-insensitive walk (with local
//! flow-sensitivity for branch narrowing in Task 2.8). Tasks 2.4-2.7 build up
//! the walker; Task 2.10 wires its output into dispatch::resolve.
//!
//! # AST variant findings (mago-syntax 0.26.1)
//!
//! - `new ClassName()` → `Expression::Instantiation(Instantiation)` (NOT `Expression::New`)
//! - `self`, `static`, `parent` → `Expression::Self_`, `Expression::Static`, `Expression::Parent`
//!   (standalone keyword expressions, NOT nested inside Instantiation.class)
//! - `Variable` is an enum: `Variable::Direct(DirectVariable { name: StringIdentifier, .. })`
//!   Variable names from the interner still include the leading `$` sign.
//! - `Assignment` has `lhs: Box<Expression>`, `operator: AssignmentOperator`, `rhs: Box<Expression>`
//! - `Statement::Expression(ExpressionStatement { expression: Box<Expression>, .. })`
//!
//! # Call-site findings (mago-syntax 0.26.1, Task 2.5)
//!
//! All calls are wrapped in a single `Expression::Call(Call)` enum with variants:
//!   - `Call::Method(MethodCall)` for `$x->method(args)`
//!   - `Call::NullSafeMethod(NullSafeMethodCall)` for `$x?->method(args)`
//!   - `Call::StaticMethod(StaticMethodCall)` for `ClassName::method(args)`
//!   - `Call::Function(FunctionCall)` for `fn(args)`
//!
//! `MethodCall.method` / `StaticMethodCall.method` are `ClassLikeMemberSelector`:
//!   - `ClassLikeMemberSelector::Identifier(LocalIdentifier { value: StringIdentifier, .. })`
//!   - `ClassLikeMemberSelector::Variable(...)` — dynamic, fall through to Mixed
//!   - `ClassLikeMemberSelector::Expression(...)` — dynamic, fall through to Mixed
//!
//! `ArgumentList.arguments` is a `TokenSeparatedSequence<Argument>` with `.iter()`.
//! `Argument::Positional(PositionalArgument { value: Expression })` or `Argument::Named(...)`
//!
//! # mago-reflection findings (Task 2.5)
//!
//! - `FunctionLikeReflection.return_type_reflection: Option<FunctionLikeReturnTypeReflection>`
//! - `FunctionLikeReturnTypeReflection.type_reflection: TypeReflection`
//! - `TypeReflection.kind: TypeKind` (the actual type variant)
//! - `TypeKind::Object(ObjectTypeKind::NamedObject { name: StringIdentifier, .. })` → class type
//! - `TypeKind::Union { kinds: Vec<TypeKind> }` — nullable is `Union { kinds: [T, Value(Null)] }`
//! - `TypeKind::Void` / `TypeKind::Never` / `TypeKind::Mixed { .. }` → our `Type::Mixed`
//! - `TypeKind::Scalar(...)` / array / callable → our `Type::Mixed`
//! - `ObjectTypeKind::Self_ { scope }` / `Static { scope }` / `Parent { scope }` → resolve via env
//!
//! `ClassLikeReflection.methods: MemeberCollection<FunctionLikeReflection>`
//! `MemeberCollection.members: HashMap<StringIdentifier, FunctionLikeReflection>`
//!
//! # Property access findings (Task 2.6)
//!
//! Property access is `Expression::Access(Access)` where `Access` is an enum:
//!   - `Access::Property(PropertyAccess { object, property: ClassLikeMemberSelector })` — `$x->prop`
//!   - `Access::NullSafeProperty(NullSafePropertyAccess { object, property: .. })` — `$x?->prop`
//!   - `Access::StaticProperty(StaticPropertyAccess { class, property: Variable })` — `Cls::$prop`
//!   - `Access::ClassConstant(ClassConstantAccess { .. })` — `Cls::CONST`
//!
//! `ClassLikeReflection.properties: MemeberCollection<PropertyReflection>`
//! `MemeberCollection<PropertyReflection>.members: HashMap<StringIdentifier, PropertyReflection>`
//! Key is the raw `StringIdentifier` from `item.variable.name` — includes the leading `$`
//! (e.g., property `$repo` is keyed as `"$repo"`). Methods are keyed lowercase; properties
//! preserve original casing (no lowercasing applied in mago-project-0.26.1 reflector).
//!
//! `PropertyReflection.type_reflection: Option<TypeReflection>` (same shape as return types)
//!
//! IMPORTANT: Constructor-promoted properties (`private Repo $repo` in `__construct`) are stored
//! only in `FunctionLikeParameterReflection.is_promoted_property = true` but are NOT copied into
//! `class.properties.members`. To look up a promoted property's type, search the `__construct`
//! method's parameters. The `lookup_property_type` function handles both regular and promoted.
//!
//! # Source path resolution (Task 2.5)
//!
//! `Source::standalone(interner, name, content)` stores `path: None` and interned `name` as
//! `identifier.0` (a StringIdentifier). To recover the path string: look up
//! `interner.lookup(&source.identifier.0)`. This is the same string passed as `name` to
//! `Source::standalone` — which in `mago_bridge::MagoProject::load` is `path.display().to_string()`.

use std::path::PathBuf;

use super::env::TypeEnv;
use super::type_repr::Type;
use mago_interner::ThreadedInterner;
use mago_reflection::class_like::ClassLikeReflection;
use mago_reflection::class_like::property::PropertyReflection;
use mago_reflection::function_like::FunctionLikeReflection;
use mago_reflection::r#type::kind::{ObjectTypeKind, TypeKind};
use mago_syntax::ast::Expression;
use mago_syntax::ast::Statement;
use mago_syntax::ast::binary::{Binary, BinaryOperator};
use mago_syntax::ast::block::Block;
use mago_syntax::ast::control_flow::r#if::{If, IfBody};
use mago_syntax::ast::r#return::Return;
use mago_syntax::ast::access::Access;
use mago_syntax::ast::assignment::Assignment;
use mago_syntax::ast::call::{Call, FunctionCall, MethodCall, NullSafeMethodCall, StaticMethodCall};
use mago_syntax::ast::class_like::member::ClassLikeMemberSelector;
use mago_syntax::ast::expression::Parenthesized;
use mago_syntax::ast::instantiation::Instantiation;
use mago_syntax::ast::variable::Variable;
use mago_span::HasSpan;

// ── CallSiteEvent ─────────────────────────────────────────────────────────────

/// Emitted by the walker each time it encounters a method or function call.
/// Task 2.10 consumes these to feed dispatch::resolve().
#[derive(Debug, Clone)]
pub struct CallSiteEvent {
    pub line: u32,
    pub receiver: Type,
    pub method_name: String,
    pub callee_class: Option<String>,
    pub callee_file: Option<PathBuf>,
}

// ── WalkerCtx ─────────────────────────────────────────────────────────────────

/// Walker context: env + interner + project + accumulated events.
pub struct WalkerCtx<'a> {
    pub env: TypeEnv,
    pub interner: &'a ThreadedInterner,
    pub project: &'a crate::mago_bridge::MagoProject,
    pub events: Vec<CallSiteEvent>,
    /// Narrowings collected when walking the most recent conditional expression.
    /// Drained by walk_if before entering branches.
    pub pending_narrowings: Vec<crate::types::narrowing::Narrowing>,
    /// Name resolution table for the current module's Program.
    /// Maps identifier byte-offsets to their resolved fully-qualified names.
    /// Consulted by `resolve_class_fqcn` (Task 2.5.2) to translate raw
    /// identifiers at class-name sites to FQCNs.
    pub names: mago_names::ResolvedNames,
    /// Current AST recursion depth and the bound past which `walk_expression`
    /// / `walk_statement_ctx` bail to `Type::Mixed` instead of recursing.
    /// Guards against stack overflow on pathologically nested untrusted PHP
    /// (mirrors the depth guard in `crate::concrete::expr::Context`).
    pub depth: u32,
    pub max_depth: u32,
}

/// Default recursion bound for the type walker. Far above any realistic
/// hand-written or generated PHP expression/statement nesting, far below the
/// depth that would overflow a worker thread's stack.
pub const WALKER_MAX_DEPTH: u32 = 512;

impl<'a> WalkerCtx<'a> {
    pub fn new(
        env: TypeEnv,
        interner: &'a ThreadedInterner,
        project: &'a crate::mago_bridge::MagoProject,
        names: mago_names::ResolvedNames,
    ) -> Self {
        Self {
            env,
            interner,
            project,
            events: Vec::new(),
            pending_narrowings: Vec::new(),
            names,
            depth: 0,
            max_depth: WALKER_MAX_DEPTH,
        }
    }

    pub fn emit(&mut self, ev: CallSiteEvent) {
        self.events.push(ev);
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Walk a sequence of statements, updating `env` in order.
///
/// Returns nothing — callers query specific variables from `env` afterwards.
pub fn walk_statements(
    env: &mut TypeEnv,
    interner: &ThreadedInterner,
    stmts: impl IntoIterator<Item = impl AsRef<Statement>>,
) {
    for stmt in stmts {
        walk_statement(env, interner, stmt.as_ref());
    }
}

/// Walk a single statement. For expression statements, delegates to
/// `walk_expression`; other statement kinds are ignored in Task 2.4.
pub fn walk_statement(env: &mut TypeEnv, interner: &ThreadedInterner, stmt: &Statement) {
    if let Statement::Expression(expr_stmt) = stmt {
        walk_expression_simple(env, interner, &expr_stmt.expression);
    }
}

/// Walk an expression with only env + interner (no project/event collection).
/// Used by the backward-compat helpers in tests.
pub fn walk_expression_simple(
    env: &mut TypeEnv,
    interner: &ThreadedInterner,
    expr: &Expression,
) -> Type {
    match expr {
        Expression::Literal(_) => Type::Mixed,
        Expression::Variable(v) => walk_variable_simple(env, interner, v),
        Expression::Parenthesized(p) => walk_expression_simple(env, interner, &p.expression),
        Expression::Instantiation(inst) => walk_instantiation_simple(env, interner, inst),
        Expression::Assignment(a) => walk_assignment_simple(env, interner, a),
        // Call expressions: without a project we can't resolve return types or
        // emit events, but we do need to keep progressing so assignments like
        // `$x = $obj->method()` work (return Mixed for now).
        Expression::Call(_) => Type::Mixed,
        _ => Type::Mixed,
    }
}

/// Walk an expression in a full `WalkerCtx` (env + interner + project + events).
///
/// Bounds recursion depth: past `ctx.max_depth` nested expressions/statements
/// it bails to `Type::Mixed` (and stops walking the subtree) so pathologically
/// nested untrusted PHP cannot overflow the stack. Mirrors the guard in
/// `crate::concrete::expr::compute`.
pub fn walk_expression(ctx: &mut WalkerCtx, expr: &Expression) -> Type {
    if ctx.depth >= ctx.max_depth {
        return Type::Mixed;
    }
    ctx.depth += 1;
    let result = walk_expression_inner(ctx, expr);
    ctx.depth -= 1;
    result
}

fn walk_expression_inner(ctx: &mut WalkerCtx, expr: &Expression) -> Type {
    match expr {
        Expression::Literal(_) => Type::Mixed,
        Expression::Variable(v) => walk_variable(ctx, v),
        Expression::Parenthesized(p) => walk_parenthesized(ctx, p),
        Expression::Instantiation(inst) => walk_instantiation(ctx, inst),
        Expression::Assignment(a) => walk_assignment(ctx, a),
        Expression::Call(call) => walk_call(ctx, call),
        Expression::Access(access) => walk_access(ctx, access),
        Expression::Binary(b) => walk_binary(ctx, b),
        // `match (subject) { conds => expr, default => expr }` (M1). Walk the
        // subject plus every arm's condition and result expression so call
        // sites reachable only through a match arm still emit events. Match is
        // an expression, so this also covers `$r = match (...) { ... };`.
        Expression::Match(m) => {
            walk_expression(ctx, &m.expression);
            for arm in m.arms.iter() {
                match arm {
                    mago_syntax::ast::MatchArm::Expression(a) => {
                        for cond in a.conditions.iter() {
                            walk_expression(ctx, cond);
                        }
                        walk_expression(ctx, &a.expression);
                    }
                    mago_syntax::ast::MatchArm::Default(a) => {
                        walk_expression(ctx, &a.expression);
                    }
                }
            }
            Type::Mixed
        }
        _ => Type::Mixed,
    }
}

// ── simple (no-project) helpers ───────────────────────────────────────────────

fn walk_variable_simple(env: &mut TypeEnv, interner: &ThreadedInterner, v: &Variable) -> Type {
    match var_name(interner, v) {
        Some(name) => env.lookup(&name),
        None => Type::Mixed,
    }
}

fn walk_instantiation_simple(
    env: &mut TypeEnv,
    interner: &ThreadedInterner,
    inst: &Instantiation,
) -> Type {
    match inst.class.as_ref() {
        Expression::Self_(_) => match env.enclosing_class() {
            Some(cls) => Type::SelfRef(cls.to_string()),
            None => Type::Mixed,
        },
        Expression::Static(_) => match env.enclosing_class() {
            Some(cls) => Type::StaticRef(cls.to_string()),
            None => Type::Mixed,
        },
        Expression::Parent(_) => Type::Mixed,
        Expression::Identifier(id) => Type::Class(interner.lookup(id.value()).to_string()),
        _ => Type::Mixed,
    }
}

fn walk_assignment_simple(
    env: &mut TypeEnv,
    interner: &ThreadedInterner,
    a: &Assignment,
) -> Type {
    let rhs_type = walk_expression_simple(env, interner, &a.rhs);
    use mago_syntax::ast::assignment::AssignmentOperator;
    if matches!(a.operator, AssignmentOperator::Assign(_)) {
        if let Expression::Variable(v) = a.lhs.as_ref() {
            if let Some(name) = var_name(interner, v) {
                env.set(name, rhs_type.clone());
            }
        }
    }
    rhs_type
}

// ── ctx helpers ───────────────────────────────────────────────────────────────

fn walk_variable(ctx: &mut WalkerCtx, v: &Variable) -> Type {
    match var_name(ctx.interner, v) {
        Some(name) => ctx.env.lookup(&name),
        None => Type::Mixed,
    }
}

fn walk_parenthesized(ctx: &mut WalkerCtx, p: &Parenthesized) -> Type {
    walk_expression(ctx, &p.expression)
}

fn walk_instantiation(ctx: &mut WalkerCtx, inst: &Instantiation) -> Type {
    // Walk arguments first so nested expressions emit their own events.
    if let Some(arg_list) = &inst.argument_list {
        walk_argument_list(ctx, arg_list);
    }

    let class_type = match inst.class.as_ref() {
        Expression::Self_(_) => match ctx.env.enclosing_class() {
            Some(cls) => Type::SelfRef(cls.to_string()),
            None => Type::Mixed,
        },
        Expression::Static(_) => match ctx.env.enclosing_class() {
            Some(cls) => Type::StaticRef(cls.to_string()),
            None => Type::Mixed,
        },
        Expression::Parent(_) => Type::Mixed,
        Expression::Identifier(id) => {
            let class_name = resolve_class_fqcn(ctx, id);
            match class_name.to_lowercase().as_str() {
                "self" => match ctx.env.enclosing_class() {
                    Some(cls) => Type::SelfRef(cls.to_string()),
                    None => Type::Mixed,
                },
                "static" => match ctx.env.enclosing_class() {
                    Some(cls) => Type::StaticRef(cls.to_string()),
                    None => Type::Mixed,
                },
                "parent" => Type::Mixed,
                _ => Type::Class(class_name),
            }
        }
        _ => Type::Mixed,
    };

    // Emit a __construct call-site event so the tracer can recurse into the
    // constructor body (Phase 2: instantiation tracing).
    if !matches!(class_type, Type::Mixed) {
        let line = line_of_span(ctx, inst.span());
        let (callee_class, callee_file) =
            resolve_callee(ctx.project, ctx.interner, &class_type, &ctx.env);
        ctx.emit(CallSiteEvent {
            line,
            receiver: class_type.clone(),
            method_name: "__construct".to_string(),
            callee_class,
            callee_file,
        });
    }

    class_type
}

fn walk_assignment(ctx: &mut WalkerCtx, a: &Assignment) -> Type {
    let rhs_type = walk_expression(ctx, &a.rhs);
    use mago_syntax::ast::assignment::AssignmentOperator;
    if matches!(a.operator, AssignmentOperator::Assign(_)) {
        if let Expression::Variable(v) = a.lhs.as_ref() {
            if let Some(name) = var_name(ctx.interner, v) {
                ctx.env.set(name, rhs_type.clone());
            }
        }
    }
    rhs_type
}

// ── Call dispatch ─────────────────────────────────────────────────────────────

fn walk_call(ctx: &mut WalkerCtx, call: &Call) -> Type {
    match call {
        Call::Method(mc) => walk_method_call(ctx, mc),
        Call::NullSafeMethod(mc) => walk_null_safe_method_call(ctx, mc),
        Call::StaticMethod(smc) => walk_static_method_call(ctx, smc),
        Call::Function(fc) => walk_function_call(ctx, fc),
    }
}

fn walk_method_call(ctx: &mut WalkerCtx, call: &MethodCall) -> Type {
    // Walk receiver to get its type (and propagate any nested call events).
    let recv_type = walk_expression(ctx, &call.object);

    // Extract static method name, fall through to Mixed for dynamic selectors.
    let method_name = match selector_name(ctx.interner, &call.method) {
        Some(n) => n,
        None => return walk_args_and_return_mixed(ctx, &call.argument_list),
    };

    let line = line_of_span(ctx, call.object.span().join(call.argument_list.span()));
    let (callee_class, callee_file) = resolve_callee(ctx.project, ctx.interner, &recv_type, &ctx.env);
    let return_type = lookup_return_type(ctx.project, ctx.interner, &recv_type, &method_name, &ctx.env);

    // Walk arguments for side effects (nested calls).
    walk_argument_list(ctx, &call.argument_list);

    ctx.emit(CallSiteEvent {
        line,
        receiver: recv_type,
        method_name,
        callee_class,
        callee_file,
    });

    return_type
}

fn walk_null_safe_method_call(ctx: &mut WalkerCtx, call: &NullSafeMethodCall) -> Type {
    // Same logic as method call — nullable operator doesn't change type resolution.
    let recv_type = walk_expression(ctx, &call.object);

    let method_name = match selector_name(ctx.interner, &call.method) {
        Some(n) => n,
        None => return walk_args_and_return_mixed(ctx, &call.argument_list),
    };

    let line = line_of_span(ctx, call.object.span().join(call.argument_list.span()));
    let (callee_class, callee_file) = resolve_callee(ctx.project, ctx.interner, &recv_type, &ctx.env);
    let return_type = lookup_return_type(ctx.project, ctx.interner, &recv_type, &method_name, &ctx.env);

    walk_argument_list(ctx, &call.argument_list);

    ctx.emit(CallSiteEvent {
        line,
        receiver: recv_type,
        method_name,
        callee_class,
        callee_file,
    });

    return_type
}

fn walk_static_method_call(ctx: &mut WalkerCtx, call: &StaticMethodCall) -> Type {
    // The "class" expression for `ClassName::method()` is typically an Identifier.
    // `Expression::Identifier` is not handled by `walk_expression` (it falls through
    // to Mixed), so we resolve it here via FQCN lookup before falling back.
    let class_type = match call.class.as_ref() {
        Expression::Self_(_) => match ctx.env.enclosing_class() {
            Some(cls) => Type::SelfRef(cls.to_string()),
            None => Type::Mixed,
        },
        Expression::Static(_) => match ctx.env.enclosing_class() {
            Some(cls) => Type::StaticRef(cls.to_string()),
            None => Type::Mixed,
        },
        Expression::Parent(_) => Type::Mixed,
        Expression::Identifier(id) => {
            let class_name = resolve_class_fqcn(ctx, id);
            match class_name.to_lowercase().as_str() {
                "self" => match ctx.env.enclosing_class() {
                    Some(cls) => Type::SelfRef(cls.to_string()),
                    None => Type::Mixed,
                },
                "static" => match ctx.env.enclosing_class() {
                    Some(cls) => Type::StaticRef(cls.to_string()),
                    None => Type::Mixed,
                },
                "parent" => Type::Mixed,
                _ => Type::Class(class_name),
            }
        }
        _ => walk_expression(ctx, &call.class),
    };

    let method_name = match selector_name(ctx.interner, &call.method) {
        Some(n) => n,
        None => return walk_args_and_return_mixed(ctx, &call.argument_list),
    };

    let line = line_of_span(ctx, call.class.span().join(call.argument_list.span()));
    let (callee_class, callee_file) = resolve_callee(ctx.project, ctx.interner, &class_type, &ctx.env);
    let return_type = lookup_return_type(ctx.project, ctx.interner, &class_type, &method_name, &ctx.env);

    walk_argument_list(ctx, &call.argument_list);

    ctx.emit(CallSiteEvent {
        line,
        receiver: class_type,
        method_name,
        callee_class,
        callee_file,
    });

    return_type
}

fn walk_function_call(ctx: &mut WalkerCtx, call: &FunctionCall) -> Type {
    // Invokable object: $obj($args) is sugar for $obj->__invoke($args).
    // When the callee is a variable holding a class type, emit a __invoke call site.
    if let Expression::Variable(_) = call.function.as_ref() {
        let recv_type = walk_expression(ctx, &call.function);
        if matches!(recv_type, Type::Class(_) | Type::SelfRef(_) | Type::StaticRef(_)
                             | Type::This | Type::Nullable(_)) {
            let line = line_of_span(ctx, call.function.span().join(call.argument_list.span()));
            let (callee_class, callee_file) =
                resolve_callee(ctx.project, ctx.interner, &recv_type, &ctx.env);
            let return_type =
                lookup_return_type(ctx.project, ctx.interner, &recv_type, "__invoke", &ctx.env);
            walk_argument_list(ctx, &call.argument_list);
            ctx.emit(CallSiteEvent {
                line,
                receiver: recv_type,
                method_name: "__invoke".into(),
                callee_class,
                callee_file,
            });
            return return_type;
        }
    }

    // For top-level function calls we only walk arguments for side effects.
    // Function return-type lookup via mago-reflection (function_like_reflections)
    // is deferred to a later task; emit a minimal event with no receiver.
    let fn_name = match call.function.as_ref() {
        Expression::Identifier(id) => ctx.interner.lookup(id.value()).to_string(),
        _ => {
            walk_args_and_return_mixed(ctx, &call.argument_list);
            return Type::Mixed;
        }
    };

    let line = line_of_span(ctx, call.function.span().join(call.argument_list.span()));

    walk_argument_list(ctx, &call.argument_list);

    ctx.emit(CallSiteEvent {
        line,
        receiver: Type::Mixed, // no object receiver for function calls
        method_name: fn_name,
        callee_class: None,
        callee_file: None,
    });

    Type::Mixed
}

// ── Property access ───────────────────────────────────────────────────────────

/// Dispatch `$x->prop`, `$x?->prop`, `ClassName::$prop`, and `Cls::CONST`.
fn walk_access(ctx: &mut WalkerCtx, access: &Access) -> Type {
    match access {
        Access::Property(prop) => {
            let recv_type = walk_expression(ctx, &prop.object);
            let prop_name = match selector_name(ctx.interner, &prop.property) {
                Some(n) => n,
                None => return Type::Mixed,
            };
            resolve_property_type(ctx, &recv_type, &prop_name)
        }
        Access::NullSafeProperty(prop) => {
            // Treat nullable-safe access the same as regular in Phase 2.
            let recv_type = walk_expression(ctx, &prop.object);
            let prop_name = match selector_name(ctx.interner, &prop.property) {
                Some(n) => n,
                None => return Type::Mixed,
            };
            resolve_property_type(ctx, &recv_type, &prop_name)
        }
        // StaticProperty (`ClassName::$prop`) → Mixed in Phase 2.
        Access::StaticProperty(_) => Type::Mixed,
        // ClassConstant (`Cls::CONST` / `Cls::class`) — handle magic `::class` literal.
        Access::ClassConstant(cca) => walk_class_constant_access(ctx, cca),
    }
}

/// Handle `Cls::CONST` and the magic `Cls::class` literal.
///
/// Phase 2.5: only `Cls::class` is modelled — it evaluates to the FQCN string,
/// so we return `Type::Class(fqcn)`. All other class constants return `Type::Mixed`.
fn walk_class_constant_access(
    ctx: &mut WalkerCtx,
    cca: &mago_syntax::ast::access::ClassConstantAccess,
) -> Type {
    use mago_syntax::ast::class_like::member::ClassLikeConstantSelector;

    // Check whether the constant selector is the magic `class` keyword.
    let is_class_literal = match &cca.constant {
        ClassLikeConstantSelector::Identifier(id) => {
            ctx.interner.lookup(&id.value).to_lowercase() == "class"
        }
        ClassLikeConstantSelector::Expression(_) => false,
    };

    if is_class_literal {
        if let Expression::Identifier(id) = cca.class.as_ref() {
            return Type::Class(resolve_class_fqcn(ctx, id));
        }
    }

    Type::Mixed
}

/// Dispatch binary expressions. For `instanceof`, emit a narrowing fact into
/// `ctx.pending_narrowings`; all other binary ops return `Type::Mixed`.
fn walk_binary(ctx: &mut WalkerCtx, b: &Binary) -> Type {
    if matches!(b.operator, BinaryOperator::Instanceof(_)) {
        walk_instanceof(ctx, b)
    } else {
        // Walk both sides for side-effects (nested call sites, assignments).
        walk_expression(ctx, &b.lhs);
        walk_expression(ctx, &b.rhs);
        Type::Mixed
    }
}

/// Handle `$x instanceof ClassName`.
///
/// Emits a narrowing fact into `ctx.pending_narrowings` so that the enclosing
/// `walk_if` can apply it to the true branch's env.  Returns `Type::Mixed`
/// (we don't model the bool result in Phase 2).
fn walk_instanceof(ctx: &mut WalkerCtx, b: &Binary) -> Type {
    // Walk subject for side effects.
    let _subject_type = walk_expression(ctx, &b.lhs);

    // Only narrow when subject is a simple direct variable.
    let var = match b.lhs.as_ref() {
        Expression::Variable(mago_syntax::ast::variable::Variable::Direct(dv)) => {
            ctx.interner.lookup(&dv.name).to_string()
        }
        _ => return Type::Mixed,
    };

    // Extract class name from RHS: Identifier or Self_/Static keywords.
    let class_type: Type = match b.rhs.as_ref() {
        Expression::Identifier(id) => {
            let name = resolve_class_fqcn(ctx, id);
            match name.to_lowercase().as_str() {
                "self" => ctx.env.enclosing_class()
                    .map(|c| Type::SelfRef(c.to_string()))
                    .unwrap_or(Type::Mixed),
                "static" => ctx.env.enclosing_class()
                    .map(|c| Type::StaticRef(c.to_string()))
                    .unwrap_or(Type::Mixed),
                _ => Type::Class(name),
            }
        }
        Expression::Self_(_) => ctx.env.enclosing_class()
            .map(|c| Type::SelfRef(c.to_string()))
            .unwrap_or(Type::Mixed),
        Expression::Static(_) => ctx.env.enclosing_class()
            .map(|c| Type::StaticRef(c.to_string()))
            .unwrap_or(Type::Mixed),
        _ => return Type::Mixed,
    };

    ctx.pending_narrowings.push(crate::types::narrowing::Narrowing { var, ty: class_type });
    Type::Mixed
}

/// Resolve the receiver type to a class FQCN and look up the property type.
fn resolve_property_type(ctx: &WalkerCtx, recv_type: &Type, prop_name: &str) -> Type {
    let fqcn = match recv_type {
        Type::Class(c) | Type::SelfRef(c) | Type::StaticRef(c) => c.clone(),
        Type::This => match ctx.env.enclosing_class() {
            Some(c) => c.to_string(),
            None => return Type::Mixed,
        },
        Type::Nullable(inner) => match inner.as_ref() {
            Type::Class(c) | Type::SelfRef(c) | Type::StaticRef(c) => c.clone(),
            Type::This => match ctx.env.enclosing_class() {
                Some(c) => c.to_string(),
                None => return Type::Mixed,
            },
            _ => return Type::Mixed,
        },
        Type::Interface(c) | Type::Mock(c) => c.clone(),
        _ => return Type::Mixed,
    };
    if fqcn.is_empty() {
        return Type::Mixed;
    }
    lookup_property_type(ctx.project, ctx.interner, &fqcn, prop_name)
}

/// Walk the inheritance chain to find the declared type of a property.
///
/// Properties in `MemeberCollection.members` are keyed by the raw variable
/// `StringIdentifier` including the leading `$` (e.g., key is `"$repo"` for
/// `private Repo $repo`). However, in the AST `$this->repo` the property selector
/// gives `"repo"` (no `$`). We therefore match with a `$`-prefix added.
///
/// Constructor-promoted properties (`private Repo $repo` in `__construct`) are
/// NOT stored in `properties.members`; they only appear as parameters in the
/// `__construct` method reflection. We fall back to scanning `__construct`
/// parameters when the direct lookup misses.
fn lookup_property_type(
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    fqcn: &str,
    property_name: &str,
) -> Type {
    // The key in properties.members includes the leading `$` (raw DirectVariable name),
    // but the property selector from `$this->prop` gives "prop" without `$`.
    // Build both forms so we can match either.
    let key_with_dollar = format!("${}", property_name);
    let key_plain = property_name.to_string();

    let mut current = Some(fqcn.to_lowercase());
    for _ in 0..50 {
        let Some(class_fqcn) = current.take() else {
            return Type::Mixed;
        };

        let Some(refl) = project.find_class_reflection(&class_fqcn) else {
            return Type::Mixed;
        };

        // 1. Check plain declared properties.
        for (id, prop_refl) in refl.properties.members.iter() {
            let raw = interner.lookup(id);
            // raw is the interned variable name, e.g. "$repo" or "repo" (try both).
            if raw == key_with_dollar || raw == key_plain {
                return reflect_property_type(project, interner, prop_refl);
            }
        }

        // 2. Fall back: check __construct promoted properties.
        // Methods are keyed by lowercase StringIdentifier; look up by iterating since
        // `interner.get` may not have "__construct" if it wasn't interned by this interner.
        let construct_refl = refl.methods.members.iter().find_map(|(mid, m)| {
            let name = interner.lookup(mid);
            if name == "__construct" { Some(m) } else { None }
        });
        if let Some(ctor) = construct_refl {
            for param in &ctor.parameters {
                if !param.is_promoted_property {
                    continue;
                }
                // param.name is the StringIdentifier for the variable (includes `$`).
                let raw_param = interner.lookup(&param.name);
                if raw_param == key_with_dollar || raw_param == key_plain {
                    let Some(t_refl) = param.type_reflection.as_ref() else {
                        return Type::Mixed;
                    };
                    return type_kind_to_type(project, interner, &t_refl.kind, &TypeEnv::default());
                }
            }
        }

        // Climb to parent.
        current = refl.inheritance.direct_extended_class.as_ref().map(|n| {
            interner.lookup(&n.value).to_string().to_lowercase()
        });
    }
    Type::Mixed
}

/// Convert a `PropertyReflection`'s declared type into our `Type`.
fn reflect_property_type(
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    prop_refl: &PropertyReflection,
) -> Type {
    let Some(t_refl) = prop_refl.type_reflection.as_ref() else {
        return Type::Mixed;
    };
    type_kind_to_type(project, interner, &t_refl.kind, &TypeEnv::default())
}

// ── Resolution helpers ────────────────────────────────────────────────────────

/// Extract an identifier name from a `ClassLikeMemberSelector`.
/// Returns `None` for dynamic (`Variable`/`Expression`) selectors.
fn selector_name(interner: &ThreadedInterner, sel: &ClassLikeMemberSelector) -> Option<String> {
    match sel {
        // `ClassLikeMemberSelector::Identifier(LocalIdentifier { value: StringIdentifier })`.
        ClassLikeMemberSelector::Identifier(id) => Some(interner.lookup(&id.value).to_string()),
        // Dynamic selector — can't statically resolve.
        ClassLikeMemberSelector::Variable(_) | ClassLikeMemberSelector::Expression(_) => None,
    }
}

/// Walk all arguments of an argument list (for side effects: nested calls → events).
fn walk_argument_list(
    ctx: &mut WalkerCtx,
    arg_list: &mago_syntax::ast::argument::ArgumentList,
) {
    use mago_syntax::ast::argument::Argument;
    for arg in arg_list.arguments.iter() {
        let expr = match arg {
            Argument::Positional(a) => &a.value,
            Argument::Named(a) => &a.value,
        };
        walk_expression(ctx, expr);
    }
}

/// Walk arguments and return `Type::Mixed`.
///
/// Used when the call can't be resolved (dynamic selector, etc.)
/// but we still want to recurse into arguments for nested events.
fn walk_args_and_return_mixed(
    ctx: &mut WalkerCtx,
    arg_list: &mago_syntax::ast::argument::ArgumentList,
) -> Type {
    walk_argument_list(ctx, arg_list);
    Type::Mixed
}

/// Resolve the callee class FQCN and source file from a receiver type.
///
/// Returns `(Some(fqcn), Some(path))` when the class is found in the project,
/// `(Some(fqcn), None)` when the class name is known but not in this project,
/// `(None, None)` for unresolvable receivers (Mixed, Union, etc.).
fn resolve_callee(
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    recv_type: &Type,
    env: &TypeEnv,
) -> (Option<String>, Option<PathBuf>) {
    let fqcn = match recv_type {
        Type::Class(c) | Type::SelfRef(c) | Type::StaticRef(c) => Some(c.clone()),
        Type::This => env.enclosing_class().map(|c| c.to_string()),
        // Nullable: recurse into the inner type.
        Type::Nullable(inner) => return resolve_callee(project, interner, inner, env),
        // Interface/Mock carry a name but are like Class for resolution purposes.
        Type::Interface(c) | Type::Mock(c) => Some(c.clone()),
        Type::Mixed | Type::Union(_, _) => None,
    };

    let Some(fqcn) = fqcn else { return (None, None) };

    // Find the class-like reflection whose name matches the FQCN (case-insensitive,
    // PHP class names are case-insensitive).
    let file = project.find_class_reflection(&fqcn).and_then(|refl| {
        // The source path is stored as the interned `name` string passed to
        // `Source::standalone` (= the file path from `path.display()`).
        // `source.path` is None for standalone sources; use identifier.0 instead.
        let src_id = refl.span.start.source;
        project.source_by_id(src_id).map(|src| {
            let name_str = interner.lookup(&src.identifier.0).to_string();
            PathBuf::from(name_str)
        })
    });

    (Some(fqcn), file)
}

/// Look up the declared return type of a method via mago-reflection.
///
/// Returns `Type::Mixed` when the class, method, or return type is not found.
fn lookup_return_type(
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    recv_type: &Type,
    method: &str,
    env: &TypeEnv,
) -> Type {
    let fqcn = match recv_type {
        Type::Class(c) | Type::SelfRef(c) | Type::StaticRef(c) => c.clone(),
        Type::This => env.enclosing_class().map(|c| c.to_string()).unwrap_or_default(),
        Type::Nullable(inner) => return lookup_return_type(project, interner, inner, method, env),
        Type::Interface(c) | Type::Mock(c) => c.clone(),
        _ => return Type::Mixed,
    };
    if fqcn.is_empty() {
        return Type::Mixed;
    }

    // O(1) index lookup (case-insensitive) — replaces an O(n) scan over all
    // class reflections that re-lowercased every FQCN on each call site.
    let class_refl: &ClassLikeReflection = match project.find_class_reflection(&fqcn) {
        Some(r) => r,
        None => return Type::Mixed,
    };

    // Methods are keyed by interned lowercase string in mago-reflection
    // (mago normalises method names to lowercase for case-insensitive PHP).
    let method_lc = method.to_lowercase();
    for (id, m_refl) in class_refl.methods.members.iter() {
        let name = interner.lookup(id);
        if name == method_lc {
            let declared = reflect_return_type(project, interner, m_refl, env);
            // When the declared return is an interface, inspect the method body
            // for a more concrete type (e.g. `return new Foo()` or a factory
            // call whose declared return is a concrete class).
            if matches!(declared, Type::Interface(_)) {
                if let Some(narrowed) =
                    narrow_return_type_from_body(project, interner, &fqcn, &method_lc, m_refl, env)
                {
                    return narrowed;
                }
            }
            return declared;
        }
    }
    Type::Mixed
}

/// Convert a method's declared return type into our `Type`.
fn reflect_return_type(
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    method_refl: &FunctionLikeReflection,
    env: &TypeEnv,
) -> Type {
    // `return_type_reflection: Option<FunctionLikeReturnTypeReflection>`
    // `FunctionLikeReturnTypeReflection.type_reflection.kind: TypeKind`
    let Some(rt) = method_refl.return_type_reflection.as_ref() else {
        return Type::Mixed;
    };
    type_kind_to_type(project, interner, &rt.type_reflection.kind, env)
}

// ── Return-type narrowing from method body ────────────────────────────────────

/// When a method's declared return is an interface, parse its body and inspect
/// every top-level `return` statement. If every return path yields the same
/// concrete class — via `new Foo(...)` or `Bar::factory()` whose declared return
/// is already concrete — narrow to that class.
///
/// One level of body inspection only: return expressions are evaluated with
/// `reflect_return_type` (declared types, no further body recursion), which
/// prevents cycles while still piercing one layer of factory indirection.
fn narrow_return_type_from_body(
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    class_fqcn: &str,
    method_lc: &str,
    method_refl: &FunctionLikeReflection,
    env: &TypeEnv,
) -> Option<Type> {
    let src = project.source_by_id(method_refl.span.start.source)?;
    let program = project.get_or_parse(src);
    let names = mago_names::resolver::NameResolver::new(interner).resolve(&program);

    let simple = class_fqcn.rsplit('\\').next().unwrap_or(class_fqcn);
    let block = find_method_block_in_stmts(
        program.statements.iter(),
        simple,
        method_lc,
        interner,
    )?;

    let return_types: Vec<Type> = block
        .statements
        .iter()
        .filter_map(|stmt| {
            if let Statement::Return(ret) = stmt {
                ret.value
                    .as_ref()
                    .map(|expr| narrow_expr_type(expr, project, interner, &names, class_fqcn, env))
            } else {
                None
            }
        })
        .collect();

    if return_types.is_empty() {
        return None;
    }
    let first = &return_types[0];
    if matches!(first, Type::Class(_)) && return_types.iter().all(|t| t == first) {
        Some(first.clone())
    } else {
        None
    }
}

/// Walk `stmts` to find a concrete method body block by class simple-name and
/// method name. Descends into namespace wrappers.
fn find_method_block_in_stmts<'a>(
    stmts: impl Iterator<Item = &'a Statement>,
    class_simple: &str,
    method_lc: &str,
    interner: &ThreadedInterner,
) -> Option<&'a Block> {
    use mago_syntax::ast::class_like::member::ClassLikeMember;
    use mago_syntax::ast::class_like::method::MethodBody;
    use mago_syntax::ast::namespace::NamespaceBody;

    for stmt in stmts {
        match stmt {
            Statement::Class(c) => {
                if interner.lookup(&c.name.value).eq_ignore_ascii_case(class_simple) {
                    for member in c.members.iter() {
                        if let ClassLikeMember::Method(m) = member {
                            if interner.lookup(&m.name.value).to_lowercase() == method_lc {
                                if let MethodBody::Concrete(block) = &m.body {
                                    return Some(block);
                                }
                            }
                        }
                    }
                    return None;
                }
            }
            Statement::Trait(t) => {
                if interner.lookup(&t.name.value).eq_ignore_ascii_case(class_simple) {
                    for member in t.members.iter() {
                        if let ClassLikeMember::Method(m) = member {
                            if interner.lookup(&m.name.value).to_lowercase() == method_lc {
                                if let MethodBody::Concrete(block) = &m.body {
                                    return Some(block);
                                }
                            }
                        }
                    }
                    return None;
                }
            }
            Statement::Namespace(ns) => {
                let found = match &ns.body {
                    NamespaceBody::Implicit(b) => find_method_block_in_stmts(
                        b.statements.iter(),
                        class_simple,
                        method_lc,
                        interner,
                    ),
                    NamespaceBody::BraceDelimited(b) => find_method_block_in_stmts(
                        b.statements.iter(),
                        class_simple,
                        method_lc,
                        interner,
                    ),
                };
                if found.is_some() {
                    return found;
                }
            }
            _ => {}
        }
    }
    None
}

/// Evaluate the concrete type of a return expression without recursing into
/// further method bodies. Handles:
/// - `new ClassName(...)` → `Type::Class(fqcn)`
/// - `StaticClass::method(...)` → declared return of that method
/// - `(expr)` → recurse on inner
/// - everything else → `Type::Mixed`
fn narrow_expr_type(
    expr: &Expression,
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    names: &mago_names::ResolvedNames,
    class_fqcn: &str,
    env: &TypeEnv,
) -> Type {
    match expr {
        Expression::Instantiation(inst) => {
            let class_name = match inst.class.as_ref() {
                Expression::Identifier(id) => narrow_resolve_fqcn(interner, names, id),
                Expression::Self_(_) => class_fqcn.to_string(),
                _ => return Type::Mixed,
            };
            if class_name.is_empty() {
                Type::Mixed
            } else {
                Type::Class(class_name)
            }
        }
        Expression::Call(Call::StaticMethod(smc)) => {
            let class_name = match smc.class.as_ref() {
                Expression::Identifier(id) => narrow_resolve_fqcn(interner, names, id),
                Expression::Self_(_) | Expression::Static(_) => class_fqcn.to_string(),
                _ => return Type::Mixed,
            };
            let method_name = match selector_name(interner, &smc.method) {
                Some(n) => n,
                None => return Type::Mixed,
            };
            let method_lc = method_name.to_lowercase();
            let Some(class_refl) = project.find_class_reflection(&class_name) else {
                return Type::Mixed;
            };
            for (id, m_refl) in class_refl.methods.members.iter() {
                if interner.lookup(id) == method_lc {
                    return reflect_return_type(project, interner, m_refl, env);
                }
            }
            Type::Mixed
        }
        Expression::Parenthesized(p) => {
            narrow_expr_type(&p.expression, project, interner, names, class_fqcn, env)
        }
        _ => Type::Mixed,
    }
}

fn narrow_resolve_fqcn(
    interner: &ThreadedInterner,
    names: &mago_names::ResolvedNames,
    id: &mago_syntax::ast::Identifier,
) -> String {
    use mago_span::HasSpan;
    let fqcn = if names.contains(&id.span().start) {
        interner.lookup(names.get(id)).to_string()
    } else {
        interner.lookup(id.value()).to_string()
    };
    fqcn.trim_start_matches('\\').to_string()
}

/// Convert a `TypeKind` from mago-reflection into our `Type` enum.
///
/// Coverage:
///   - `Object(NamedObject { name })` → `Type::Class(name)`
///   - `Object(Self_ { scope })` → `Type::SelfRef(scope)` (resolved via env if needed)
///   - `Object(Static { scope })` → `Type::StaticRef(scope)`
///   - `Union { kinds }` where exactly one non-null kind exists → `Type::Nullable(inner)`
///   - `Union { kinds }` of two non-null → `Type::Union(a, b)` (first two)
///   - Everything else (Void, Never, Mixed, Scalar, Array, etc.) → `Type::Mixed`
fn type_kind_to_type(
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    kind: &TypeKind,
    env: &TypeEnv,
) -> Type {
    match kind {
        TypeKind::Object(obj) => match obj {
            ObjectTypeKind::NamedObject { name, .. } => {
                let name_str = interner.lookup(name).to_string();
                let is_iface = project
                    .find_class_reflection(&name_str)
                    .map(|r| r.is_interface())
                    .unwrap_or(false);
                if is_iface { Type::Interface(name_str) } else { Type::Class(name_str) }
            }
            ObjectTypeKind::Self_ { scope } => {
                Type::SelfRef(interner.lookup(scope).to_string())
            }
            ObjectTypeKind::Static { scope } => {
                Type::StaticRef(interner.lookup(scope).to_string())
            }
            // AnyObject, TypedObject, AnonymousObject, EnumCase, Generator, Parent → Mixed
            _ => Type::Mixed,
        },

        TypeKind::Union { kinds } => {
            // Check for nullable pattern: Union of [T, null] or [null, T].
            let null_count = kinds.iter().filter(|k| matches!(k, TypeKind::Value(mago_reflection::r#type::kind::ValueTypeKind::Null))).count();
            let non_nulls: Vec<&TypeKind> = kinds
                .iter()
                .filter(|k| !matches!(k, TypeKind::Value(mago_reflection::r#type::kind::ValueTypeKind::Null)))
                .collect();

            if null_count > 0 && non_nulls.len() == 1 {
                // Nullable<T>
                let inner = type_kind_to_type(project, interner, non_nulls[0], env);
                Type::Nullable(Box::new(inner))
            } else if non_nulls.len() == 2 && null_count == 0 {
                // Binary union T|U
                let a = type_kind_to_type(project, interner, non_nulls[0], env);
                let b = type_kind_to_type(project, interner, non_nulls[1], env);
                Type::Union(Box::new(a), Box::new(b))
            } else if non_nulls.len() == 1 {
                // Union with single non-null element (degenerate) → unwrap.
                type_kind_to_type(project, interner, non_nulls[0], env)
            } else {
                // 3+ non-null types or other complex unions → Mixed for Phase 2.
                Type::Mixed
            }
        }

        // Intersection type (A&B): pick the first named concrete class.
        // The pattern `ConcreteClass&MockObject` is common in PHPUnit tests;
        // using the concrete class lets the tracer follow real implementations.
        TypeKind::Intersection { kinds } => {
            for k in kinds.iter() {
                if let TypeKind::Object(ObjectTypeKind::NamedObject { name, .. }) = k {
                    return Type::Class(interner.lookup(name).to_string());
                }
            }
            Type::Mixed
        }

        // All other TypeKind variants (Void, Never, Mixed, Scalar, Array, Callable,
        // Value, Conditional, KeyOf, ValueOf, etc.) → Mixed in Phase 2.
        _ => Type::Mixed,
    }
}

/// Public(crate) wrapper around `type_kind_to_type` for callers outside this module
/// (e.g., `analyzer::trace::seed_param_types`).
pub(crate) fn type_kind_to_type_pub(
    project: &crate::mago_bridge::MagoProject,
    interner: &ThreadedInterner,
    kind: &TypeKind,
    env: &TypeEnv,
) -> Type {
    type_kind_to_type(project, interner, kind, env)
}

// ── Span / line helper ────────────────────────────────────────────────────────

/// Return the 1-based source line number for a span's start position.
///
/// Falls back to 0 when the source cannot be found (e.g., built-in stubs).
fn line_of_span(ctx: &WalkerCtx, span: mago_span::Span) -> u32 {
    if let Some(source) = ctx.project.source_by_id(span.start.source) {
        // Source::line_number returns a 0-based index; add 1 for 1-based.
        source.line_number(span.start.offset) as u32 + 1
    } else {
        0
    }
}

// ── shared helpers ────────────────────────────────────────────────────────────

/// Resolve a class-name identifier to its fully-qualified class name (FQCN).
///
/// Looks up the identifier's byte-offset position in `ctx.names` (the
/// `ResolvedNames` table produced by mago-names). Falls back to the raw
/// identifier value if no resolution exists at that position.
///
/// The returned FQCN has any leading backslash stripped, because
/// mago-project's reflection FQCNs don't carry one. Example: identifier
/// `Logger` in `namespace Monolog;` resolves to `"Monolog\\Logger"`, NOT
/// `"\\Monolog\\Logger"`.
fn resolve_class_fqcn(
    ctx: &WalkerCtx,
    id: &mago_syntax::ast::Identifier,
) -> String {
    use mago_span::HasSpan;
    let position = id.span().start;

    let fqcn = if ctx.names.contains(&position) {
        ctx.interner.lookup(ctx.names.get(id)).to_string()
    } else {
        ctx.interner.lookup(id.value()).to_string()
    };

    fqcn.trim_start_matches('\\').to_string()
}

/// Extract the variable name string (including `$` prefix) from a `Variable`.
///
/// Only `Variable::Direct` has a static name; indirect (`${...}`) and nested
/// (`$$foo`) are dynamic and return `None`.
fn var_name(interner: &ThreadedInterner, v: &Variable) -> Option<String> {
    match v {
        Variable::Direct(dv) => {
            // StringIdentifier lookup returns the raw PHP source text, which
            // already includes the `$` sigil (e.g., `"$x"`).
            Some(interner.lookup(&dv.name).to_string())
        }
        Variable::Indirect(_) | Variable::Nested(_) => None,
    }
}

// ── Statement-level ctx walker (Task 2.8) ────────────────────────────────────

/// Walk a statement using a full `WalkerCtx` (env + interner + project + events).
///
/// This ctx-based variant handles if-statements with branch env forking.
/// It coexists with the legacy `walk_statement(env, interner, stmt)` used by
/// the simple tests; the two paths are independent.
pub fn walk_statement_ctx(ctx: &mut WalkerCtx, stmt: &Statement) {
    if ctx.depth >= ctx.max_depth {
        return;
    }
    ctx.depth += 1;
    walk_statement_ctx_inner(ctx, stmt);
    ctx.depth -= 1;
}

fn walk_statement_ctx_inner(ctx: &mut WalkerCtx, stmt: &Statement) {
    match stmt {
        Statement::Expression(e) => { walk_expression(ctx, &e.expression); }
        Statement::If(if_stmt) => walk_if(ctx, if_stmt),
        Statement::Return(ret) => walk_return(ctx, ret),
        Statement::Block(b) => walk_block(ctx, b),
        // ── Loops / switch / match / try (M1) ──────────────────────────────
        //
        // Call sites inside these constructs must still emit CallSiteEvents so
        // the tracer recurses into callees reachable only through them. We are
        // flow-insensitive here: walk the loop/condition expressions and the
        // inner statement sequences, without env-forking (loop bodies may run
        // 0..n times, so narrowing them buys nothing). Nested statements go
        // back through `walk_statement_ctx` so the depth guard keeps applying.
        Statement::Foreach(f) => {
            walk_expression(ctx, &f.expression);
            if let Some(key) = f.target.key() {
                walk_expression(ctx, key);
            }
            walk_expression(ctx, f.target.value());
            for s in f.body.statements() {
                walk_statement_ctx(ctx, s);
            }
        }
        Statement::For(f) => {
            for e in f.initializations.iter() {
                walk_expression(ctx, e);
            }
            for e in f.conditions.iter() {
                walk_expression(ctx, e);
            }
            for e in f.increments.iter() {
                walk_expression(ctx, e);
            }
            for s in f.body.statements() {
                walk_statement_ctx(ctx, s);
            }
        }
        Statement::While(w) => {
            walk_expression(ctx, &w.condition);
            for s in w.body.statements() {
                walk_statement_ctx(ctx, s);
            }
        }
        Statement::DoWhile(d) => {
            walk_statement_ctx(ctx, &d.statement);
            walk_expression(ctx, &d.condition);
        }
        Statement::Switch(sw) => {
            walk_expression(ctx, &sw.expression);
            for case in sw.body.cases() {
                if let mago_syntax::ast::SwitchCase::Expression(c) = case {
                    walk_expression(ctx, &c.expression);
                }
                for s in case.statements() {
                    walk_statement_ctx(ctx, s);
                }
            }
        }
        Statement::Try(t) => {
            walk_block(ctx, &t.block);
            for catch in t.catch_clauses.iter() {
                walk_block(ctx, &catch.block);
            }
            if let Some(finally) = &t.finally_clause {
                walk_block(ctx, &finally.block);
            }
        }
        _ => {}
    }
}

fn walk_return(ctx: &mut WalkerCtx, ret: &Return) {
    if let Some(value) = &ret.value {
        walk_expression(ctx, value);
    }
}

/// Walk a `Block` (sequence of statements) using a `WalkerCtx`.
pub fn walk_block(ctx: &mut WalkerCtx, block: &Block) {
    for s in block.statements.iter() {
        walk_statement_ctx(ctx, s);
    }
}

/// Walk an if-statement, forking the env for each branch and restoring it after.
///
/// # Narrowing
///
/// For the true branch only: if the condition contains `$x instanceof Foo`,
/// `$x` is narrowed to `Foo` inside the branch body. After the branch the env
/// is restored to its pre-branch state. Negation (else branch) is **not**
/// modelled in Phase 2.
///
/// # IfBody shape (mago-syntax 0.26.1)
///
/// - `IfBody::Statement(IfStatementBody)` — `if (...) <statement>` or `if (...) { ... }`
///   - `statement: Box<Statement>`  (single stmt; may itself be a Block)
///   - `else_if_clauses: Sequence<IfStatementBodyElseIfClause>` each with `condition` + `statement`
///   - `else_clause: Option<IfStatementBodyElseClause>` with `statement`
/// - `IfBody::ColonDelimited(IfColonDelimitedBody)` — `if (...): ... endif;`
///   - `statements: Sequence<Statement>`
///   - `else_if_clauses: Sequence<IfColonDelimitedBodyElseIfClause>` each with `condition` + `statements`
///   - `else_clause: Option<IfColonDelimitedBodyElseClause>` with `statements`
fn walk_if(ctx: &mut WalkerCtx, if_stmt: &If) {
    // Walk condition, collecting any instanceof narrowings.
    ctx.pending_narrowings.clear();
    walk_expression(ctx, &if_stmt.condition);
    let narrowings: Vec<crate::types::narrowing::Narrowing> =
        std::mem::take(&mut ctx.pending_narrowings);

    // True branch: save env, apply narrowings, walk body, restore env.
    {
        let saved = ctx.env.clone();
        let tuples: Vec<(String, Type)> = narrowings.iter()
            .map(|n| (n.var.clone(), n.ty.clone()))
            .collect();
        ctx.env.apply_narrowing(&tuples);
        walk_if_body_true(ctx, &if_stmt.body);
        ctx.env = saved;
    }

    // Elseif chain: each clause gets its own condition narrowing + body.
    match &if_stmt.body {
        IfBody::Statement(sb) => {
            for elseif in sb.else_if_clauses.iter() {
                ctx.pending_narrowings.clear();
                walk_expression(ctx, &elseif.condition);
                let ei_narrowings: Vec<crate::types::narrowing::Narrowing> =
                    std::mem::take(&mut ctx.pending_narrowings);
                let saved = ctx.env.clone();
                let tuples: Vec<(String, Type)> = ei_narrowings.iter()
                    .map(|n| (n.var.clone(), n.ty.clone()))
                    .collect();
                ctx.env.apply_narrowing(&tuples);
                walk_statement_ctx(ctx, &elseif.statement);
                ctx.env = saved;
            }
            // Else branch: no narrowing applied (negation not modelled in Phase 2).
            if let Some(else_clause) = &sb.else_clause {
                let saved = ctx.env.clone();
                walk_statement_ctx(ctx, &else_clause.statement);
                ctx.env = saved;
            }
        }
        IfBody::ColonDelimited(cb) => {
            for elseif in cb.else_if_clauses.iter() {
                ctx.pending_narrowings.clear();
                walk_expression(ctx, &elseif.condition);
                let ei_narrowings: Vec<crate::types::narrowing::Narrowing> =
                    std::mem::take(&mut ctx.pending_narrowings);
                let saved = ctx.env.clone();
                let tuples: Vec<(String, Type)> = ei_narrowings.iter()
                    .map(|n| (n.var.clone(), n.ty.clone()))
                    .collect();
                ctx.env.apply_narrowing(&tuples);
                for s in elseif.statements.iter() {
                    walk_statement_ctx(ctx, s);
                }
                ctx.env = saved;
            }
            if let Some(else_clause) = &cb.else_clause {
                let saved = ctx.env.clone();
                for s in else_clause.statements.iter() {
                    walk_statement_ctx(ctx, s);
                }
                ctx.env = saved;
            }
        }
    }
}

/// Walk the true-branch body of an if-statement (one statement or a sequence).
fn walk_if_body_true(ctx: &mut WalkerCtx, body: &IfBody) {
    match body {
        IfBody::Statement(sb) => walk_statement_ctx(ctx, &sb.statement),
        IfBody::ColonDelimited(cb) => {
            for s in cb.statements.iter() {
                walk_statement_ctx(ctx, s);
            }
        }
    }
}

// (HasSpan already imported at top of file)

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mago_bridge::MagoProject;
    use mago_syntax::ast::class_like::member::ClassLikeMember;
    use mago_syntax::ast::class_like::method::MethodBody;
    use mago_syntax::ast::Statement;

    /// Parse the first class's first method body from a PHP snippet.
    /// Returns `(Program, enclosing_class_name)` so callers can walk it.
    pub fn find_first_method_body(
        project: &MagoProject,
    ) -> (mago_syntax::ast::Program, String) {
        let module = project.inner().modules.first().expect("at least one module");
        let program = module.parse(project.interner());
        // Find the first class statement.
        let class = program.statements.iter().find_map(|s| {
            if let Statement::Class(c) = s { Some(c) } else { None }
        }).expect("a class statement");
        // The class name from the interner.
        let class_name = project.interner().lookup(&class.name.value).to_string();
        (program, class_name)
    }

    /// Build a PHP snippet whose single method nests `n` `new A(...)` calls,
    /// e.g. `new A(new A(...))`. Each instantiation emits one `__construct`
    /// CallSiteEvent, so the event count equals the reached nesting depth.
    fn nested_new(n: usize) -> String {
        let mut s = String::from("<?php\nclass A {\n  public function go(): void {\n    $x = ");
        for _ in 0..n { s.push_str("new A("); }
        for _ in 0..n { s.push(')'); }
        s.push_str(";\n  }\n}\n");
        s
    }

    /// Walk the first concrete method of the first class with a custom
    /// `max_depth`, returning how many CallSiteEvents were emitted.
    fn count_events_with_max_depth(php: &str, max_depth: u32) -> usize {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Test.php"), php).unwrap();
        let project = MagoProject::load(dir.path()).expect("load ok");
        let interner = project.interner();
        let module = project.inner().modules.first().expect("module");
        let program = module.parse(interner);
        let class = program.statements.iter().find_map(|s| {
            if let Statement::Class(c) = s { Some(c) } else { None }
        }).expect("class");
        let class_name = interner.lookup(&class.name.value).to_string();
        let method = class.members.iter().find_map(|m| {
            if let ClassLikeMember::Method(m) = m { Some(m) } else { None }
        }).expect("method");
        let block = match &method.body {
            MethodBody::Concrete(b) => b,
            _ => panic!("expected concrete body"),
        };
        let env = TypeEnv::for_class(&class_name);
        let names = mago_names::resolver::NameResolver::new(interner).resolve(&program);
        let mut ctx = WalkerCtx::new(env, interner, &project, names);
        ctx.max_depth = max_depth;
        for stmt in block.statements.iter() {
            walk_statement_ctx(&mut ctx, stmt);
        }
        ctx.events.len()
    }

    /// H4: the walker must bound recursion depth on untrusted PHP. With a
    /// small `max_depth`, walking deeply-nested expressions must stop early
    /// (truncating events) rather than recursing without bound (stack overflow).
    /// (n=20 parses cleanly in mago; this isolates the *walker* guard.)
    #[test]
    fn depth_guard_truncates_deeply_nested_expressions() {
        let php = nested_new(20);
        let events = count_events_with_max_depth(&php, 8);
        assert!(
            events >= 1 && events <= 8,
            "expected the depth guard to truncate walking at max_depth=8 \
             (1..=8 events), but got {events} events for 20 nested instantiations"
        );
    }

    /// H4: the guard must not perturb normal-depth code — at the default
    /// max_depth, all 20 nested instantiations are walked and emit events.
    #[test]
    fn depth_guard_does_not_affect_normal_depth() {
        let php = nested_new(20);
        let events = count_events_with_max_depth(&php, 512);
        assert_eq!(events, 20, "default-depth walking must emit one event per instantiation");
    }

    /// H4 (parser stage): mago's recursive-descent parser overflows a default
    /// 2 MB stack inside `MagoProject::load` at ~30–40 levels of nesting.
    /// `with_deep_stack` must let such input load without aborting the process.
    #[test]
    fn deep_nesting_loads_on_big_stack() {
        let php = nested_new(60);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Test.php"), &php).unwrap();
        let ok = crate::cli::analyze::with_deep_stack(|| MagoProject::load(dir.path()).is_ok());
        assert!(ok, "deeply-nested PHP must load on the deep stack without overflowing");
    }

    /// Walk the first method body of the first class, return the type of `$var_name`.
    fn walk_first_method_var(php: &str, var_name: &str) -> Type {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Test.php"), php).unwrap();
        let project = MagoProject::load(dir.path()).expect("load ok");

        let (program, class_name) = find_first_method_body(&project);

        // Find the class node again from the program.
        let class = program.statements.iter().find_map(|s| {
            if let Statement::Class(c) = s { Some(c) } else { None }
        }).expect("class");

        // Find first concrete method.
        let method = class.members.iter().find_map(|m| {
            if let ClassLikeMember::Method(m) = m { Some(m) } else { None }
        }).expect("method");

        let block = match &method.body {
            MethodBody::Concrete(b) => b,
            MethodBody::Abstract(_) => panic!("expected concrete method"),
        };

        let mut env = TypeEnv::for_class(&class_name);
        for stmt in block.statements.iter() {
            walk_statement(&mut env, project.interner(), stmt);
        }
        env.lookup(var_name)
    }

    #[test]
    fn assignment_of_new_yields_class_type() {
        let ty = walk_first_method_var(
            r#"<?php
class A {
    public function test(): void {
        $x = new A();
    }
}
"#,
            "$x",
        );
        assert_eq!(ty, Type::Class("A".into()));
    }

    #[test]
    fn assignment_of_literal_yields_mixed() {
        let ty = walk_first_method_var(
            r#"<?php
class B {
    public function test(): void {
        $x = 42;
    }
}
"#,
            "$x",
        );
        assert_eq!(ty, Type::Mixed);
    }

    #[test]
    fn unknown_var_yields_mixed() {
        let ty = walk_first_method_var(
            r#"<?php
class C {
    public function test(): void {
        $y = 1;
    }
}
"#,
            "$z",
        );
        assert_eq!(ty, Type::Mixed);
    }

    #[test]
    fn new_self_resolves_to_selfref_of_enclosing_class() {
        let ty = walk_first_method_var(
            r#"<?php
class D {
    public function test(): void {
        $x = new self();
    }
}
"#,
            "$x",
        );
        assert_eq!(ty, Type::SelfRef("D".into()));
    }

    // ── Task 2.5 tests ─────────────────────────────────────────────────────────

    #[test]
    fn method_call_emits_call_site_event() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.php"), r#"<?php
class A {
    public function caller(): void {
        $b = new B();
        $b->doIt();
    }
}
class B {
    public function doIt(): void {}
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let interner = project.interner().clone();

        // Find class A and its `caller` method body.
        let module = project.inner().modules.first().expect("module");
        let program = module.parse(&interner);

        let class_a = program.statements.iter().find_map(|s| {
            if let Statement::Class(c) = s {
                let name = interner.lookup(&c.name.value).to_string();
                if name.to_lowercase() == "a" { Some(c) } else { None }
            } else {
                None
            }
        }).expect("class A");

        let caller_method = class_a.members.iter().find_map(|m| {
            if let ClassLikeMember::Method(m) = m {
                let name = interner.lookup(&m.name.value).to_string();
                if name.to_lowercase() == "caller" { Some(m) } else { None }
            } else {
                None
            }
        }).expect("caller method");

        let block = match &caller_method.body {
            MethodBody::Concrete(b) => b,
            _ => panic!("expected concrete body"),
        };

        let env = TypeEnv::for_class("A");
        let names = mago_names::resolver::NameResolver::new(&interner).resolve(&program);
        let mut ctx = WalkerCtx::new(env, &interner, &project, names);

        for stmt in block.statements.iter() {
            if let Statement::Expression(e) = stmt {
                walk_expression(&mut ctx, &e.expression);
            }
        }

        // Also walk assignment statements ($b = new B()).
        // Re-walk all statements properly.
        let env2 = TypeEnv::for_class("A");
        let names2 = mago_names::resolver::NameResolver::new(&interner).resolve(&program);
        let mut ctx2 = WalkerCtx::new(env2, &interner, &project, names2);
        for stmt in block.statements.iter() {
            match stmt {
                Statement::Expression(e) => {
                    walk_expression(&mut ctx2, &e.expression);
                }
                _ => {}
            }
        }

        let do_it_events: Vec<&CallSiteEvent> = ctx2.events.iter()
            .filter(|e| e.method_name == "doIt")
            .collect();

        assert_eq!(
            do_it_events.len(),
            1,
            "expected 1 doIt call site event; got {} — all events: {:?}",
            do_it_events.len(),
            ctx2.events
        );

        let ev = do_it_events[0];
        assert_eq!(
            ev.receiver,
            Type::Class("B".into()),
            "expected receiver to be Class(B); got {:?}",
            ev.receiver
        );
        assert!(
            ev.callee_class.as_deref().map(|s| s.to_lowercase()) == Some("b".to_string()),
            "expected callee_class to be B; got {:?}",
            ev.callee_class
        );
    }

    #[test]
    fn method_call_return_type_resolved_from_reflection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Chain.php"), r#"<?php
class Builder {
    public function build(): Result {}
}
class Result {}
class Client {
    public function run(): void {
        $builder = new Builder();
        $result = $builder->build();
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let interner = project.interner().clone();

        let module = project.inner().modules.first().expect("module");
        let program = module.parse(&interner);

        let client = program.statements.iter().find_map(|s| {
            if let Statement::Class(c) = s {
                let name = interner.lookup(&c.name.value).to_string();
                if name.to_lowercase() == "client" { Some(c) } else { None }
            } else {
                None
            }
        }).expect("Client class");

        let run_method = client.members.iter().find_map(|m| {
            if let ClassLikeMember::Method(m) = m {
                let name = interner.lookup(&m.name.value).to_string();
                if name.to_lowercase() == "run" { Some(m) } else { None }
            } else {
                None
            }
        }).expect("run method");

        let block = match &run_method.body {
            MethodBody::Concrete(b) => b,
            _ => panic!("expected concrete body"),
        };

        let env = TypeEnv::for_class("Client");
        let names = mago_names::resolver::NameResolver::new(&interner).resolve(&program);
        let mut ctx = WalkerCtx::new(env, &interner, &project, names);

        for stmt in block.statements.iter() {
            match stmt {
                Statement::Expression(e) => {
                    walk_expression(&mut ctx, &e.expression);
                }
                _ => {}
            }
        }

        // $result should have been assigned the return type of Builder::build() = Class("Result")
        let result_type = ctx.env.lookup("$result");
        assert_eq!(
            result_type,
            Type::Class("Result".into()),
            "expected $result to be Class(Result) from reflection; got {:?}",
            result_type
        );
    }

    // ── Task 2.6 tests ─────────────────────────────────────────────────────────

    /// Walk a named method body and return the env.
    fn walk_method_body(project: &MagoProject, class_lc: &str, method_lc: &str) -> TypeEnv {
        let interner = project.interner();
        let module = project.inner().modules.first().expect("module");
        let program = module.parse(interner);

        // Find the target class.
        let class = program.statements.iter().find_map(|s| {
            if let Statement::Class(c) = s {
                let n = interner.lookup(&c.name.value).to_lowercase();
                if n == class_lc { Some(c) } else { None }
            } else {
                None
            }
        }).unwrap_or_else(|| panic!("class {class_lc} not found"));

        let class_name = interner.lookup(&class.name.value).to_string();

        // Find the target method.
        let method = class.members.iter().find_map(|m| {
            if let ClassLikeMember::Method(m) = m {
                let n = interner.lookup(&m.name.value).to_lowercase();
                if n == method_lc { Some(m) } else { None }
            } else {
                None
            }
        }).unwrap_or_else(|| panic!("method {method_lc} not found"));

        let block = match &method.body {
            MethodBody::Concrete(b) => b,
            _ => panic!("expected concrete body"),
        };

        let env = TypeEnv::for_class(&class_name);
        let names = mago_names::resolver::NameResolver::new(interner).resolve(&program);
        let mut ctx = WalkerCtx::new(env, interner, project, names);

        for stmt in block.statements.iter() {
            match stmt {
                Statement::Expression(e) => { walk_expression(&mut ctx, &e.expression); }
                _ => {}
            }
        }

        ctx.env
    }

    #[test]
    fn this_property_resolves_to_declared_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Service.php"), r#"<?php
class Repo {}
class Service {
    public Repo $repo;
    public function go(): void {
        $r = $this->repo;
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();

        let env = walk_method_body(&project, "service", "go");
        let ty = env.lookup("$r");

        assert_eq!(
            ty,
            Type::Class("Repo".into()),
            "expected $r to be Class(Repo) via $this->repo property type; got {:?}",
            ty
        );
    }

    #[test]
    fn this_promoted_property_resolves_to_declared_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Service2.php"), r#"<?php
class Repo {}
class Service2 {
    public function __construct(private Repo $repo) {}
    public function go(): void {
        $r = $this->repo;
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();

        let env = walk_method_body(&project, "service2", "go");
        let ty = env.lookup("$r");

        assert_eq!(
            ty,
            Type::Class("Repo".into()),
            "expected $r to be Class(Repo) via constructor-promoted $this->repo; got {:?}",
            ty
        );
    }

    #[test]
    fn null_safe_property_access_resolves_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("NullSafe.php"), r#"<?php
class Inner {}
class Outer {
    public Inner $inner;
    public function go(): void {
        $i = $this?->inner;
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();

        let env = walk_method_body(&project, "outer", "go");
        let ty = env.lookup("$i");

        assert_eq!(
            ty,
            Type::Class("Inner".into()),
            "expected $i to be Class(Inner) via ?-> property access; got {:?}",
            ty
        );
    }

    #[test]
    fn inherited_property_resolves_via_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Inherit.php"), r#"<?php
class Dep {}
class Base {
    public Dep $dep;
}
class Child extends Base {
    public function go(): void {
        $d = $this->dep;
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();

        let env = walk_method_body(&project, "child", "go");
        let ty = env.lookup("$d");

        assert_eq!(
            ty,
            Type::Class("Dep".into()),
            "expected $d to be Class(Dep) via inherited property; got {:?}",
            ty
        );
    }

    // ── Task 2.7 tests ─────────────────────────────────────────────────────────

    #[test]
    fn method_chain_resolves_through_declared_return_types() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Chain.php"), r#"<?php
class C {
    public function returnsB(): B { return new B(); }
}
class B {
    public function returnsA(): A { return new A(); }
}
class A {
    public function done(): void {}
}
class Caller {
    public function go(): void {
        $c = new C();
        $c->returnsB()->returnsA()->done();
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let interner = project.interner().clone();
        let env = TypeEnv::for_class("Caller");

        let caller_source = project.class_likes()
            .find(|(n, _)| project.class_name_str(n).to_lowercase() == "caller")
            .map(|(_, r)| r.span.start.source).unwrap();
        let module = project.inner().modules.iter()
            .find(|m| m.source.identifier == caller_source).unwrap();
        let program = module.parse(&interner);
        let names = mago_names::resolver::NameResolver::new(&interner).resolve(&program);

        let mut ctx = WalkerCtx::new(env, &interner, &project, names);

        // Walk Caller::go body
        let mut found = false;
        for stmt in program.statements.iter() {
            if let Statement::Class(c) = stmt {
                if interner.lookup(&c.name.value).to_lowercase() != "caller" { continue; }
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if interner.lookup(&m.name.value).to_lowercase() != "go" { continue; }
                        if let MethodBody::Concrete(block) = &m.body {
                            for s in block.statements.iter() {
                                if let Statement::Expression(e) = s {
                                    walk_expression(&mut ctx, &e.expression);
                                }
                            }
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "didn't reach Caller::go body");

        // We expect FOUR call site events: __construct (for new C()), returnsB(), returnsA(), done().
        // Their receivers should be C → C → B → A.
        let method_names: Vec<&str> = ctx.events.iter().map(|e| e.method_name.as_str()).collect();
        assert_eq!(method_names, vec!["__construct", "returnsB", "returnsA", "done"],
            "expected events for __construct, returnsB, returnsA, done in order; got: {method_names:?}");

        assert_eq!(ctx.events[0].receiver, Type::Class("C".into()),
            "__construct receiver should be C (instantiation); got {:?}", ctx.events[0].receiver);
        assert_eq!(ctx.events[1].receiver, Type::Class("C".into()),
            "returnsB receiver should be C; got {:?}", ctx.events[1].receiver);
        assert_eq!(ctx.events[2].receiver, Type::Class("B".into()),
            "returnsA receiver should be B (from returnsB's declared return type); got {:?}", ctx.events[2].receiver);
        assert_eq!(ctx.events[3].receiver, Type::Class("A".into()),
            "done receiver should be A (from returnsA's declared return type); got {:?}", ctx.events[3].receiver);
    }

    // ── Task 2.8 tests ─────────────────────────────────────────────────────────

    #[test]
    fn instanceof_narrows_inside_if_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Narrow.php"), r#"<?php
class Base { public function baseMethod(): void {} }
class Dog extends Base { public function bark(): void {} }
class Caller {
    public function go(Base $animal): void {
        if ($animal instanceof Dog) {
            $animal->bark();
        }
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let interner = project.interner().clone();
        let env = TypeEnv::for_class("Caller");

        let caller_source = project.class_likes()
            .find(|(n, _)| project.class_name_str(n).to_lowercase() == "caller")
            .map(|(_, r)| r.span.start.source).unwrap();
        let module = project.inner().modules.iter()
            .find(|m| m.source.identifier == caller_source).unwrap();
        let program = module.parse(&interner);
        let names = mago_names::resolver::NameResolver::new(&interner).resolve(&program);

        let mut ctx = WalkerCtx::new(env, &interner, &project, names);
        // Seed $animal as Base (its declared param type).
        ctx.env.set("$animal".into(), Type::Class("Base".into()));

        // Find Caller class and its go() method.
        for stmt in program.statements.iter() {
            if let Statement::Class(c) = stmt {
                if interner.lookup(&c.name.value).to_lowercase() != "caller" { continue; }
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if interner.lookup(&m.name.value).to_lowercase() != "go" { continue; }
                        if let MethodBody::Concrete(block) = &m.body {
                            walk_block(&mut ctx, block);
                        }
                    }
                }
            }
        }

        // The bark() call should have been emitted with receiver = Class(Dog).
        let bark_event = ctx.events.iter().find(|e| e.method_name == "bark");
        assert!(bark_event.is_some(),
            "expected bark() call site emitted; got events: {:?}", ctx.events);
        assert_eq!(bark_event.unwrap().receiver, Type::Class("Dog".into()),
            "bark() receiver should be narrowed Dog (not Base)");

        // After the if-body, $animal should be Base again.
        assert_eq!(ctx.env.lookup("$animal"), Type::Class("Base".into()),
            "$animal should restore to Base after the if-body");
    }

    #[test]
    fn narrowing_set_push_and_extend() {
        use crate::types::narrowing::NarrowingSet;
        let mut a = NarrowingSet::new();
        a.push("$x".into(), Type::Class("Foo".into()));
        let mut b = NarrowingSet::new();
        b.push("$y".into(), Type::Class("Bar".into()));
        a.extend(b);
        assert_eq!(a.facts.len(), 2);
        assert_eq!(a.facts[0].var, "$x");
        assert_eq!(a.facts[1].var, "$y");
    }

    // ── Task 2.9 tests: Union types 2-way + narrowing ───────────────────────

    #[test]
    fn union_return_type_resolves_through_walker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("UR.php"), r#"<?php
class A {}
class B {}
class Factory {
    public function make(): A|B { return new A(); }
}
class Caller {
    public function go(): void {
        $f = new Factory();
        $x = $f->make();
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let interner = project.interner().clone();
        let env = TypeEnv::for_class("Caller");

        let caller_source = project.class_likes()
            .find(|(n, _)| project.class_name_str(n).to_lowercase() == "caller")
            .map(|(_, r)| r.span.start.source).unwrap();
        let module = project.inner().modules.iter()
            .find(|m| m.source.identifier == caller_source).unwrap();
        let program = module.parse(&interner);
        let names = mago_names::resolver::NameResolver::new(&interner).resolve(&program);

        let mut ctx = WalkerCtx::new(env, &interner, &project, names);

        for stmt in program.statements.iter() {
            if let Statement::Class(c) = stmt {
                if interner.lookup(&c.name.value).to_lowercase() != "caller" { continue; }
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if interner.lookup(&m.name.value).to_lowercase() != "go" { continue; }
                        if let MethodBody::Concrete(block) = &m.body {
                            for s in block.statements.iter() {
                                if let Statement::Expression(e) = s {
                                    walk_expression(&mut ctx, &e.expression);
                                }
                            }
                        }
                    }
                }
            }
        }

        // $x should now have Type::Union(Class(A), Class(B)) or Type::Union(Class(B), Class(A))
        let x_type = ctx.env.lookup("$x");
        match &x_type {
            Type::Union(left, right) => {
                // Verify both variants are the expected concrete classes.
                let names: std::collections::HashSet<String> = [left.as_ref(), right.as_ref()]
                    .iter()
                    .filter_map(|t| match t {
                        Type::Class(c) => Some(c.clone()),
                        _ => None,
                    })
                    .collect();
                assert!(
                    names.contains("A"),
                    "expected Class(A) in union; got: {x_type:?}"
                );
                assert!(
                    names.contains("B"),
                    "expected Class(B) in union; got: {x_type:?}"
                );
            }
            other => panic!("expected Type::Union, got: {other:?}"),
        }
    }

    #[test]
    fn nullable_return_type_resolves_through_walker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("NR.php"), r#"<?php
class Foo {}
class Builder {
    public function build(): ?Foo { return new Foo(); }
}
class Caller {
    public function go(): void {
        $b = new Builder();
        $result = $b->build();
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let interner = project.interner().clone();
        let env = TypeEnv::for_class("Caller");

        let caller_source = project.class_likes()
            .find(|(n, _)| project.class_name_str(n).to_lowercase() == "caller")
            .map(|(_, r)| r.span.start.source).unwrap();
        let module = project.inner().modules.iter()
            .find(|m| m.source.identifier == caller_source).unwrap();
        let program = module.parse(&interner);
        let names = mago_names::resolver::NameResolver::new(&interner).resolve(&program);

        let mut ctx = WalkerCtx::new(env, &interner, &project, names);

        for stmt in program.statements.iter() {
            if let Statement::Class(c) = stmt {
                if interner.lookup(&c.name.value).to_lowercase() != "caller" { continue; }
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if interner.lookup(&m.name.value).to_lowercase() != "go" { continue; }
                        if let MethodBody::Concrete(block) = &m.body {
                            for s in block.statements.iter() {
                                if let Statement::Expression(e) = s {
                                    walk_expression(&mut ctx, &e.expression);
                                }
                            }
                        }
                    }
                }
            }
        }

        // $result should have Type::Nullable(Box::new(Type::Class("Foo")))
        let result_type = ctx.env.lookup("$result");
        match &result_type {
            Type::Nullable(inner) => {
                match inner.as_ref() {
                    Type::Class(c) => {
                        assert_eq!(c, "Foo", "expected nullable Foo; got: {result_type:?}");
                    }
                    _other => panic!("expected Nullable(Class(Foo)), got: {result_type:?}"),
                }
            }
            other => panic!("expected Type::Nullable, got: {other:?}"),
        }
    }

    #[test]
    fn degenerate_union_unwraps_to_single_type() {
        // A union with only one actual non-null kind should unwrap to that kind.
        // This is a tricky edge case from type_kind_to_type's logic.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Degen.php"), r#"<?php
class OnlyOne {}
class Wrapper {
    public function get(): OnlyOne { return new OnlyOne(); }
}
class Caller {
    public function go(): void {
        $w = new Wrapper();
        $x = $w->get();
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let interner = project.interner().clone();
        let env = TypeEnv::for_class("Caller");

        let caller_source = project.class_likes()
            .find(|(n, _)| project.class_name_str(n).to_lowercase() == "caller")
            .map(|(_, r)| r.span.start.source).unwrap();
        let module = project.inner().modules.iter()
            .find(|m| m.source.identifier == caller_source).unwrap();
        let program = module.parse(&interner);
        let names = mago_names::resolver::NameResolver::new(&interner).resolve(&program);

        let mut ctx = WalkerCtx::new(env, &interner, &project, names);

        for stmt in program.statements.iter() {
            if let Statement::Class(c) = stmt {
                if interner.lookup(&c.name.value).to_lowercase() != "caller" { continue; }
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if interner.lookup(&m.name.value).to_lowercase() != "go" { continue; }
                        if let MethodBody::Concrete(block) = &m.body {
                            for s in block.statements.iter() {
                                if let Statement::Expression(e) = s {
                                    walk_expression(&mut ctx, &e.expression);
                                }
                            }
                        }
                    }
                }
            }
        }

        // $x should be Class("OnlyOne"), not Union or Mixed.
        let x_type = ctx.env.lookup("$x");
        assert_eq!(
            x_type,
            Type::Class("OnlyOne".into()),
            "expected Class(OnlyOne); got: {x_type:?}"
        );
    }

    #[test]
    fn three_way_union_becomes_mixed() {
        // Union with 3+ non-null kinds should produce Type::Mixed in Phase 2.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Three.php"), r#"<?php
class A {}
class B {}
class C {}
class Factory {
    public function make(): A|B|C { return new A(); }
}
class Caller {
    public function go(): void {
        $f = new Factory();
        $x = $f->make();
    }
}
"#).unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let interner = project.interner().clone();
        let env = TypeEnv::for_class("Caller");

        let caller_source = project.class_likes()
            .find(|(n, _)| project.class_name_str(n).to_lowercase() == "caller")
            .map(|(_, r)| r.span.start.source).unwrap();
        let module = project.inner().modules.iter()
            .find(|m| m.source.identifier == caller_source).unwrap();
        let program = module.parse(&interner);
        let names = mago_names::resolver::NameResolver::new(&interner).resolve(&program);

        let mut ctx = WalkerCtx::new(env, &interner, &project, names);

        for stmt in program.statements.iter() {
            if let Statement::Class(c) = stmt {
                if interner.lookup(&c.name.value).to_lowercase() != "caller" { continue; }
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if interner.lookup(&m.name.value).to_lowercase() != "go" { continue; }
                        if let MethodBody::Concrete(block) = &m.body {
                            for s in block.statements.iter() {
                                if let Statement::Expression(e) = s {
                                    walk_expression(&mut ctx, &e.expression);
                                }
                            }
                        }
                    }
                }
            }
        }

        // $x should be Type::Mixed (3+ non-null unions → Mixed in Phase 2).
        let x_type = ctx.env.lookup("$x");
        assert_eq!(
            x_type,
            Type::Mixed,
            "expected Type::Mixed for 3-way union; got: {x_type:?}"
        );
    }

    // ── Return-type narrowing from method body ────────────────────────────────

    /// Abstract-base pattern: `loadDriver(): DriverInterface` is declared with
    /// an interface return type, but the concrete override returns
    /// `new ConcreteDriver()`. `lookup_return_type` should narrow to
    /// `Type::Class("ConcreteDriver")` when called on the concrete class.
    #[test]
    fn narrows_interface_return_via_new_in_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Driver.php"),
            "<?php\ninterface DriverInterface { public function work(): void; }\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("ConcreteDriver.php"),
            "<?php\nclass ConcreteDriver implements DriverInterface { public function work(): void {} }\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("Base.php"),
            "<?php\nabstract class Base {\n  abstract protected function loadDriver(): DriverInterface;\n}\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("Concrete.php"),
            "<?php\nclass Concrete extends Base {\n  protected function loadDriver(): DriverInterface {\n    return new ConcreteDriver();\n  }\n}\n",
        ).unwrap();

        let project = MagoProject::load(dir.path()).expect("load ok");
        let interner = project.interner();
        let env = TypeEnv::for_class("Concrete");

        let result = lookup_return_type(
            &project,
            interner,
            &Type::Class("Concrete".to_string()),
            "loaddriver",
            &env,
        );

        assert_eq!(
            result,
            Type::Class("ConcreteDriver".to_string()),
            "expected narrowed Type::Class(ConcreteDriver), got: {result:?}"
        );
    }

    /// Factory variant: concrete override returns `Factory::create()` whose
    /// declared return is already a concrete class, not an interface.
    #[test]
    fn narrows_interface_return_via_static_factory_in_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("DriverInterface.php"),
            "<?php\ninterface DriverInterface {}\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("ConcreteDriver.php"),
            "<?php\nclass ConcreteDriver implements DriverInterface {}\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("Factory.php"),
            "<?php\nclass Factory {\n  public static function create(): ConcreteDriver { return new ConcreteDriver(); }\n}\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("Concrete.php"),
            "<?php\nclass Concrete {\n  protected function loadDriver(): DriverInterface {\n    return Factory::create();\n  }\n}\n",
        ).unwrap();

        let project = MagoProject::load(dir.path()).expect("load ok");
        let interner = project.interner();
        let env = TypeEnv::for_class("Concrete");

        let result = lookup_return_type(
            &project,
            interner,
            &Type::Class("Concrete".to_string()),
            "loaddriver",
            &env,
        );

        assert_eq!(
            result,
            Type::Class("ConcreteDriver".to_string()),
            "expected narrowed Type::Class(ConcreteDriver) via factory, got: {result:?}"
        );
    }

    /// Cross-namespace factory: concrete override uses a `use`-imported factory
    /// class from a different namespace (mirrors doctrine/orm AttributeDriverTest).
    #[test]
    fn narrows_interface_return_via_namespaced_factory() {
        let dir = tempfile::tempdir().unwrap();
        // DriverInterface in App\Contracts
        std::fs::write(
            dir.path().join("DriverInterface.php"),
            "<?php\nnamespace App\\Contracts;\ninterface DriverInterface {}\n",
        ).unwrap();
        // ConcreteDriver in App\Driver
        std::fs::write(
            dir.path().join("ConcreteDriver.php"),
            "<?php\nnamespace App\\Driver;\nuse App\\Contracts\\DriverInterface;\nclass ConcreteDriver implements DriverInterface {}\n",
        ).unwrap();
        // Factory in App\Support
        std::fs::write(
            dir.path().join("Factory.php"),
            "<?php\nnamespace App\\Support;\nuse App\\Driver\\ConcreteDriver;\nclass Factory {\n  public static function make(): ConcreteDriver { return new ConcreteDriver(); }\n}\n",
        ).unwrap();
        // Concrete class in App\Tests, importing Factory via use
        std::fs::write(
            dir.path().join("ConcreteTest.php"),
            "<?php\nnamespace App\\Tests;\nuse App\\Contracts\\DriverInterface;\nuse App\\Support\\Factory;\nclass ConcreteTest {\n  protected function loadDriver(): DriverInterface {\n    return Factory::make();\n  }\n}\n",
        ).unwrap();

        let project = MagoProject::load(dir.path()).expect("load ok");
        let interner = project.interner();
        let env = TypeEnv::for_class("App\\Tests\\ConcreteTest");

        let result = lookup_return_type(
            &project,
            interner,
            &Type::Class("App\\Tests\\ConcreteTest".to_string()),
            "loaddriver",
            &env,
        );

        assert_eq!(
            result,
            Type::Class("App\\Driver\\ConcreteDriver".to_string()),
            "expected narrowed FQCN via use-imported factory, got: {result:?}"
        );
    }

    // ── M1: coverage walker must recurse into loops/switch/try/match ────────────

    /// Walk the first concrete method of the named class with `walk_statement_ctx`
    /// (the ctx-based statement path), returning every emitted `CallSiteEvent`.
    fn collect_events_walking_method(php: &str, class_lc: &str, method_lc: &str) -> Vec<CallSiteEvent> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("M1.php"), php).unwrap();
        let project = MagoProject::load(dir.path()).expect("load ok");
        let interner = project.interner();
        let module = project.inner().modules.first().expect("module");
        let program = module.parse(interner);

        let class = program.statements.iter().find_map(|s| {
            if let Statement::Class(c) = s {
                let n = interner.lookup(&c.name.value).to_lowercase();
                if n == class_lc { Some(c) } else { None }
            } else {
                None
            }
        }).unwrap_or_else(|| panic!("class {class_lc} not found"));
        let class_name = interner.lookup(&class.name.value).to_string();

        let method = class.members.iter().find_map(|m| {
            if let ClassLikeMember::Method(m) = m {
                let n = interner.lookup(&m.name.value).to_lowercase();
                if n == method_lc { Some(m) } else { None }
            } else {
                None
            }
        }).unwrap_or_else(|| panic!("method {method_lc} not found"));

        let block = match &method.body {
            MethodBody::Concrete(b) => b,
            _ => panic!("expected concrete body"),
        };

        let env = TypeEnv::for_class(&class_name);
        let names = mago_names::resolver::NameResolver::new(interner).resolve(&program);
        let mut ctx = WalkerCtx::new(env, interner, &project, names);
        for stmt in block.statements.iter() {
            walk_statement_ctx(&mut ctx, stmt);
        }
        ctx.events
    }

    fn assert_helper_called(php: &str, helper: &str) {
        let events = collect_events_walking_method(php, "a", "go");
        assert!(
            events.iter().any(|e| e.method_name == helper),
            "expected a CallSiteEvent for `{helper}` reachable only through the \
             control-flow construct, but none was emitted; events: {events:?}"
        );
    }

    /// The only call to `helper` happens inside a `foreach` body. The walker
    /// must recurse into the loop body and emit the call-site event.
    #[test]
    fn foreach_body_call_emits_event() {
        assert_helper_called(
            r#"<?php
class A {
    public function go(array $items): void {
        foreach ($items as $i) {
            $this->helper();
        }
    }
    public function helper(): void {}
}
"#,
            "helper",
        );
    }

    #[test]
    fn while_body_call_emits_event() {
        assert_helper_called(
            r#"<?php
class A {
    public function go(): void {
        while (true) {
            $this->helper();
        }
    }
    public function helper(): void {}
}
"#,
            "helper",
        );
    }

    #[test]
    fn for_body_call_emits_event() {
        assert_helper_called(
            r#"<?php
class A {
    public function go(): void {
        for ($i = 0; $i < 10; $i++) {
            $this->helper();
        }
    }
    public function helper(): void {}
}
"#,
            "helper",
        );
    }

    #[test]
    fn do_while_body_call_emits_event() {
        assert_helper_called(
            r#"<?php
class A {
    public function go(): void {
        do {
            $this->helper();
        } while (false);
    }
    public function helper(): void {}
}
"#,
            "helper",
        );
    }

    #[test]
    fn switch_case_call_emits_event() {
        assert_helper_called(
            r#"<?php
class A {
    public function go(int $x): void {
        switch ($x) {
            case 1:
                $this->helper();
                break;
            default:
                $this->fallback();
        }
    }
    public function helper(): void {}
    public function fallback(): void {}
}
"#,
            "helper",
        );
    }

    #[test]
    fn switch_default_call_emits_event() {
        assert_helper_called(
            r#"<?php
class A {
    public function go(int $x): void {
        switch ($x) {
            case 1:
                break;
            default:
                $this->fallback();
        }
    }
    public function fallback(): void {}
}
"#,
            "fallback",
        );
    }

    #[test]
    fn try_catch_finally_calls_emit_events() {
        let events = collect_events_walking_method(
            r#"<?php
class A {
    public function go(): void {
        try {
            $this->inTry();
        } catch (\Throwable $e) {
            $this->inCatch();
        } finally {
            $this->inFinally();
        }
    }
    public function inTry(): void {}
    public function inCatch(): void {}
    public function inFinally(): void {}
}
"#,
            "a",
            "go",
        );
        for name in ["inTry", "inCatch", "inFinally"] {
            assert!(
                events.iter().any(|e| e.method_name == name),
                "expected a CallSiteEvent for `{name}`; events: {events:?}"
            );
        }
    }

    #[test]
    fn match_arm_call_emits_event() {
        assert_helper_called(
            r#"<?php
class A {
    public function go(int $x): void {
        $r = match ($x) {
            1 => $this->helper(),
            default => $this->fallback(),
        };
    }
    public function helper(): void {}
    public function fallback(): void {}
}
"#,
            "helper",
        );
    }
}
