//! The mago AST → [`Term`] bridge — building symbolic terms from a test method's
//! body and deciding the final assertion (the "réduction par substitution" front
//! end over the [`super::term`] kernel).
//!
//! # What this does
//!
//! [`decide_test`] locates a TEST method's body, enters its parameters as FREE
//! Givens ([`Term::Sym`], typed via the param hints), walks the pure prefix of the
//! body (assignments + a final assertion), building a [`Term`] per expression —
//! INCLUDING through `new C(args)` (a transparent [`Term::Obj`]) and pure
//! accessor/transform method bodies (one-statement `return` inlined by
//! substitution). The final assertion is decided by [`super::term::decide_same`] /
//! [`decide_eq`] over the normal forms. A test true *for all inputs* decides with
//! NO concrete values:
//!
//! ```php
//! function testPlus(int $a, int $b): void {
//!     $r = (new Money($a))->plus($b);          // plus(x){ return new Money($this->amount + x); }
//!     $this->assertSame($a + $b, $r->getAmount());   // getAmount(){ return $this->amount; }
//! }
//! ```
//!
//! Both sides reduce to `a + b` → [`Decision::True`] ∀ a, b.
//!
//! # Ownership constraint (load-bearing, same as [`super::subst`])
//!
//! mago 1.30 keeps NO parsed AST around: [`MagoProject::with_program`] re-parses a
//! file into a SCOPED arena dropped when the closure returns. [`Term`] is 100%
//! owned (String / i64 / Box), so every term is built COMPLETELY INSIDE the
//! closure and returned owned — no `&Block` / `&Expression` / `&[u8]` borrow of the
//! source or AST ever escapes. Inlining a method re-enters `with_program` with its
//! own arena (nested correctly), guarded by a shared depth cap.
//!
//! # Fail-closed
//!
//! Every construct outside the modelled pure subset returns `Err(BailReason)`,
//! which [`decide_test`] converts to [`Decision::Unknown`] — the runner then
//! executes the test for real. No path guesses a verdict.

use std::cell::Cell;
use std::collections::HashMap;

use mago_names::ResolvedNames;
use mago_syntax::ast::ast::access::Access;
use mago_syntax::ast::ast::argument::{Argument, ArgumentList};
use mago_syntax::ast::ast::array::{ArrayElement, LegacyArray};
use mago_syntax::ast::ast::assignment::AssignmentOperator;
use mago_syntax::ast::ast::binary::BinaryOperator;
use mago_syntax::ast::ast::call::Call;
use mago_syntax::ast::ast::class_like::member::ClassLikeMemberSelector;
use mago_syntax::ast::ast::class_like::method::MethodBody;
use mago_syntax::ast::ast::expression::Expression;
use mago_syntax::ast::ast::function_like::parameter::{
    FunctionLikeParameter, FunctionLikeParameterList,
};
use mago_syntax::ast::ast::instantiation::Instantiation;
use mago_syntax::ast::ast::literal::Literal;
use mago_syntax::ast::ast::statement::Statement;
use mago_syntax::ast::ast::type_hint::Hint;
use mago_syntax::ast::ast::variable::Variable;

use super::eval::BailReason;
use super::subst::{find_class_method, normalize_fqcn, strip_dollar};
use super::term::{decide_eq, decide_same, Decision, Op, Term};
use crate::mago_bridge::MagoProject;

/// The scalar type of a Given / a typed sink, for the coercion guard (step 6).
/// A param/property whose hint is not a bare scalar is `None` in the maps below —
/// "no scalar coercion possible, store/pass verbatim".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarTy {
    Int,
    Float,
    Str,
    Bool,
}

/// Bare variable/property name → its built [`Term`] (the scope's variable bindings).
type TermBindings = HashMap<Vec<u8>, Term>;
/// Bare variable name → its [`ScalarTy`] (the in-scope Givens' types).
type TypeBindings = HashMap<Vec<u8>, ScalarTy>;

/// Classify a param/return `&Hint` into a [`ScalarTy`] — ONLY a bare scalar hint
/// (`int`/`float`/`string`/`bool`); everything else (class names, arrays, unions,
/// nullable wrappers, `mixed`, …) is `None` ("no scalar sink, no coercion").
fn scalar_ty_of(hint: &Hint) -> Option<ScalarTy> {
    match hint {
        Hint::Integer(_) => Some(ScalarTy::Int),
        Hint::Float(_) => Some(ScalarTy::Float),
        Hint::String(_) => Some(ScalarTy::Str),
        Hint::Bool(_) => Some(ScalarTy::Bool),
        _ => None,
    }
}

/// The cap on `with_program` / inline re-entries (mirrors [`super::subst`]'s
/// `max_depth = 64`): a deep call graph bails rather than re-parsing forever.
const MAX_DEPTH: u32 = 64;

/// The term-building environment for ONE body scope (calques [`super::eval::Scope`]
/// but produces [`Term`]s and carries NO value-side state — no `allow_this_write`,
/// `prop_hints`, or aliasing). `vars` carries `$this` (bound to the receiver
/// [`Term`]) and locals; `given_types` carries the FREE Givens' scalar types for
/// the coercion guard; `depth` is a shared re-entry counter.
struct TermEnv<'a> {
    vars: TermBindings,
    given_types: TypeBindings,
    project: &'a MagoProject,
    names: Option<&'a ResolvedNames<'a>>,
    depth: &'a Cell<u32>,
}

impl<'a> TermEnv<'a> {
    /// Run `f` under an incremented re-entry depth (the `with_program` / inline
    /// guard), restoring the depth afterwards. Bails past [`MAX_DEPTH`].
    fn with_depth<R>(
        depth: &Cell<u32>,
        f: impl FnOnce() -> Result<R, BailReason>,
    ) -> Result<R, BailReason> {
        let d = depth.get();
        if d >= MAX_DEPTH {
            return Err(BailReason::Other("term-bridge recursion depth cap".into()));
        }
        depth.set(d + 1);
        let result = f();
        depth.set(d);
        result
    }

