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
use crate::mago_bridge::word_to_string;
use mago_codex::metadata::function_like::FunctionLikeMetadata;
use mago_codex::symbol::SymbolKind;
use mago_codex::ttype::atomic::object::TObject;
use mago_codex::ttype::atomic::TAtomic;
use mago_codex::ttype::union::TUnion;
use mago_names::ResolvedNames;
use mago_span::HasSpan;
use mago_syntax::ast::access::Access;
use mago_syntax::ast::assignment::Assignment;
use mago_syntax::ast::binary::{Binary, BinaryOperator};
use mago_syntax::ast::block::Block;
use mago_syntax::ast::call::{
    Call, FunctionCall, MethodCall, NullSafeMethodCall, StaticMethodCall,
};
use mago_syntax::ast::class_like::member::ClassLikeMemberSelector;
use mago_syntax::ast::control_flow::r#if::{If, IfBody};
use mago_syntax::ast::expression::Parenthesized;
use mago_syntax::ast::instantiation::Instantiation;
use mago_syntax::ast::r#return::Return;
use mago_syntax::ast::variable::Variable;
use mago_syntax::ast::Expression;
use mago_syntax::ast::Statement;

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

/// Walker context: env + project (codebase) + accumulated events.
///
/// `'a` is the borrow of the project and the resolved-names table; `'arena` is
/// the AST/`ResolvedNames` arena lifetime (mago 1.30 arena allocation).
pub struct WalkerCtx<'a, 'arena> {
    pub env: TypeEnv,
    pub project: &'a crate::mago_bridge::MagoProject,
    pub events: Vec<CallSiteEvent>,
    /// Narrowings collected when walking the most recent conditional expression.
    /// Drained by walk_if before entering branches.
    pub pending_narrowings: Vec<crate::types::narrowing::Narrowing>,
    /// Name resolution table for the current module's Program.
    /// Maps identifier byte-offsets to their resolved fully-qualified names.
    /// Consulted by `resolve_class_fqcn` to translate raw identifiers at
    /// class-name sites to FQCNs. Borrowed transiently (mago 1.30 arena model).
    pub names: &'a ResolvedNames<'arena>,
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

impl<'a, 'arena> WalkerCtx<'a, 'arena> {
    pub fn new(
        env: TypeEnv,
        project: &'a crate::mago_bridge::MagoProject,
        names: &'a ResolvedNames<'arena>,
    ) -> Self {
        Self {
            env,
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
pub fn walk_statements<'arena>(
    env: &mut TypeEnv,
    stmts: impl IntoIterator<Item = impl AsRef<Statement<'arena>>>,
) {
    for stmt in stmts {
        walk_statement(env, stmt.as_ref());
    }
}

/// Walk a single statement. For expression statements, delegates to
/// `walk_expression_simple`; other statement kinds are ignored.
pub fn walk_statement(env: &mut TypeEnv, stmt: &Statement) {
    if let Statement::Expression(expr_stmt) = stmt {
        walk_expression_simple(env, expr_stmt.expression);
    }
}

/// Walk an expression with only env (no project/event collection).
/// Used by the backward-compat helpers in tests.
pub fn walk_expression_simple(env: &mut TypeEnv, expr: &Expression) -> Type {
    match expr {
        Expression::Literal(_) => Type::Mixed,
        Expression::Variable(v) => walk_variable_simple(env, v),
        Expression::Parenthesized(p) => walk_expression_simple(env, p.expression),
        Expression::Instantiation(inst) => walk_instantiation_simple(env, inst),
        Expression::Assignment(a) => walk_assignment_simple(env, a),
        // Call expressions: without a project we can't resolve return types or
        // emit events, but we do need to keep progressing so assignments like
        // `$x = $obj->method()` work (return Mixed for now).
        Expression::Call(_) => Type::Mixed,
        _ => Type::Mixed,
    }
}

/// Walk an expression in a full `WalkerCtx` (env + project + events).
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
            walk_expression(ctx, m.expression);
            for arm in m.arms.iter() {
                match arm {
                    mago_syntax::ast::MatchArm::Expression(a) => {
                        for cond in a.conditions.iter() {
                            walk_expression(ctx, cond);
                        }
                        walk_expression(ctx, a.expression);
                    }
                    mago_syntax::ast::MatchArm::Default(a) => {
                        walk_expression(ctx, a.expression);
                    }
                }
            }
            Type::Mixed
        }
        _ => Type::Mixed,
    }
}

// ── simple (no-project) helpers ───────────────────────────────────────────────

fn walk_variable_simple(env: &mut TypeEnv, v: &Variable) -> Type {
    match var_name(v) {
        Some(name) => env.lookup(&name),
        None => Type::Mixed,
    }
}

fn walk_instantiation_simple(env: &mut TypeEnv, inst: &Instantiation) -> Type {
    match inst.class {
        Expression::Self_(_) => match env.enclosing_class() {
            Some(cls) => Type::SelfRef(cls.to_string()),
            None => Type::Mixed,
        },
        Expression::Static(_) => match env.enclosing_class() {
            Some(cls) => Type::StaticRef(cls.to_string()),
            None => Type::Mixed,
        },
        Expression::Parent(_) => Type::Mixed,
        Expression::Identifier(id) => Type::Class(String::from_utf8_lossy(id.value()).into_owned()),
        _ => Type::Mixed,
    }
}

fn walk_assignment_simple(env: &mut TypeEnv, a: &Assignment) -> Type {
    let rhs_type = walk_expression_simple(env, a.rhs);
    use mago_syntax::ast::assignment::AssignmentOperator;
    if matches!(a.operator, AssignmentOperator::Assign(_)) {
        if let Expression::Variable(v) = a.lhs {
            if let Some(name) = var_name(v) {
                env.set(name, rhs_type.clone());
            }
        }
    }
    rhs_type
}

// ── ctx helpers ───────────────────────────────────────────────────────────────