    /// Resolve a class-name expression at `inst.class`'s position to a concrete
    /// FQCN via the names table (calque [`super::eval`]'s `resolve_class_name_in_scope`):
    /// `self`/`parent`/`static`, `new $var`, anonymous classes → BAIL.
    fn resolve_class_name(&self, expr: &Expression) -> Result<Vec<u8>, BailReason> {
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
            if let Some(names) = self.names {
                if let Some(fqcn) = names.resolve(expr) {
                    return Ok(fqcn.to_vec());
                }
            }
            return Ok(name.to_vec());
        }
        Err(BailReason::UnsupportedConstruct(
            "dynamic/unresolvable class name (new \\$var / anonymous class)".into(),
        ))
    }

    // ─── Step 3+4: build a Term from an expression ────────────────────────────

    fn build(&mut self, expr: &Expression) -> Result<Term, BailReason> {
        match expr {
            Expression::Literal(lit) => build_literal(lit),
            Expression::Parenthesized(p) => self.build(p.expression),

            Expression::Variable(Variable::Direct(v)) => {
                let key = var_name(v.name);
                if let Some(t) = self.vars.get(&key) {
                    return Ok(t.clone());
                }
                // ABSENT from the env = a FREE Given → a fresh symbol (the
                // divergence vs the value interpreter's UnboundVariable bail).
                Ok(Term::Sym(String::from_utf8_lossy(&key).into_owned()))
            }
            Expression::Variable(_) => Err(BailReason::UnsupportedConstruct(
                "indirect/nested variable".into(),
            )),

            Expression::Binary(b) => self.build_binary(&b.operator, b.lhs, b.rhs),

            // `$obj->prop` read → a transparent Field (reduces by substitution
            // when the receiver normalises to an Obj).
            Expression::Access(access) => self.build_access(access),

            // `new C(args)` → a transparent Obj (step 6).
            Expression::Instantiation(inst) => self.build_instantiation(inst),

            // `count(...)`/`sizeof(...)` → Len; `$recv->m(args)` accessor/transform
            // → substitution (step 7); array literal → List (step 4).
            Expression::Call(call) => self.build_call(call),
            Expression::Array(arr) => self.build_array(arr.elements.as_slice()),
            Expression::LegacyArray(arr) => self.build_legacy_array(arr),

            other => Err(BailReason::UnsupportedConstruct(format!(
                "expression: {}",
                expr_kind(other)
            ))),
        }
    }

    fn build_binary(
        &mut self,
        op: &BinaryOperator,
        lhs: &Expression,
        rhs: &Expression,
    ) -> Result<Term, BailReason> {
        let term_op = match op {
            BinaryOperator::Addition(_) => Op::Add,
            BinaryOperator::Subtraction(_) => Op::Sub,
            BinaryOperator::Multiplication(_) => Op::Mul,
            // Division (and every other operator) is NOT modelled in the kernel →
            // BAIL (fail-closed). `/` in particular has the 0-divisor trap.
            other => {
                return Err(BailReason::UnsupportedConstruct(format!(
                    "binary operator {:?} (only +,-,* modelled in the term kernel)",
                    std::mem::discriminant(other)
                )))
            }
        };
        let a = self.build(lhs)?;
        let b = self.build(rhs)?;
        Ok(Term::Bin(term_op, Box::new(a), Box::new(b)))
    }

    fn build_access(&mut self, access: &Access) -> Result<Term, BailReason> {
        match access {
            Access::Property(pa) => {
                let ClassLikeMemberSelector::Identifier(prop_id) = &pa.property else {
                    return Err(BailReason::UnsupportedConstruct(
                        "dynamic property selector".into(),
                    ));
                };
                let obj = self.build(pa.object)?;
                Ok(Term::Field(
                    Box::new(obj),
                    String::from_utf8_lossy(prop_id.value).into_owned(),
                ))
            }
            Access::NullSafeProperty(_) => Err(BailReason::UnsupportedConstruct(
                "null-safe property access (?->)".into(),
            )),
            Access::StaticProperty(_) => Err(BailReason::UnsupportedConstruct(
                "static property access".into(),
            )),
            Access::ClassConstant(_) => Err(BailReason::UnsupportedConstruct(
                "class-constant access".into(),
            )),
        }
    }

    fn build_array(&mut self, elements: &[ArrayElement]) -> Result<Term, BailReason> {
        let mut items = Vec::new();
        for element in elements {
            match element {
                ArrayElement::Value(ve) => items.push(self.build(ve.value)?),
                ArrayElement::KeyValue(kv) => items.push(self.build(kv.value)?),
                ArrayElement::Variadic(_) => {
                    return Err(BailReason::UnsupportedConstruct(
                        "array spread (...)".into(),
                    ))
                }
                ArrayElement::Missing(_) => {
                    return Err(BailReason::UnsupportedConstruct(
                        "missing array element".into(),
                    ))
                }
            }
        }
        Ok(Term::List(items))
    }

    fn build_legacy_array(&mut self, arr: &LegacyArray) -> Result<Term, BailReason> {
        self.build_array(arr.elements.as_slice())
    }

    fn build_call(&mut self, call: &Call) -> Result<Term, BailReason> {
        match call {
            Call::Function(fc) => {
                // Only the structural `count`/`sizeof` builtin is modelled as a
                // Term; a closure callee or any other free call BAILS.
                let Some(name) = identifier_name(fc.function) else {
                    return Err(BailReason::UnsupportedConstruct(
                        "dynamic function call (non-identifier callee)".into(),
                    ));
                };
                if matches!(name, b"count" | b"sizeof") {
                    let arg = single_positional_arg(&fc.argument_list)?;
                    let inner = self.build(arg)?;
                    return Ok(Term::Len(Box::new(inner)));
                }
                Err(BailReason::UnknownCall(
                    String::from_utf8_lossy(name).into_owned(),
                ))
            }
            // `$recv->method(args)` accessor/transform → substitution (step 7).
            Call::Method(m) => {
                let ClassLikeMemberSelector::Identifier(method_id) = &m.method else {
                    return Err(BailReason::UnsupportedConstruct(
                        "dynamic method selector".into(),
                    ));
                };
                let receiver = self.build(m.object)?;
                let arg_terms = self.build_arguments(&m.argument_list)?;
                self.inline_pure_method(receiver, method_id.value, arg_terms)
            }
            Call::StaticMethod(_) => Err(BailReason::UnsupportedConstruct(
                "static method call".into(),
            )),
            Call::NullSafeMethod(_) => Err(BailReason::UnsupportedConstruct(
                "null-safe method call (?->)".into(),
            )),
        }
    }

    /// Build each positional argument as a [`Term`]. Variadic / by-ref-spread /
    /// named arguments BAIL (no model).
    fn build_arguments(&mut self, args: &ArgumentList) -> Result<Vec<Term>, BailReason> {
        let mut out = Vec::new();
        for arg in args.arguments.iter() {
            match arg {
                Argument::Positional(p) => {
                    if p.ellipsis.is_some() {
                        return Err(BailReason::UnsupportedConstruct("argument spread".into()));
                    }
                    out.push(self.build(p.value)?);
                }
                Argument::Named(_) => {
                    return Err(BailReason::UnsupportedConstruct("named argument".into()))
                }
            }
        }
        Ok(out)
    }

    // ─── Step 6: `new C(args)` → a transparent Obj ────────────────────────────

    fn build_instantiation(&mut self, inst: &Instantiation) -> Result<Term, BailReason> {
        let class = self.resolve_class_name(inst.class)?;
        let arg_terms = match &inst.argument_list {
            Some(list) => self.build_arguments(list)?,
            None => Vec::new(),
        };
        self.construct_obj(&class, arg_terms)
    }

    /// Construct a transparent [`Term::Obj`] for `new class(args)`. Mirrors
    /// [`super::subst`]'s `construct_object` SKELETON + GUARDS in EXACT order (the
    /// resolvable/abstract/trait/magic-method/non-public-ctor bails), then maps the
    /// promoted ctor params to fields (overlaid by the ctor body's pure
    /// `$this->x = <expr>` writes). The object's class tag is the original-cased
    /// FQCN. A non-promoted, non-trivial ctor body BAILS.
    fn construct_obj(&mut self, class: &[u8], args: Vec<Term>) -> Result<Term, BailReason> {
        let codebase = self.project.codebase();
        let class = normalize_fqcn(class);
        let class = class.as_slice();

        let Some(class_meta) = codebase.get_class_like(class) else {
            return Err(BailReason::UnknownCall(format!(
                "new {} (class not in codebase)",
                String::from_utf8_lossy(class)
            )));
        };
        // Same guard order as construct_object: abstract → non-class → declared
        // ancestry → used traits → __set/__get → public ctor.
        if class_meta.flags.is_abstract() {
            return Err(BailReason::UnsupportedConstruct(
                "cannot instantiate abstract class (PHP Error)".into(),
            ));
        }
        if !class_meta.kind.is_class() {
            return Err(BailReason::UnsupportedConstruct(format!(
                "cannot instantiate a {} (PHP Error)",
                class_meta.kind.as_str()
            )));
        }
        self.bail_unresolvable_declared_ancestry(class)?;
        self.bail_unresolvable_used_traits(class)?;
        self.bail_magic_method_in_chain(class, b"__set", "magic property-write routing")?;
        self.bail_magic_method_in_chain(class, b"__get", "magic property-read routing")?;

        let record_class =
            String::from_utf8_lossy(class_meta.original_name.as_bytes()).into_owned();

        // No constructor: only the no-arg case is a valid object with no fields.
        let Some(ctor_meta) = codebase.get_declaring_method(class, b"__construct") else {
            if !args.is_empty() {
                return Err(BailReason::TypeError(
                    "arguments passed to a class with no constructor".into(),
                ));
            }
            return Ok(Term::Obj(record_class, Vec::new()));
        };
        if ctor_meta
            .method_metadata
            .as_ref()
            .is_some_and(|m| m.is_abstract)
        {
            return Err(BailReason::UnsupportedConstruct(
                "abstract constructor".into(),
            ));
        }
        let ctor_is_public = ctor_meta
            .method_metadata
            .as_ref()
            .is_none_or(|m| matches!(m.visibility, mago_codex::visibility::Visibility::Public));
        if !ctor_is_public {
            return Err(BailReason::UnsupportedConstruct(
                "new on a class with a non-public constructor (PHP visibility Error unmodelled)"
                    .into(),
            ));
        }

        let ctor_class = codebase
            .get_declaring_method_class(class, b"__construct")
            .map(|w| w.as_bytes().to_vec())
            .unwrap_or_else(|| class.to_vec());
        let file = self
            .project
            .file_of_span(&ctor_meta.span)
            .ok_or_else(|| BailReason::Other("constructor file not loaded".into()))?;
        let logical = String::from_utf8_lossy(&file.name).into_owned();

        // Re-enter with_program (depth-guarded) to read the ctor body and build
        // the field terms entirely inside the closure (owned out).
        let project = self.project;
        let given_types = self.given_types.clone();
        let depth = self.depth;
        let arg_terms = args;
        let record_class_for_closure = record_class.clone();
        let outcome = Self::with_depth(depth, || {
            project
                .with_program(&logical, |program, _file, names| {
                    let ctor = find_class_method(program, &ctor_class, b"__construct").ok_or_else(
                        || BailReason::Other("constructor AST not found after re-parse".into()),
                    )?;
                    let MethodBody::Concrete(block) = &ctor.body else {
                        return Err(BailReason::UnsupportedConstruct(
                            "abstract/interface constructor body".into(),
                        ));
                    };
                    // Bind the ctor params to the arg terms (with the coercion
                    // guard at the typed-scalar sink), tracking each param's scalar
                    // type so a body `$this->x = $param` can carry it forward.
                    let (param_terms, param_types) =
                        bind_param_terms(&ctor.parameter_list, &arg_terms, &given_types)?;

                    // Promoted params seed fields directly.
                    let mut fields: Vec<(String, Term)> = Vec::new();
                    for p in ctor.parameter_list.parameters.iter() {
                        if p.is_promoted_property() {
                            let name = strip_dollar(p.variable.name);
                            let term = param_terms.get(&name).cloned().ok_or_else(|| {
                                BailReason::Other("promoted param unbound".into())
                            })?;
                            set_field(
                                &mut fields,
                                String::from_utf8_lossy(&name).into_owned(),
                                term,
                            );
                        }
                    }

                    // A child TermEnv whose vars carry the params (no `$this` —
                    // the object is being built); used to evaluate the ctor body's
                    // pure `$this->x = <expr>` writes for the overlay.
                    let mut child = TermEnv {
                        vars: param_terms,
                        given_types: param_types,
                        project,
                        names: Some(names),
                        depth,
                    };
                    overlay_ctor_body(block, &mut child, &mut fields)?;

                    Ok(Term::Obj(record_class_for_closure.clone(), fields))
                })
                .unwrap_or_else(|| {
                    Err(BailReason::Other(
                        "could not re-parse constructor file".into(),
                    ))
                })
        });
        outcome
    }

    // ─── Step 7: `$recv->method(args)` pure accessor/transform → substitution ──

    /// Inline a PURE one-statement-`return` method by substitution. The receiver's
    /// class comes from the runtime [`Term::Obj`] tag (NEVER a static type): if the
    /// receiver does not reduce to an `Obj` with a known class tag → BAIL. The
    /// declaring method is resolved FQN-aware (mirrors [`super::subst`]'s
    /// `inline_method`); a non-single-`return` body BAILS (strict v1 purity).
    fn inline_pure_method(
        &mut self,
        receiver: Term,
        method: &[u8],
        arg_terms: Vec<Term>,
    ) -> Result<Term, BailReason> {
        // The receiver's class is its REDUCED Obj tag (runtime), not a static type.
        let reduced = super::term::reduce(&receiver);
        let Term::Obj(class, _) = &reduced else {
            return Err(BailReason::UnsupportedConstruct(
                "method call on a receiver that does not reduce to a constructed object".into(),
            ));
        };
        let class = class.clone();
        let codebase = self.project.codebase();
        let class_key = normalize_fqcn(class.as_bytes());
        let Some(meta) = codebase.get_declaring_method(&class_key, method) else {
            return Err(BailReason::UnknownCall(format!(
                "{}::{}",
                class,
                String::from_utf8_lossy(method)
            )));
        };
        if meta.method_metadata.as_ref().is_some_and(|m| m.is_abstract) {
            return Err(BailReason::UnsupportedConstruct(
                "abstract method dispatch".into(),
            ));
        }
        let declaring_fqcn = codebase
            .get_declaring_method_class(&class_key, method)
            .map(|w| w.as_bytes().to_vec())
            .unwrap_or_else(|| class_key.clone());
        let file = self
            .project
            .file_of_span(&meta.span)
            .ok_or_else(|| BailReason::Other("method's declaring file not loaded".into()))?;
        let logical = String::from_utf8_lossy(&file.name).into_owned();

        let project = self.project;
        let given_types = self.given_types.clone();
        let depth = self.depth;
        let method_owned = method.to_vec();
        Self::with_depth(depth, || {
            project
                .with_program(&logical, |program, _file, names| {
                    let m = find_class_method(program, &declaring_fqcn, &method_owned).ok_or_else(
                        || {
                            BailReason::UnknownCall(format!(
                                "{}::{}",
                                String::from_utf8_lossy(&declaring_fqcn),
                                String::from_utf8_lossy(&method_owned)
                            ))
                        },
                    )?;
                    let MethodBody::Concrete(block) = &m.body else {
                        return Err(BailReason::UnsupportedConstruct(
                            "abstract/interface method body".into(),
                        ));
                    };
                    // Strict v1 purity: EXACTLY one `return <expr>` statement, no
                    // other statements (a mutator / multi-statement body bails).
                    let ret_expr = single_return_expr(block).ok_or_else(|| {
                        BailReason::UnsupportedConstruct(
                            "method body is not a single `return <expr>;` (strict purity v1)"
                                .into(),
                        )
                    })?;
                    // Bind the params to the arg terms (coercion guard at typed
                    // scalar sinks), plus `$this` to the receiver Obj term.
                    let (mut bindings, param_types) =
                        bind_param_terms(&m.parameter_list, &arg_terms, &given_types)?;
                    bindings.insert(b"this".to_vec(), receiver.clone());
                    let mut child = TermEnv {
                        vars: bindings,
                        given_types: param_types,
                        project,
                        names: Some(names),
                        depth,
                    };
                    child.build(ret_expr)
                })
                .unwrap_or_else(|| Err(BailReason::Other("could not re-parse method file".into())))
        })
    }

    // ─── construct_obj guards (mirror super::subst's BridgeResolver helpers) ──

    fn bail_unresolvable_declared_ancestry(&self, start_key: &[u8]) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut current = normalize_fqcn(start_key).to_ascii_lowercase();
        loop {
            if seen.contains(&current) {
                return Ok(());
            }
            let Some(meta) = codebase.get_class_like(&current) else {
                return Ok(());
            };
            let Some(parent) = &meta.direct_parent_class else {
                return Ok(());
            };
            let parent_key = normalize_fqcn(parent.as_bytes()).to_ascii_lowercase();
            if codebase.get_class_like(&parent_key).is_none() {
                return Err(BailReason::UnsupportedConstruct(format!(
                    "class extends `{}`, a class not in the codebase (ext-internal state unmodelled)",
                    String::from_utf8_lossy(parent.as_bytes()),
                )));
            }
            seen.push(current);
            current = parent_key;
        }
    }

    fn bail_unresolvable_used_traits(&self, start_key: &[u8]) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let leaf_key = normalize_fqcn(start_key).to_ascii_lowercase();
        let Some(class_meta) = codebase.get_class_like(&leaf_key) else {
            return Ok(());
        };
        let mut chain: Vec<Vec<u8>> = class_meta
            .all_parent_classes
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        chain.push(leaf_key);
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for fqcn in &chain {
            self.bail_unresolvable_used_traits_of(fqcn, &mut seen)?;
        }
        Ok(())
    }

    fn bail_unresolvable_used_traits_of(
        &self,
        fqcn: &[u8],
        seen: &mut Vec<Vec<u8>>,
    ) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let lower = normalize_fqcn(fqcn).to_ascii_lowercase();
        if seen.contains(&lower) {
            return Ok(());
        }
        seen.push(lower.clone());
        let Some(meta) = codebase.get_class_like(&lower) else {
            return Ok(());
        };
        let traits: Vec<Vec<u8>> = meta
            .used_traits
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        for trait_fqcn in &traits {
            let trait_key = normalize_fqcn(trait_fqcn).to_ascii_lowercase();
            if codebase.get_class_like(&trait_key).is_none() {
                return Err(BailReason::UnsupportedConstruct(format!(
                    "class uses trait `{}`, a trait not in the codebase (trait state unmodelled)",
                    String::from_utf8_lossy(trait_fqcn),
                )));
            }
            self.bail_unresolvable_used_traits_of(trait_fqcn, seen)?;
        }
        Ok(())
    }

    fn bail_magic_method_in_chain(
        &self,
        start_key: &[u8],
        magic: &[u8],
        what: &str,
    ) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let leaf_key = normalize_fqcn(start_key).to_ascii_lowercase();
        let Some(class_meta) = codebase.get_class_like(&leaf_key) else {
            return Ok(());
        };
        let mut chain: Vec<Vec<u8>> = class_meta
            .all_parent_classes
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        chain.push(leaf_key);
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for fqcn in &chain {
            self.bail_magic_method_of(fqcn, magic, what, &mut seen)?;
        }
        Ok(())
    }

    fn bail_magic_method_of(
        &self,
        fqcn: &[u8],
        magic: &[u8],
        what: &str,
        seen: &mut Vec<Vec<u8>>,
    ) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let lower = normalize_fqcn(fqcn).to_ascii_lowercase();
        if seen.contains(&lower) {
            return Ok(());
        }
        seen.push(lower.clone());
        if codebase.get_declaring_method(&lower, magic).is_some() {
            return Err(BailReason::UnsupportedConstruct(format!(
                "`{}` declares {} ({} unmodelled)",
                String::from_utf8_lossy(fqcn),
                String::from_utf8_lossy(magic),
                what
            )));
        }
        let Some(meta) = codebase.get_class_like(&lower) else {
            return Ok(());
        };
        let traits: Vec<Vec<u8>> = meta
            .used_traits
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        for trait_fqcn in &traits {
            self.bail_magic_method_of(trait_fqcn, magic, what, seen)?;
        }
        Ok(())
    }

    // ─── Step 5: statement driver + assertion interception ────────────────────

    /// Walk the body's statements (calque [`super::eval`]'s `exec_stmt` but ONLY
    /// the pure forms): a `$var = <expr>` assignment binds the var; an assertion
    /// expression-statement is intercepted and decides. Any other statement BAILS.
    /// The final assertion's [`Decision`] is the result; a body with NO assertion
    /// (or a trailing non-assertion) → BAIL (incomplete — nothing to decide).
    fn run_body(&mut self, statements: &[Statement]) -> Result<Decision, BailReason> {
        let mut decision: Option<Decision> = None;
        for stmt in statements {
            match stmt {
                Statement::Expression(es) => {
                    if let Some(d) = self.try_assertion(es.expression)? {
                        decision = Some(d);
                        continue;
                    }
                    // A non-assertion expression statement: only a simple
                    // `$var = <expr>` assignment is modelled.
                    if let Expression::Assignment(a) = es.expression {
                        if !matches!(a.operator, AssignmentOperator::Assign(_)) {
                            return Err(BailReason::UnsupportedConstruct(
                                "compound assignment in test body".into(),
                            ));
                        }
                        let Expression::Variable(Variable::Direct(target)) = a.lhs else {
                            return Err(BailReason::UnsupportedConstruct(
                                "assignment to non-simple lvalue".into(),
                            ));
                        };
                        let key = var_name(target.name);
                        let rhs = self.build(a.rhs)?;
                        self.vars.insert(key, rhs);
                        continue;
                    }
                    return Err(BailReason::UnsupportedConstruct(
                        "non-assertion, non-assignment expression statement in test body".into(),
                    ));
                }
                other => {
                    return Err(BailReason::UnsupportedConstruct(format!(
                        "statement in test body: {}",
                        stmt_kind(other)
                    )))
                }
            }
        }
        decision.ok_or_else(|| {
            BailReason::UnsupportedConstruct("test body has no decidable assertion".into())
        })
    }

    /// If `expr` is a recognised assertion call (`$this->assertSame(...)` or a
    /// bare `assertSame(...)`), build its argument terms and decide. Returns
    /// `Ok(None)` when `expr` is not an assertion. Supports 2 or 3 args (the 3rd
    /// is an optional message, ignored).
    fn try_assertion(&mut self, expr: &Expression) -> Result<Option<Decision>, BailReason> {
        let Expression::Call(call) = expr else {
            return Ok(None);
        };
        let (name, args) = match call {
            Call::Method(m) => {
                let ClassLikeMemberSelector::Identifier(id) = &m.method else {
                    return Ok(None);
                };
                (id.value, &m.argument_list)
            }
            Call::Function(fc) => match identifier_name(fc.function) {
                Some(n) => (n, &fc.argument_list),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        if !is_bridge_assertion(name) {
            // An assertion the bridge does not model (or a normal method call):
            // for a method/function call shaped like an assertion but unknown,
            // fall through (the caller treats a non-`$this->assert*` call as a
            // normal expression; a genuinely unknown assertion bails there).
            return Ok(None);
        }
        let arg_exprs = positional_arg_exprs(args)?;
        Ok(Some(self.decide_assertion(name, &arg_exprs)?))
    }

    /// Route a recognised assertion to a kernel decision over the built terms.
    fn decide_assertion(
        &mut self,
        name: &[u8],
        args: &[&Expression],
    ) -> Result<Decision, BailReason> {
        // Build helpers (each builds a fresh term for the given arg index).
        let need = |n: usize| -> Result<(), BailReason> {
            if args.len() < n {
                Err(BailReason::UnsupportedConstruct(format!(
                    "assertion {} needs {} args, got {}",
                    String::from_utf8_lossy(name),
                    n,
                    args.len()
                )))
            } else {
                Ok(())
            }
        };
        let decision = match name {
            b"assertSame" => {
                need(2)?;
                let a = self.build(args[0])?;
                let b = self.build(args[1])?;
                decide_same(&a, &b)
            }
            b"assertNotSame" => {
                need(2)?;
                let a = self.build(args[0])?;
                let b = self.build(args[1])?;
                invert(decide_same(&a, &b))
            }
            b"assertEquals" => {
                need(2)?;
                let a = self.build(args[0])?;
                let b = self.build(args[1])?;
                decide_eq(&a, &b)
            }
            b"assertNotEquals" => {
                need(2)?;
                let a = self.build(args[0])?;
                let b = self.build(args[1])?;
                invert(decide_eq(&a, &b))
            }
            b"assertCount" => {
                need(2)?;
                let expected = self.build(args[0])?;
                let actual = self.build(args[1])?;
                decide_eq(&expected, &Term::Len(Box::new(actual)))
            }
            b"assertTrue" => {
                need(1)?;
                let x = self.build(args[0])?;
                decide_eq(&x, &Term::Bool(true))
            }
            b"assertFalse" => {
                need(1)?;
                let x = self.build(args[0])?;
                decide_eq(&x, &Term::Bool(false))
            }
            other => {
                return Err(BailReason::UnsupportedConstruct(format!(
                    "assertion {} not modelled by the term bridge",
                    String::from_utf8_lossy(other)
                )))
            }
        };
        Ok(decision)
    }
}

// ─── Free helpers ─────────────────────────────────────────────────────────────

/// Build a [`Term`] from a scalar literal. Float / Null have no Term variant →
/// BAIL (fail-closed); an integer overflow / unresolved string escape also bails.
fn build_literal(lit: &Literal) -> Result<Term, BailReason> {
    match lit {
        Literal::Integer(i) => match i.value {
            Some(v) => Ok(Term::Int(v as i64)),
            None => Err(BailReason::UnsupportedConstruct(
                "integer literal overflow (>i64)".into(),
            )),
        },
        Literal::String(s) => match &s.value {
            Some(v) => Ok(Term::Str(String::from_utf8_lossy(v).into_owned())),
            None => Err(BailReason::UnsupportedConstruct(
                "string literal with unresolved escapes".into(),
            )),
        },
        Literal::True(_) => Ok(Term::Bool(true)),
        Literal::False(_) => Ok(Term::Bool(false)),
        Literal::Float(_) => Err(BailReason::UnsupportedConstruct(
            "float literal (no Term variant)".into(),
        )),
        Literal::Null(_) => Err(BailReason::UnsupportedConstruct(
            "null literal (no Term variant)".into(),
        )),
    }
}

/// Strip a leading `$` from a variable token to get its bare name.
fn var_name(name: &[u8]) -> Vec<u8> {
    name.strip_prefix(b"$").unwrap_or(name).to_vec()
}

/// The bare name of a call/class target if it is a plain identifier, else `None`.
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

/// The whitelist of assertions the term bridge models (a subset of
/// [`super::eval`]'s `is_assertion_name`).
fn is_bridge_assertion(name: &[u8]) -> bool {
    matches!(
        name,
        b"assertSame"
            | b"assertNotSame"
            | b"assertEquals"
            | b"assertNotEquals"
            | b"assertCount"
            | b"assertTrue"
            | b"assertFalse"
    )
}

/// Invert a decision for the `Not*` assertions: True↔False, Unknown stays Unknown.
fn invert(d: Decision) -> Decision {
    match d {
        Decision::True => Decision::False,
        Decision::False => Decision::True,
        Decision::Unknown => Decision::Unknown,
    }
}

/// The single positional argument expression of a 1-arg call (`count($x)`); any
/// other shape (0, 2+, spread, named) BAILS.
fn single_positional_arg<'a>(args: &'a ArgumentList<'a>) -> Result<&'a Expression<'a>, BailReason> {
    let exprs = positional_arg_exprs(args)?;
    if exprs.len() != 1 {
        return Err(BailReason::UnsupportedConstruct(format!(
            "expected exactly 1 argument, got {}",
            exprs.len()
        )));
    }
    Ok(exprs[0])
}

/// The positional argument expressions of a call (no spread / named — those BAIL).
fn positional_arg_exprs<'a>(
    args: &'a ArgumentList<'a>,
) -> Result<Vec<&'a Expression<'a>>, BailReason> {
    let mut out = Vec::new();
    for arg in args.arguments.iter() {
        match arg {
            Argument::Positional(p) => {
                if p.ellipsis.is_some() {
                    return Err(BailReason::UnsupportedConstruct("argument spread".into()));
                }
                out.push(p.value);
            }
            Argument::Named(_) => {
                return Err(BailReason::UnsupportedConstruct("named argument".into()))
            }
        }
    }
    Ok(out)
}

/// Bind positional `args` to a parameter list, producing the name→Term map AND
/// the name→ScalarTy map (each param's scalar type, for the coercion guard
/// downstream). Variadic / by-ref params BAIL; too many args BAIL; a param with
/// no arg AND no default BAIL (defaults are not built — a literal default could
/// be modelled later, but v1 bails to stay fail-closed).
///
/// COERCION GUARD (step 6): a param with a bare-scalar hint is a typed scalar
/// sink. The arg term crossing it is checked: a CONCRETE scalar of a DIFFERENT
/// type, or a `Sym` whose Given type is unknown/incompatible, BAILS (coercion not
/// modelled). A `Sym` of the SAME scalar type (e.g. int→int) traverses unchanged.
fn bind_param_terms(
    param_list: &FunctionLikeParameterList,
    args: &[Term],
    given_types: &TypeBindings,
) -> Result<(TermBindings, TypeBindings), BailReason> {
    let params: Vec<&FunctionLikeParameter> = param_list.parameters.iter().collect();
    if args.len() > params.len() {
        return Err(BailReason::Other(
            "more arguments than method parameters (variadic call?)".into(),
        ));
    }
    let mut terms = HashMap::new();
    // Inherit the caller's Given types so a Sym carried INTO the callee (via an
    // arg term or the `$this` field state) stays typed inside the callee's scope.
    // Param names shadow on a clash, which is the correct PHP scoping.
    let mut types = given_types.clone();
    for (i, param) in params.iter().enumerate() {
        if param.ellipsis.is_some() {
            return Err(BailReason::UnsupportedConstruct(
                "variadic parameter".into(),
            ));
        }
        if param.ampersand.is_some() {
            return Err(BailReason::UnsupportedConstruct(
                "by-reference parameter".into(),
            ));
        }
        let key = strip_dollar(param.variable.name);
        let Some(arg_term) = args.get(i) else {
            return Err(BailReason::UnsupportedConstruct(
                "parameter has no argument (defaults not modelled in the term bridge)".into(),
            ));
        };
        // The param's declared scalar sink type (if any).
        let sink_ty = param.hint.as_ref().and_then(scalar_ty_of);
        // The arg term's OWN scalar type (for the guard + to thread the Given type
        // into the callee's given_types so a nested sink sees it).
        let arg_ty = scalar_ty_of_term(arg_term, given_types);
        if let Some(sink) = sink_ty {
            bail_if_term_coerces(arg_term, sink, arg_ty)?;
        }
        // Thread the resolved scalar type forward: prefer the arg's own type; else
        // (a non-scalar / unknown arg) the declared sink type narrows a free Sym.
        if let Some(ty) = arg_ty.or(sink_ty) {
            types.insert(key.clone(), ty);
        }
        terms.insert(key, arg_term.clone());
    }
    Ok((terms, types))
}