fn walk_variable(ctx: &mut WalkerCtx, v: &Variable) -> Type {
    match var_name(v) {
        Some(name) => ctx.env.lookup(&name),
        None => Type::Mixed,
    }
}

fn walk_parenthesized(ctx: &mut WalkerCtx, p: &Parenthesized) -> Type {
    walk_expression(ctx, p.expression)
}

fn walk_instantiation(ctx: &mut WalkerCtx, inst: &Instantiation) -> Type {
    // Walk arguments first so nested expressions emit their own events.
    if let Some(arg_list) = &inst.argument_list {
        walk_argument_list(ctx, arg_list);
    }

    let class_type = match inst.class {
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
        let (callee_class, callee_file) = resolve_callee(ctx.project, &class_type, &ctx.env);
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
    let rhs_type = walk_expression(ctx, a.rhs);
    use mago_syntax::ast::assignment::AssignmentOperator;
    if matches!(a.operator, AssignmentOperator::Assign(_)) {
        if let Expression::Variable(v) = a.lhs {
            if let Some(name) = var_name(v) {
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
    let recv_type = walk_expression(ctx, call.object);

    // Extract static method name, fall through to Mixed for dynamic selectors.
    let method_name = match selector_name(&call.method) {
        Some(n) => n,
        None => return walk_args_and_return_mixed(ctx, &call.argument_list),
    };

    let line = line_of_span(ctx, call.object.span().join(call.argument_list.span()));
    let (callee_class, callee_file) = resolve_callee(ctx.project, &recv_type, &ctx.env);
    let return_type = lookup_return_type(ctx.project, &recv_type, &method_name, &ctx.env);

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
    let recv_type = walk_expression(ctx, call.object);

    let method_name = match selector_name(&call.method) {
        Some(n) => n,
        None => return walk_args_and_return_mixed(ctx, &call.argument_list),
    };

    let line = line_of_span(ctx, call.object.span().join(call.argument_list.span()));
    let (callee_class, callee_file) = resolve_callee(ctx.project, &recv_type, &ctx.env);
    let return_type = lookup_return_type(ctx.project, &recv_type, &method_name, &ctx.env);

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
    let class_type = match call.class {
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
        _ => walk_expression(ctx, call.class),
    };

    let method_name = match selector_name(&call.method) {
        Some(n) => n,
        None => return walk_args_and_return_mixed(ctx, &call.argument_list),
    };

    let line = line_of_span(ctx, call.class.span().join(call.argument_list.span()));
    let (callee_class, callee_file) = resolve_callee(ctx.project, &class_type, &ctx.env);
    let return_type = lookup_return_type(ctx.project, &class_type, &method_name, &ctx.env);

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
    if let Expression::Variable(_) = call.function {
        let recv_type = walk_expression(ctx, call.function);
        if matches!(
            recv_type,
            Type::Class(_) | Type::SelfRef(_) | Type::StaticRef(_) | Type::This | Type::Nullable(_)
        ) {
            let line = line_of_span(ctx, call.function.span().join(call.argument_list.span()));
            let (callee_class, callee_file) = resolve_callee(ctx.project, &recv_type, &ctx.env);
            let return_type = lookup_return_type(ctx.project, &recv_type, "__invoke", &ctx.env);
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
    let fn_name = match call.function {
        Expression::Identifier(id) => String::from_utf8_lossy(id.value()).into_owned(),
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
            let recv_type = walk_expression(ctx, prop.object);
            let prop_name = match selector_name(&prop.property) {
                Some(n) => n,
                None => return Type::Mixed,
            };
            resolve_property_type(ctx, &recv_type, &prop_name)
        }
        Access::NullSafeProperty(prop) => {
            // Treat nullable-safe access the same as regular in Phase 2.
            let recv_type = walk_expression(ctx, prop.object);
            let prop_name = match selector_name(&prop.property) {
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
        ClassLikeConstantSelector::Identifier(id) => name_to_lower(id.value) == "class",
        ClassLikeConstantSelector::Expression(_) | ClassLikeConstantSelector::Missing(_) => false,
    };

    if is_class_literal {
        if let Expression::Identifier(id) = cca.class {
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
        walk_expression(ctx, b.lhs);
        walk_expression(ctx, b.rhs);
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
    let _subject_type = walk_expression(ctx, b.lhs);

    // Only narrow when subject is a simple direct variable.
    let var = match b.lhs {
        Expression::Variable(mago_syntax::ast::variable::Variable::Direct(dv)) => {
            String::from_utf8_lossy(dv.name).into_owned()
        }
        _ => return Type::Mixed,
    };

    // Extract class name from RHS: Identifier or Self_/Static keywords.
    let class_type: Type = match b.rhs {
        Expression::Identifier(id) => {
            let name = resolve_class_fqcn(ctx, id);
            match name.to_lowercase().as_str() {
                "self" => ctx
                    .env
                    .enclosing_class()
                    .map(|c| Type::SelfRef(c.to_string()))
                    .unwrap_or(Type::Mixed),
                "static" => ctx
                    .env
                    .enclosing_class()
                    .map(|c| Type::StaticRef(c.to_string()))
                    .unwrap_or(Type::Mixed),
                _ => Type::Class(name),
            }
        }
        Expression::Self_(_) => ctx
            .env
            .enclosing_class()
            .map(|c| Type::SelfRef(c.to_string()))
            .unwrap_or(Type::Mixed),
        Expression::Static(_) => ctx
            .env
            .enclosing_class()
            .map(|c| Type::StaticRef(c.to_string()))
            .unwrap_or(Type::Mixed),
        _ => return Type::Mixed,
    };

    ctx.pending_narrowings
        .push(crate::types::narrowing::Narrowing {
            var,
            ty: class_type,
        });
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
    lookup_property_type(ctx.project, &fqcn, prop_name)
}

/// Walk the inheritance chain to find the declared type of a property.
///
/// Codex `ClassLikeMetadata.properties` is a `WordMap<PropertyMetadata>` keyed
/// by the variable name including the leading `$` (e.g., `"$repo"`). The AST
/// selector `$this->repo` gives `"repo"` (no `$`), so we match either form.
///
/// Constructor-promoted properties may also surface as `__construct` parameters;
/// we fall back to scanning them (via `codebase.get_method`) when the direct
/// property lookup misses.
fn lookup_property_type(
    project: &crate::mago_bridge::MagoProject,
    fqcn: &str,
    property_name: &str,
) -> Type {
    let key_with_dollar = format!("${}", property_name);
    let key_plain = property_name.to_string();
    let codebase = project.codebase();

    let mut current = Some(fqcn.trim_start_matches('\\').to_lowercase());
    for _ in 0..50 {
        let Some(class_fqcn) = current.take() else {
            return Type::Mixed;
        };

        let Some(refl) = project.find_class(&class_fqcn) else {
            return Type::Mixed;
        };

        // 1. Check declared properties (keyed by `$name`).
        for (id, prop_refl) in refl.properties.iter() {
            let raw = word_to_string(id);
            if raw == key_with_dollar || raw == key_plain {
                return reflect_property_type(project, prop_refl);
            }
        }

        // 2. Fall back: check __construct promoted properties.
        if let Some(ctor) = codebase.get_method(class_fqcn.as_bytes(), b"__construct") {
            for param in &ctor.parameters {
                let raw_param = word_to_string(&param.name.0);
                if raw_param == key_with_dollar || raw_param == key_plain {
                    return match param.type_metadata.as_ref() {
                        Some(t) => type_union_to_type(project, &t.type_union, &TypeEnv::default()),
                        None => Type::Mixed,
                    };
                }
            }
        }

        // Climb to parent.
        current = refl
            .direct_parent_class
            .as_ref()
            .map(|n| word_to_string(n).trim_start_matches('\\').to_lowercase());
    }
    Type::Mixed
}

/// Convert a `PropertyMetadata`'s declared type into our `Type`.
fn reflect_property_type(
    project: &crate::mago_bridge::MagoProject,
    prop_refl: &mago_codex::metadata::property::PropertyMetadata,
) -> Type {
    let Some(t) = prop_refl.type_metadata.as_ref() else {
        return Type::Mixed;
    };
    type_union_to_type(project, &t.type_union, &TypeEnv::default())
}

// ── Resolution helpers ────────────────────────────────────────────────────────

/// Extract an identifier name from a `ClassLikeMemberSelector`.
/// Returns `None` for dynamic (`Variable`/`Expression`) selectors.
fn selector_name(sel: &ClassLikeMemberSelector) -> Option<String> {
    match sel {
        ClassLikeMemberSelector::Identifier(id) => {
            Some(String::from_utf8_lossy(id.value).into_owned())
        }
        // Dynamic / missing selector — can't statically resolve.
        ClassLikeMemberSelector::Variable(_)
        | ClassLikeMemberSelector::Expression(_)
        | ClassLikeMemberSelector::Missing(_) => None,
    }
}

/// Walk all arguments of an argument list (for side effects: nested calls → events).
fn walk_argument_list(ctx: &mut WalkerCtx, arg_list: &mago_syntax::ast::argument::ArgumentList) {
    use mago_syntax::ast::argument::Argument;
    for arg in arg_list.arguments.iter() {
        let expr = match arg {
            Argument::Positional(a) => a.value,
            Argument::Named(a) => a.value,
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
    recv_type: &Type,
    env: &TypeEnv,
) -> (Option<String>, Option<PathBuf>) {
    let fqcn = match recv_type {
        Type::Class(c) | Type::SelfRef(c) | Type::StaticRef(c) => Some(c.clone()),
        Type::This => env.enclosing_class().map(|c| c.to_string()),
        // Nullable: recurse into the inner type.
        Type::Nullable(inner) => return resolve_callee(project, inner, env),
        // Interface/Mock carry a name but are like Class for resolution purposes.
        Type::Interface(c) | Type::Mock(c) => Some(c.clone()),
        Type::Mixed | Type::Union(_, _) => None,
    };

    let Some(fqcn) = fqcn else {
        return (None, None);
    };

    // Find the class-like metadata whose name matches the FQCN (case-insensitive,
    // PHP class names are case-insensitive), then resolve its declaring file.
    let file = project
        .find_class(&fqcn)
        .and_then(|refl| project.file_of_span(&refl.span))
        .map(file_path_of);

    (Some(fqcn), file)
}

/// Resolve a loaded `File` to its on-disk path (falls back to logical name).
fn file_path_of(file: &mago_database::file::File) -> PathBuf {
    match &file.path {
        Some(p) => p.clone(),
        None => PathBuf::from(String::from_utf8_lossy(&file.name).into_owned()),
    }
}

/// Look up the declared return type of a method via the codex metadata.
///
/// Returns `Type::Mixed` when the class, method, or return type is not found.
/// `codebase.get_method` resolves the method through the inheritance chain.
fn lookup_return_type(
    project: &crate::mago_bridge::MagoProject,
    recv_type: &Type,
    method: &str,
    env: &TypeEnv,
) -> Type {
    let fqcn = match recv_type {
        Type::Class(c) | Type::SelfRef(c) | Type::StaticRef(c) => c.clone(),
        Type::This => env
            .enclosing_class()
            .map(|c| c.to_string())
            .unwrap_or_default(),
        Type::Nullable(inner) => return lookup_return_type(project, inner, method, env),
        Type::Interface(c) | Type::Mock(c) => c.clone(),
        _ => return Type::Mixed,
    };
    if fqcn.is_empty() {
        return Type::Mixed;
    }

    let class_key = fqcn.trim_start_matches('\\').to_lowercase();
    let method_lc = method.to_lowercase();
    let codebase = project.codebase();
    let Some(m_refl) = codebase.get_method(class_key.as_bytes(), method_lc.as_bytes()) else {
        return Type::Mixed;
    };

    let declared = reflect_return_type(project, m_refl, env);
    // When the declared return is an interface, inspect the method body for a
    // more concrete type (e.g. `return new Foo()` or a factory call whose
    // declared return is a concrete class).
    if matches!(declared, Type::Interface(_)) {
        if let Some(narrowed) =
            narrow_return_type_from_body(project, &fqcn, &method_lc, m_refl, env)
        {
            return narrowed;
        }
    }
    declared
}

/// Convert a method's declared return type into our `Type`.
fn reflect_return_type(
    project: &crate::mago_bridge::MagoProject,
    method_refl: &FunctionLikeMetadata,
    env: &TypeEnv,
) -> Type {
    let Some(rt) = method_refl.return_type_metadata.as_ref() else {
        return Type::Mixed;
    };
    type_union_to_type(project, &rt.type_union, env)
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
    class_fqcn: &str,
    method_lc: &str,
    method_refl: &FunctionLikeMetadata,
    env: &TypeEnv,
) -> Option<Type> {
    // Re-parse the file declaring the method into a scratch arena and inspect
    // its return statements; the AST is arena-bound so the analysis runs inside
    // the `with_program` closure and only the owned `Type` escapes.
    let file = project.file_of_span(&method_refl.span)?;
    let logical_name = String::from_utf8_lossy(&file.name).into_owned();
    let simple = class_fqcn.rsplit('\\').next().unwrap_or(class_fqcn);

    project.with_program(&logical_name, |program, _file, names| {
        let block = find_method_block_in_stmts(program.statements.iter(), simple, method_lc)?;

        let return_types: Vec<Type> = block
            .statements
            .iter()
            .filter_map(|stmt| {
                if let Statement::Return(ret) = stmt {
                    ret.value
                        .map(|expr| narrow_expr_type(expr, project, names, class_fqcn, env))
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
    })?
}

/// Walk `stmts` to find a concrete method body block by class simple-name and
/// method name. Descends into namespace wrappers.
fn find_method_block_in_stmts<'s, 'arena>(
    stmts: impl Iterator<Item = &'s Statement<'arena>>,
    class_simple: &str,
    method_lc: &str,
) -> Option<&'s Block<'arena>>
where
    'arena: 's,
{
    use mago_syntax::ast::class_like::member::ClassLikeMember;
    use mago_syntax::ast::class_like::method::MethodBody;
    use mago_syntax::ast::namespace::NamespaceBody;

    for stmt in stmts {
        match stmt {
            Statement::Class(c) if name_eq_ignore_case(c.name.value, class_simple) => {
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if name_to_lower(m.name.value) == method_lc {
                            if let MethodBody::Concrete(block) = &m.body {
                                return Some(block);
                            }
                        }
                    }
                }
                return None;
            }
            Statement::Trait(t) if name_eq_ignore_case(t.name.value, class_simple) => {
                for member in t.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if name_to_lower(m.name.value) == method_lc {
                            if let MethodBody::Concrete(block) = &m.body {
                                return Some(block);
                            }
                        }
                    }
                }
                return None;
            }
            Statement::Namespace(ns) => {
                let found = match &ns.body {
                    NamespaceBody::Implicit(b) => {
                        find_method_block_in_stmts(b.statements.iter(), class_simple, method_lc)
                    }
                    NamespaceBody::BraceDelimited(b) => {
                        find_method_block_in_stmts(b.statements.iter(), class_simple, method_lc)
                    }
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
    names: &ResolvedNames,
    class_fqcn: &str,
    env: &TypeEnv,
) -> Type {
    match expr {
        Expression::Instantiation(inst) => {
            let class_name = match inst.class {
                Expression::Identifier(id) => narrow_resolve_fqcn(names, id),
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
            let class_name = match smc.class {
                Expression::Identifier(id) => narrow_resolve_fqcn(names, id),
                Expression::Self_(_) | Expression::Static(_) => class_fqcn.to_string(),
                _ => return Type::Mixed,
            };
            let method_name = match selector_name(&smc.method) {
                Some(n) => n,
                None => return Type::Mixed,
            };
            let method_lc = method_name.to_lowercase();
            let class_key = class_name.trim_start_matches('\\').to_lowercase();
            match project
                .codebase()
                .get_method(class_key.as_bytes(), method_lc.as_bytes())
            {
                Some(m_refl) => reflect_return_type(project, m_refl, env),
                None => Type::Mixed,
            }
        }
        Expression::Parenthesized(p) => {
            narrow_expr_type(p.expression, project, names, class_fqcn, env)
        }
        _ => Type::Mixed,
    }
}

fn narrow_resolve_fqcn(names: &ResolvedNames, id: &mago_syntax::ast::Identifier) -> String {
    let fqcn = if names.contains(&id.span().start) {
        String::from_utf8_lossy(names.get(id)).into_owned()
    } else {
        String::from_utf8_lossy(id.value()).into_owned()
    };
    fqcn.trim_start_matches('\\').to_string()
}

/// Case-insensitive compare an AST identifier's raw bytes against a `&str`.
fn name_eq_ignore_case(bytes: &[u8], s: &str) -> bool {
    String::from_utf8_lossy(bytes).eq_ignore_ascii_case(s)
}

/// Lowercase an AST identifier's raw bytes into an owned `String`.
fn name_to_lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_lowercase()
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
// justification: `env` is threaded for API consistency and future use across the
// recursive type-resolution arms; keep it even though it is currently only forwarded.
#[allow(clippy::only_used_in_recursion)]
fn type_union_to_type(
    project: &crate::mago_bridge::MagoProject,
    union: &TUnion,
    env: &TypeEnv,
) -> Type {
    let atoms: &[TAtomic] = union.types.as_ref();

    // Partition into nulls and non-nulls (preserves the 0.26 nullable/union model).
    let null_count = atoms.iter().filter(|a| matches!(a, TAtomic::Null)).count();
    let non_nulls: Vec<&TAtomic> = atoms
        .iter()
        .filter(|a| !matches!(a, TAtomic::Null))
        .collect();

    if null_count > 0 && non_nulls.len() == 1 {
        // Nullable<T>
        let inner = type_atomic_to_type(project, non_nulls[0], env);
        Type::Nullable(Box::new(inner))
    } else if non_nulls.len() == 2 && null_count == 0 {
        // Binary union T|U
        let a = type_atomic_to_type(project, non_nulls[0], env);
        let b = type_atomic_to_type(project, non_nulls[1], env);
        Type::Union(Box::new(a), Box::new(b))
    } else if non_nulls.len() == 1 {
        // Single non-null element → unwrap.
        type_atomic_to_type(project, non_nulls[0], env)
    } else {
        // 3+ non-null types or other complex unions → Mixed for Phase 2.
        Type::Mixed
    }
}

/// Convert a single `TAtomic` (mago_codex::ttype) into our `Type` enum.
///
/// Coverage:
///   - named object → `Type::Interface` or `Type::Class` (by symbol kind);
///     `static`/`$this` named objects map to `StaticRef`/`This`.
///   - intersection (`A&B`) → first named concrete class (the
///     `ConcreteClass&MockObject` PHPUnit pattern; concrete lets the tracer
///     follow real implementations).
///   - everything else (scalar, array, mixed, never, …) → `Type::Mixed`.
fn type_atomic_to_type(
    project: &crate::mago_bridge::MagoProject,
    atomic: &TAtomic,
    env: &TypeEnv,
) -> Type {
    match atomic {
        TAtomic::Object(TObject::Named(named)) => {
            // Intersection type: prefer the first named concrete class among the
            // additional intersection members; otherwise fall through to primary.
            if let Some(intersections) = &named.intersection_types {
                for it in intersections.iter() {
                    if let TAtomic::Object(TObject::Named(inner)) = it {
                        let name_str = word_to_string(&inner.name);
                        if !is_interface(project, &name_str) {
                            return Type::Class(name_str);
                        }
                    }
                }
            }

            let name_str = word_to_string(&named.name);
            if named.is_this {
                // `$this` return type → resolve via env where possible.
                return match env.enclosing_class() {
                    Some(_) => Type::This,
                    None => Type::SelfRef(name_str),
                };
            }
            if named.is_static {
                return Type::StaticRef(name_str);
            }
            if is_interface(project, &name_str) {
                Type::Interface(name_str)
            } else {
                Type::Class(name_str)
            }
        }
        // AnyObject, Enum, WithProperties, HasMethod/HasProperty → Mixed.
        TAtomic::Object(_) => Type::Mixed,
        // Scalar, Array, Mixed, Never, Void, Callable, etc. → Mixed in Phase 2.
        _ => Type::Mixed,
    }
}

/// Whether the given FQCN names an interface (case-insensitive lookup).
fn is_interface(project: &crate::mago_bridge::MagoProject, fqcn: &str) -> bool {
    project
        .find_class(fqcn)
        .map(|r| r.kind == SymbolKind::Interface)
        .unwrap_or(false)
}

/// Public(crate) wrapper for callers outside this module
/// (e.g., `analyzer::trace::seed_param_types`).
pub(crate) fn type_union_to_type_pub(
    project: &crate::mago_bridge::MagoProject,
    union: &TUnion,
    env: &TypeEnv,
) -> Type {
    type_union_to_type(project, union, env)
}

// ── Span / line helper ────────────────────────────────────────────────────────

/// Return the 1-based source line number for a span's start position.
///
/// Falls back to 0 when the source cannot be found (e.g., built-in stubs).
fn line_of_span(ctx: &WalkerCtx, span: mago_span::Span) -> u32 {
    if let Some(file) = ctx.project.file_of_span(&span) {
        // File::line_number returns a 0-based index; add 1 for 1-based.
        file.line_number(span.start.offset) + 1
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
/// The returned FQCN has any leading backslash stripped, because mago's
/// reflection FQCNs don't carry one. Example: identifier `Logger` in
/// `namespace Monolog;` resolves to `"Monolog\\Logger"`, NOT `"\\Monolog\\Logger"`.
fn resolve_class_fqcn(ctx: &WalkerCtx, id: &mago_syntax::ast::Identifier) -> String {
    let position = id.span().start;

    let fqcn = if ctx.names.contains(&position) {
        String::from_utf8_lossy(ctx.names.get(id)).into_owned()
    } else {
        String::from_utf8_lossy(id.value()).into_owned()
    };

    fqcn.trim_start_matches('\\').to_string()
}

/// Extract the variable name string (including `$` prefix) from a `Variable`.
///
/// Only `Variable::Direct` has a static name; indirect (`${...}`) and nested
/// (`$$foo`) are dynamic and return `None`.
fn var_name(v: &Variable) -> Option<String> {
    match v {
        Variable::Direct(dv) => {
            // The raw identifier byte-string already includes the `$` sigil.
            Some(String::from_utf8_lossy(dv.name).into_owned())
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
        Statement::Expression(e) => {
            walk_expression(ctx, e.expression);
        }
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
            walk_expression(ctx, f.expression);
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
            walk_expression(ctx, w.condition);
            for s in w.body.statements() {
                walk_statement_ctx(ctx, s);
            }
        }
        Statement::DoWhile(d) => {
            walk_statement_ctx(ctx, d.statement);
            walk_expression(ctx, d.condition);
        }
        Statement::Switch(sw) => {
            walk_expression(ctx, sw.expression);
            for case in sw.body.cases() {
                if let mago_syntax::ast::SwitchCase::Expression(c) = case {
                    walk_expression(ctx, c.expression);
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
    walk_expression(ctx, if_stmt.condition);
    let narrowings: Vec<crate::types::narrowing::Narrowing> =
        std::mem::take(&mut ctx.pending_narrowings);

    // True branch: save env, apply narrowings, walk body, restore env.
    {
        let saved = ctx.env.clone();
        let tuples: Vec<(String, Type)> = narrowings
            .iter()
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
                walk_expression(ctx, elseif.condition);
                let ei_narrowings: Vec<crate::types::narrowing::Narrowing> =
                    std::mem::take(&mut ctx.pending_narrowings);
                let saved = ctx.env.clone();
                let tuples: Vec<(String, Type)> = ei_narrowings
                    .iter()
                    .map(|n| (n.var.clone(), n.ty.clone()))
                    .collect();
                ctx.env.apply_narrowing(&tuples);
                walk_statement_ctx(ctx, elseif.statement);
                ctx.env = saved;
            }
            // Else branch: no narrowing applied (negation not modelled in Phase 2).
            if let Some(else_clause) = &sb.else_clause {
                let saved = ctx.env.clone();
                walk_statement_ctx(ctx, else_clause.statement);
                ctx.env = saved;
            }
        }
        IfBody::ColonDelimited(cb) => {
            for elseif in cb.else_if_clauses.iter() {
                ctx.pending_narrowings.clear();
                walk_expression(ctx, elseif.condition);
                let ei_narrowings: Vec<crate::types::narrowing::Narrowing> =
                    std::mem::take(&mut ctx.pending_narrowings);
                let saved = ctx.env.clone();
                let tuples: Vec<(String, Type)> = ei_narrowings
                    .iter()
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
        IfBody::Statement(sb) => walk_statement_ctx(ctx, sb.statement),
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

    /// Owns walk results so legacy `ctx.events` / `ctx.env` assertions keep working
    /// after the helpers moved the walk inside a `with_program` closure.
    struct EventsHolder {
        events: Vec<CallSiteEvent>,
    }

    /// Owns a walked env for legacy `ctx.env.lookup(...)` assertions.
    struct EnvHolder {
        env: TypeEnv,
    }

    /// Logical name of the file declaring the given class (case-insensitive FQCN).
    fn file_name_of_class(project: &MagoProject, class_lc: &str) -> String {
        let refl = project
            .class_likes()
            .find(|r| word_to_string(&r.name).to_lowercase() == class_lc)
            .expect("class not found in codebase");
        let file = project
            .file_of_span(&refl.span)
            .expect("declaring file not found");
        String::from_utf8_lossy(&file.name).into_owned()
    }

    /// Logical name of the first loaded file's declaring class. Tests write a
    /// single class per snippet, so the first class is unambiguous.
    fn first_class_file_name(project: &MagoProject) -> (String, String) {
        let refl = project.class_likes().next().expect("at least one class");
        let class_name = word_to_string(&refl.name);
        let file = project
            .file_of_span(&refl.span)
            .expect("declaring file not found");
        (String::from_utf8_lossy(&file.name).into_owned(), class_name)
    }

    /// Find the AST node of the named class in a program's statements,
    /// descending into namespaces. Returns the matching `Class` node.
    fn find_class_node<'p, 'arena>(
        program: &'p mago_syntax::ast::Program<'arena>,
        class_lc: &str,
    ) -> Option<&'p mago_syntax::ast::Class<'arena>> {
        for s in program.statements.iter() {
            match s {
                Statement::Class(c) if name_to_lower(c.name.value) == class_lc => return Some(c),
                _ => {}
            }
        }
        // Fall back to the first class statement when no name match is given.
        if class_lc.is_empty() {
            for s in program.statements.iter() {
                if let Statement::Class(c) = s {
                    return Some(c);
                }
            }
        }
        None
    }

    /// Find the concrete body block of a method on a class AST node.
    fn method_block<'p, 'arena>(
        class: &'p mago_syntax::ast::Class<'arena>,
        method_lc: &str,
    ) -> Option<&'p mago_syntax::ast::Block<'arena>> {
        for m in class.members.iter() {
            if let ClassLikeMember::Method(m) = m {
                if name_to_lower(m.name.value) == method_lc {
                    if let MethodBody::Concrete(b) = &m.body {
                        return Some(b);
                    }
                }
            }
        }
        None
    }

    /// Walk every `Statement::Expression` in the named method's body and return
    /// the resulting events + env. Mirrors the original inline test loops.
    fn walk_method_expressions(
        project: &MagoProject,
        class_lc: &str,
        method_lc: &str,
    ) -> (Vec<CallSiteEvent>, TypeEnv) {
        let logical = file_name_of_class(project, class_lc);
        project
            .with_program(&logical, |program, _file, names| {
                let class = find_class_node(program, class_lc).expect("class node");
                let class_name = String::from_utf8_lossy(class.name.value).into_owned();
                let block = method_block(class, method_lc).expect("method block");
                let env = TypeEnv::for_class(&class_name);
                let mut ctx = WalkerCtx::new(env, project, names);
                for stmt in block.statements.iter() {
                    if let Statement::Expression(e) = stmt {
                        walk_expression(&mut ctx, e.expression);
                    }
                }
                (ctx.events, ctx.env)
            })
            .expect("file parses")
    }

    /// Walk the full body of the named method via `walk_block` (statement-level
    /// ctx walk), seeding `seeds` into the env first. Returns events + env.
    fn walk_method_block_seeded(
        project: &MagoProject,
        class_lc: &str,
        method_lc: &str,
        seeds: &[(&str, Type)],
    ) -> (Vec<CallSiteEvent>, TypeEnv) {
        let logical = file_name_of_class(project, class_lc);
        project
            .with_program(&logical, |program, _file, names| {
                let class = find_class_node(program, class_lc).expect("class node");
                let class_name = String::from_utf8_lossy(class.name.value).into_owned();
                let block = method_block(class, method_lc).expect("method block");
                let mut env = TypeEnv::for_class(&class_name);
                for (k, v) in seeds {
                    env.set((*k).to_string(), v.clone());
                }
                let mut ctx = WalkerCtx::new(env, project, names);
                walk_block(&mut ctx, block);
                (ctx.events, ctx.env)
            })
            .expect("file parses")
    }

    /// Build a PHP snippet whose single method nests `n` `new A(...)` calls,
    /// e.g. `new A(new A(...))`. Each instantiation emits one `__construct`
    /// CallSiteEvent, so the event count equals the reached nesting depth.
    fn nested_new(n: usize) -> String {
        let mut s = String::from("<?php\nclass A {\n  public function go(): void {\n    $x = ");
        for _ in 0..n {
            s.push_str("new A(");
        }
        for _ in 0..n {
            s.push(')');
        }
        s.push_str(";\n  }\n}\n");
        s
    }

    /// Walk the first concrete method of the first class with a custom
    /// `max_depth`, returning how many CallSiteEvents were emitted.
    fn count_events_with_max_depth(php: &str, max_depth: u32) -> usize {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Test.php"), php).unwrap();
        let project = MagoProject::load(dir.path()).expect("load ok");
        let project = &project;
        let (logical, class_name) = first_class_file_name(project);
        project
            .with_program(&logical, |program, _file, names| {
                let class = find_class_node(program, &class_name.to_lowercase()).expect("class");
                let block = class
                    .members
                    .iter()
                    .find_map(|m| {
                        if let ClassLikeMember::Method(m) = m {
                            if let MethodBody::Concrete(b) = &m.body {
                                return Some(b);
                            }
                        }
                        None
                    })
                    .expect("method");
                let env = TypeEnv::for_class(&class_name);
                let mut ctx = WalkerCtx::new(env, project, names);
                ctx.max_depth = max_depth;
                for stmt in block.statements.iter() {
                    walk_statement_ctx(&mut ctx, stmt);
                }
                ctx.events.len()
            })
            .expect("file parses")
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
            (1..=8).contains(&events),
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
        assert_eq!(
            events, 20,
            "default-depth walking must emit one event per instantiation"
        );
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
        assert!(
            ok,
            "deeply-nested PHP must load on the deep stack without overflowing"
        );
    }

    /// Walk the first method body of the first class, return the type of `$var_name`.
    fn walk_first_method_var(php: &str, var_name: &str) -> Type {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Test.php"), php).unwrap();
        let project = MagoProject::load(dir.path()).expect("load ok");
        let (logical, class_name) = first_class_file_name(&project);

        project
            .with_program(&logical, |program, _file, _names| {
                let class = find_class_node(program, &class_name.to_lowercase()).expect("class");
                // Use the AST class name (original casing) so `SelfRef`/`StaticRef`
                // preserve the source spelling, matching the 0.26 behaviour.
                let ast_class_name = String::from_utf8_lossy(class.name.value).into_owned();
                let block = class
                    .members
                    .iter()
                    .find_map(|m| {
                        if let ClassLikeMember::Method(m) = m {
                            if let MethodBody::Concrete(b) = &m.body {
                                return Some(b);
                            }
                        }
                        None
                    })
                    .expect("method");

                let mut env = TypeEnv::for_class(&ast_class_name);
                for stmt in block.statements.iter() {
                    walk_statement(&mut env, stmt);
                }
                env.lookup(var_name)
            })
            .expect("file parses")
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
        std::fs::write(
            dir.path().join("A.php"),
            r#"<?php
class A {
    public function caller(): void {
        $b = new B();
        $b->doIt();
    }
}
class B {
    public function doIt(): void {}
}
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();

        let (events, _env) = walk_method_expressions(&project, "a", "caller");

        let do_it_events: Vec<&CallSiteEvent> =
            events.iter().filter(|e| e.method_name == "doIt").collect();

        assert_eq!(
            do_it_events.len(),
            1,
            "expected 1 doIt call site event; got {} — all events: {:?}",
            do_it_events.len(),
            events
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
        std::fs::write(
            dir.path().join("Chain.php"),
            r#"<?php
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
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();

        let (_events, env) = walk_method_expressions(&project, "client", "run");

        // $result should have been assigned the return type of Builder::build() = Class("Result")
        let result_type = env.lookup("$result");
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
        let (_events, env) = walk_method_expressions(project, class_lc, method_lc);
        env
    }

    #[test]
    fn this_property_resolves_to_declared_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Service.php"),
            r#"<?php
class Repo {}
class Service {
    public Repo $repo;
    public function go(): void {
        $r = $this->repo;
    }
}
"#,
        )
        .unwrap();

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
        std::fs::write(
            dir.path().join("Service2.php"),
            r#"<?php
class Repo {}
class Service2 {
    public function __construct(private Repo $repo) {}
    public function go(): void {
        $r = $this->repo;
    }
}
"#,
        )
        .unwrap();

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
        std::fs::write(
            dir.path().join("NullSafe.php"),
            r#"<?php
class Inner {}
class Outer {
    public Inner $inner;
    public function go(): void {
        $i = $this?->inner;
    }
}
"#,
        )
        .unwrap();

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
        std::fs::write(
            dir.path().join("Inherit.php"),
            r#"<?php
class Dep {}
class Base {
    public Dep $dep;
}
class Child extends Base {
    public function go(): void {
        $d = $this->dep;
    }
}
"#,
        )
        .unwrap();

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
        std::fs::write(
            dir.path().join("Chain.php"),
            r#"<?php
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
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let (events, _env) = walk_method_expressions(&project, "caller", "go");
        let ctx = EventsHolder { events };

        // We expect FOUR call site events: __construct (for new C()), returnsB(), returnsA(), done().
        // Their receivers should be C → C → B → A.
        let method_names: Vec<&str> = ctx.events.iter().map(|e| e.method_name.as_str()).collect();
        assert_eq!(method_names, vec!["__construct", "returnsB", "returnsA", "done"],
            "expected events for __construct, returnsB, returnsA, done in order; got: {method_names:?}");

        assert_eq!(
            ctx.events[0].receiver,
            Type::Class("C".into()),
            "__construct receiver should be C (instantiation); got {:?}",
            ctx.events[0].receiver
        );
        assert_eq!(
            ctx.events[1].receiver,
            Type::Class("C".into()),
            "returnsB receiver should be C; got {:?}",
            ctx.events[1].receiver
        );
        assert_eq!(
            ctx.events[2].receiver,
            Type::Class("B".into()),
            "returnsA receiver should be B (from returnsB's declared return type); got {:?}",
            ctx.events[2].receiver
        );
        assert_eq!(
            ctx.events[3].receiver,
            Type::Class("A".into()),
            "done receiver should be A (from returnsA's declared return type); got {:?}",
            ctx.events[3].receiver
        );
    }

    // ── Task 2.8 tests ─────────────────────────────────────────────────────────

    #[test]
    fn instanceof_narrows_inside_if_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Narrow.php"),
            r#"<?php
class Base { public function baseMethod(): void {} }
class Dog extends Base { public function bark(): void {} }
class Caller {
    public function go(Base $animal): void {
        if ($animal instanceof Dog) {
            $animal->bark();
        }
    }
}
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        // Seed $animal as Base (its declared param type), then walk the full body.
        let (events, env) = walk_method_block_seeded(
            &project,
            "caller",
            "go",
            &[("$animal", Type::Class("Base".into()))],
        );

        // The bark() call should have been emitted with receiver = Class(Dog).
        let bark_event = events.iter().find(|e| e.method_name == "bark");
        assert!(
            bark_event.is_some(),
            "expected bark() call site emitted; got events: {:?}",
            events
        );
        assert_eq!(
            bark_event.unwrap().receiver,
            Type::Class("Dog".into()),
            "bark() receiver should be narrowed Dog (not Base)"
        );

        // After the if-body, $animal should be Base again.
        assert_eq!(
            env.lookup("$animal"),
            Type::Class("Base".into()),
            "$animal should restore to Base after the if-body"
        );
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
        std::fs::write(
            dir.path().join("UR.php"),
            r#"<?php
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
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let (_events, walk_env) = walk_method_expressions(&project, "caller", "go");
        let ctx = EnvHolder { env: walk_env };

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
        std::fs::write(
            dir.path().join("NR.php"),
            r#"<?php
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
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let (_events, walk_env) = walk_method_expressions(&project, "caller", "go");
        let ctx = EnvHolder { env: walk_env };

        // $result should have Type::Nullable(Box::new(Type::Class("Foo")))
        let result_type = ctx.env.lookup("$result");
        match &result_type {
            Type::Nullable(inner) => match inner.as_ref() {
                Type::Class(c) => {
                    assert_eq!(c, "Foo", "expected nullable Foo; got: {result_type:?}");
                }
                _other => panic!("expected Nullable(Class(Foo)), got: {result_type:?}"),
            },
            other => panic!("expected Type::Nullable, got: {other:?}"),
        }
    }

    #[test]
    fn degenerate_union_unwraps_to_single_type() {
        // A union with only one actual non-null kind should unwrap to that kind.
        // This is a tricky edge case from type_kind_to_type's logic.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Degen.php"),
            r#"<?php
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
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let (_events, walk_env) = walk_method_expressions(&project, "caller", "go");
        let ctx = EnvHolder { env: walk_env };

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
        std::fs::write(
            dir.path().join("Three.php"),
            r#"<?php
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
"#,
        )
        .unwrap();

        let project = MagoProject::load(dir.path()).unwrap();
        let (_events, walk_env) = walk_method_expressions(&project, "caller", "go");
        let ctx = EnvHolder { env: walk_env };

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
        )
        .unwrap();
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
        let env = TypeEnv::for_class("Concrete");

        let result = lookup_return_type(
            &project,
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
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ConcreteDriver.php"),
            "<?php\nclass ConcreteDriver implements DriverInterface {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Factory.php"),
            "<?php\nclass Factory {\n  public static function create(): ConcreteDriver { return new ConcreteDriver(); }\n}\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("Concrete.php"),
            "<?php\nclass Concrete {\n  protected function loadDriver(): DriverInterface {\n    return Factory::create();\n  }\n}\n",
        ).unwrap();

        let project = MagoProject::load(dir.path()).expect("load ok");
        let env = TypeEnv::for_class("Concrete");

        let result = lookup_return_type(
            &project,
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
        )
        .unwrap();
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
        let env = TypeEnv::for_class("App\\Tests\\ConcreteTest");

        let result = lookup_return_type(
            &project,
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
    fn collect_events_walking_method(
        php: &str,
        class_lc: &str,
        method_lc: &str,
    ) -> Vec<CallSiteEvent> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("M1.php"), php).unwrap();
        let project = MagoProject::load(dir.path()).expect("load ok");
        let project = &project;
        let logical = file_name_of_class(project, class_lc);
        project
            .with_program(&logical, |program, _file, names| {
                let class = find_class_node(program, class_lc)
                    .unwrap_or_else(|| panic!("class {class_lc} not found"));
                let class_name = String::from_utf8_lossy(class.name.value).into_owned();
                let block = method_block(class, method_lc)
                    .unwrap_or_else(|| panic!("method {method_lc} not found"));
                let env = TypeEnv::for_class(&class_name);
                let mut ctx = WalkerCtx::new(env, project, names);
                for stmt in block.statements.iter() {
                    walk_statement_ctx(&mut ctx, stmt);
                }
                ctx.events
            })
            .expect("file parses")
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