/// The scalar type of a term, if determinable. The term is REDUCED first so a
/// transparent `Field(Obj{f: g}, f)` collapses to its field term `g` before
/// classification (the whole point of the value-object transparency). A concrete
/// scalar reads off the variant; a `Sym` reads from `given_types`; an `Add/Sub/Mul`
/// over two Int-typed operands stays Int; anything else is `None` (unknown).
fn scalar_ty_of_term(t: &Term, given_types: &TypeBindings) -> Option<ScalarTy> {
    scalar_ty_of_reduced(&super::term::reduce(t), given_types)
}

fn scalar_ty_of_reduced(t: &Term, given_types: &TypeBindings) -> Option<ScalarTy> {
    match t {
        Term::Int(_) => Some(ScalarTy::Int),
        Term::Bool(_) => Some(ScalarTy::Bool),
        Term::Str(_) => Some(ScalarTy::Str),
        Term::Sym(name) => given_types.get(name.as_bytes()).copied(),
        // An arithmetic Bin over Int-typed operands stays Int; otherwise unknown.
        Term::Bin(_, a, b) => {
            let ta = scalar_ty_of_reduced(a, given_types);
            let tb = scalar_ty_of_reduced(b, given_types);
            match (ta, tb) {
                (Some(ScalarTy::Int), Some(ScalarTy::Int)) => Some(ScalarTy::Int),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The coercion guard at a typed scalar sink (step 6). `arg_ty` is the term's own
/// scalar type if known. A scalar value of the SAME type traverses; a CONCRETE
/// scalar of a different type, or an unknown/incompatible-typed Sym, BAILS.
fn bail_if_term_coerces(
    _arg: &Term,
    sink: ScalarTy,
    arg_ty: Option<ScalarTy>,
) -> Result<(), BailReason> {
    match arg_ty {
        // Same scalar type → no coercion (e.g. int Sym → int param).
        Some(ty) if ty == sink => Ok(()),
        // A KNOWN but DIFFERENT scalar type → a coercion PHP would perform that the
        // term kernel does not model → BAIL.
        Some(_) => Err(BailReason::UnsupportedConstruct(format!(
            "scalar parameter-type coercion (a {:?} value into a {:?} sink) not modelled",
            arg_ty, sink
        ))),
        // UNKNOWN type: a free Sym of an unknown Given type into a typed scalar
        // sink could coerce → BAIL (fail-closed). A concrete scalar always has a
        // known type, so this only fires for an untyped/opaque term.
        None => Err(BailReason::UnsupportedConstruct(format!(
            "value of unknown type into a {:?}-typed scalar sink (coercion not modelled)",
            sink
        ))),
    }
}

/// Overlay a constructor body's pure `$this->x = <expr>` writes onto `fields`
/// (last write wins). The body must consist ONLY of such property-assignment
/// statements (promoted params already seeded `fields`); ANY other statement
/// BAILS (fail-closed). An empty body is fine (promotion-only ctor).
fn overlay_ctor_body(
    block: &mago_syntax::ast::ast::block::Block,
    child: &mut TermEnv,
    fields: &mut Vec<(String, Term)>,
) -> Result<(), BailReason> {
    for stmt in block.statements.iter() {
        let Statement::Expression(es) = stmt else {
            return Err(BailReason::UnsupportedConstruct(
                "non-expression statement in constructor body (only `$this->x = <expr>;` modelled)"
                    .into(),
            ));
        };
        let Expression::Assignment(a) = es.expression else {
            return Err(BailReason::UnsupportedConstruct(
                "non-assignment statement in constructor body".into(),
            ));
        };
        if !matches!(a.operator, AssignmentOperator::Assign(_)) {
            return Err(BailReason::UnsupportedConstruct(
                "compound assignment in constructor body".into(),
            ));
        }
        // The lhs must be `$this->prop` with a static identifier selector.
        let Expression::Access(Access::Property(pa)) = a.lhs else {
            return Err(BailReason::UnsupportedConstruct(
                "constructor body writes a non-`$this->prop` lvalue".into(),
            ));
        };
        let Expression::Variable(Variable::Direct(recv)) = pa.object else {
            return Err(BailReason::UnsupportedConstruct(
                "constructor property write on a non-`$this` receiver".into(),
            ));
        };
        if var_name(recv.name) != b"this" {
            return Err(BailReason::UnsupportedConstruct(
                "constructor property write on a non-`$this` receiver".into(),
            ));
        }
        let ClassLikeMemberSelector::Identifier(prop_id) = &pa.property else {
            return Err(BailReason::UnsupportedConstruct(
                "dynamic property selector in constructor body".into(),
            ));
        };
        let value = child.build(a.rhs)?;
        set_field(
            fields,
            String::from_utf8_lossy(prop_id.value).into_owned(),
            value,
        );
    }
    Ok(())
}

/// Set-or-update a field (last write wins, keeping the first insertion order).
fn set_field(fields: &mut Vec<(String, Term)>, name: String, value: Term) {
    if let Some(slot) = fields.iter_mut().find(|(k, _)| *k == name) {
        slot.1 = value;
    } else {
        fields.push((name, value));
    }
}

/// If a block is EXACTLY one `return <expr>;` statement (and nothing else), the
/// returned expression; else `None` (the strict-purity-v1 gate for an inlined
/// accessor/transform method).
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

/// A short stable kind tag for an unsupported statement (error messages only).
fn stmt_kind(s: &Statement) -> &'static str {
    match s {
        Statement::Return(_) => "return",
        Statement::If(_) => "if",
        Statement::While(_) => "while",
        Statement::For(_) => "for",
        Statement::Foreach(_) => "foreach",
        Statement::Switch(_) => "switch",
        Statement::Try(_) => "try",
        Statement::Echo(_) => "echo",
        _ => "other",
    }
}

/// A short stable kind tag for an unsupported expression (error messages only).
fn expr_kind(e: &Expression) -> &'static str {
    match e {
        Expression::Conditional(_) => "conditional",
        Expression::Assignment(_) => "assignment",
        Expression::Closure(_) => "closure",
        Expression::ArrowFunction(_) => "arrow function",
        Expression::ArrayAccess(_) => "array access",
        Expression::UnaryPrefix(_) => "unary prefix",
        Expression::UnaryPostfix(_) => "unary postfix",
        _ => "other",
    }
}

// ─── Step 1 + 8: the public entry point ───────────────────────────────────────

/// Decide a test method's final assertion symbolically. Locates `class::method`'s
/// body, enters its parameters as FREE Givens (typed via their hints), walks the
/// pure body, and decides the final assertion → [`Decision`]. ANY bail anywhere
/// (an unmodelled construct, a failed inline, a coercion) collapses to
/// [`Decision::Unknown`] — the runner then executes the test for real.
pub fn decide_test(project: &MagoProject, class: &str, method: &str) -> Decision {
    match decide_test_inner(project, class, method) {
        Ok(d) => d,
        Err(_) => Decision::Unknown,
    }
}

fn decide_test_inner(
    project: &MagoProject,
    class: &str,
    method: &str,
) -> Result<Decision, BailReason> {
    let class_meta = project
        .find_class(class)
        .ok_or_else(|| BailReason::Other(format!("test class {class} not in codebase")))?;
    let file = project
        .file_of_span(&class_meta.span)
        .ok_or_else(|| BailReason::Other("test class's declaring file not loaded".into()))?;
    let logical = String::from_utf8_lossy(&file.name).into_owned();
    let class_fqcn = class_meta.name.as_bytes().to_vec();
    let depth = Cell::new(0u32);

    project
        .with_program(&logical, |program, _file, names| {
            let m =
                find_class_method(program, &class_fqcn, method.as_bytes()).ok_or_else(|| {
                    BailReason::Other(format!("test method {class}::{method} not found"))
                })?;
            let MethodBody::Concrete(block) = &m.body else {
                return Err(BailReason::UnsupportedConstruct(
                    "abstract/interface test method body".into(),
                ));
            };
            // Enter the test params as FREE Givens (Sym), recording their scalar
            // type from the hint (so the coercion guard at typed scalar sinks knows
            // each Given's type — e.g. `int $a` → Sym a : Int).
            let mut vars: TermBindings = HashMap::new();
            let mut given_types: TypeBindings = HashMap::new();
            for p in m.parameter_list.parameters.iter() {
                if p.ellipsis.is_some() || p.ampersand.is_some() {
                    return Err(BailReason::UnsupportedConstruct(
                        "variadic / by-ref test parameter".into(),
                    ));
                }
                let name = strip_dollar(p.variable.name);
                let sym = Term::Sym(String::from_utf8_lossy(&name).into_owned());
                if let Some(ty) = p.hint.as_ref().and_then(scalar_ty_of) {
                    given_types.insert(name.clone(), ty);
                }
                vars.insert(name, sym);
            }
            let mut env = TermEnv {
                vars,
                given_types,
                project,
                names: Some(names),
                depth: &depth,
            };
            env.run_body(block.statements.as_slice())
        })
        .unwrap_or_else(|| Err(BailReason::Other("could not re-parse test file".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `MagoProject` from a single PHP source in a tempdir (mirrors
    /// [`super::super::subst`]'s `reduce_with_subst` harness) and decide a test.
    fn decide(src: &str, class: &str, method: &str) -> Decision {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Code.php"), src).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        decide_test(&project, class, method)
    }

    /// The canonical value-object source used by several tests.
    const MONEY_SRC: &str = r#"<?php
use PHPUnit\Framework\TestCase;
class Money {
    public function __construct(public int $amount) {}
    public function plus(int $x): Money { return new Money($this->amount + $x); }
    public function getAmount(): int { return $this->amount; }
}
class MoneyTest extends TestCase {
    public function testPlus(int $a, int $b): void {
        $r = (new Money($a))->plus($b);
        $this->assertSame($a + $b, $r->getAmount());
    }
}
"#;

    // ── Step 1: the canonical oracle (red → green) ────────────────────────────

    #[test]
    fn money_plus_getamount_decides_true_for_all_givens() {
        // ∀ a, b — no concrete values: both sides reduce to `a + b`.
        assert_eq!(decide(MONEY_SRC, "MoneyTest", "testPlus"), Decision::True);
    }

    // ── Step 3: leaves ────────────────────────────────────────────────────────

    #[test]
    fn int_literal_leaf() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(): void { $this->assertSame(5, 5); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn distinct_int_literals_decide_false() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(): void { $this->assertSame(5, 6); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::False);
    }

    #[test]
    fn free_param_symbol_same_as_itself_is_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(int $a): void { $this->assertSame($a, $a); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn string_literal_leaf_decides() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(): void { $this->assertSame("hi", "hi"); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn bool_literal_leaf_decides() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(): void { $this->assertTrue(true); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn addition_is_commutative_decides_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(int $a, int $b): void { $this->assertSame($a + $b, $b + $a); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn unmodelled_division_operator_is_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(int $a): void { $this->assertSame($a, $a / 1); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::Unknown);
    }

    // ── Step 4: Field, count→Len, [..]→List ───────────────────────────────────

    #[test]
    fn field_access_on_fresh_object_substitutes() {
        // assertSame($a, (new Money($a))->getAmount()) — getAmount inlined →
        // $this->amount → the Obj field → $a.
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class Money {
    public function __construct(public int $amount) {}
    public function getAmount(): int { return $this->amount; }
}
class T extends TestCase {
    public function t(int $a): void { $this->assertSame($a, (new Money($a))->getAmount()); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn assert_count_over_list_literal_is_true() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(int $x, int $y): void { $this->assertCount(2, [$x, $y]); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    // ── Step 5: assignment binding + assertion routing ────────────────────────

    #[test]
    fn assignment_then_assert_equals_decides() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(int $a, int $b): void {
        $r = $a + $b;
        $this->assertEquals($b + $a, $r);
    }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn assert_not_same_inverts_to_false() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(): void { $this->assertNotSame(5, 5); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::False);
    }

    #[test]
    fn bare_assertion_function_form_decides() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
use function PHPUnit\Framework\assertSame;
class T extends TestCase {
    public function t(int $a): void { assertSame($a, $a); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn assertion_with_message_arg_decides() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(int $a): void { $this->assertSame($a, $a, "msg"); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    #[test]
    fn control_flow_statement_is_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class T extends TestCase {
    public function t(int $a): void { if ($a > 0) { $this->assertSame($a, $a); } }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::Unknown);
    }

    // ── Step 6: ctor body overlay + non-promoted ctor ─────────────────────────

    #[test]
    fn ctor_body_assignment_overlay_decides() {
        // A non-promoted ctor: the body writes $this->amount = $amount.
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class Money {
    public int $amount;
    public function __construct(int $amount) { $this->amount = $amount; }
    public function getAmount(): int { return $this->amount; }
}
class T extends TestCase {
    public function t(int $a): void { $this->assertSame($a, (new Money($a))->getAmount()); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::True);
    }

    // ── Step 8: guards (each → Unknown) ───────────────────────────────────────

    #[test]
    fn new_on_interface_is_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
interface Shape {}
class T extends TestCase {
    public function t(): void { $this->assertSame(1, (new Shape())->x); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::Unknown);
    }

    #[test]
    fn new_on_abstract_is_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
abstract class Base { public function __construct(public int $n) {} }
class T extends TestCase {
    public function t(int $a): void { $this->assertSame($a, (new Base($a))->n); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::Unknown);
    }

    #[test]
    fn mutator_method_body_is_unknown() {
        // A multi-statement (non single-return) method body bails the inline.
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class Box {
    public function __construct(public int $v) {}
    public function weird(int $x): int { $y = $x; return $this->v + $y; }
}
class T extends TestCase {
    public function t(int $a, int $b): void { $this->assertSame($a + $b, (new Box($a))->weird($b)); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::Unknown);
    }

    #[test]
    fn null_safe_method_call_is_unknown() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class Money {
    public function __construct(public int $amount) {}
    public function getAmount(): int { return $this->amount; }
}
class T extends TestCase {
    public function t(int $a): void { $this->assertSame($a, (new Money($a))?->getAmount()); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::Unknown);
    }

    #[test]
    fn assert_same_on_two_fresh_objects_is_unknown() {
        // assertSame(new Money(1), new Money(1)) — distinct handles in PHP; the
        // kernel's decide_same is fail-closed Unknown on any residual Obj.
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
class Money { public function __construct(public int $amount) {} }
class T extends TestCase {
    public function t(): void { $this->assertSame(new Money(1), new Money(1)); }
}
"#;
        assert_eq!(decide(src, "T", "t"), Decision::Unknown);
    }
}
