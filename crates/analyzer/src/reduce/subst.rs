//! User-function substitution via the mago bridge (spec §12.3).
//!
//! A test that calls an application/same-file user function reduces by INLINING
//! that function's body: bind the call's concrete arguments to the parameters,
//! evaluate the body natively ([`super::eval::run_body_returning`]), and use the
//! returned [`Value`] in place of the call. This is what makes the reducer a
//! partial evaluator over application code, not just an assertion checker.
//!
//! # Execution-model constraint (load-bearing)
//!
//! mago 1.30 keeps NO parsed AST around — [`MagoProject::with_program`] re-parses
//! the declaring file into a SCOPED arena that is dropped when the closure
//! returns. So the entire inlined evaluation (find the function, bind params, run
//! the body, and resolve any NESTED calls) happens INSIDE the closure: no AST node
//! ever escapes. Nested user calls re-enter `with_program` with their own arenas,
//! which nest correctly.
//!
//! # Fail-closed (spec §12.3)
//!
//! Bails on: an unknown callee (no metadata / no body found), an abstract body,
//! a variadic / by-reference parameter, a parameter with neither an argument nor
//! a computable default, too many arguments, or a recursion depth cap.

use std::cell::Cell;
use std::collections::HashMap;

use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
use mago_syntax::ast::ast::class_like::method::{Method, MethodBody};
use mago_syntax::ast::ast::function_like::function::Function;
use mago_syntax::ast::ast::function_like::parameter::FunctionLikeParameter;
use mago_syntax::ast::ast::statement::Statement;
use mago_syntax::ast::Program;

use super::eval::{
    bail_if_scalar_hint_coerces, bail_if_scalar_return_coerces, make_object,
    run_body_returning_with_names, run_ctor_body_with_names, run_method_with_this_writes,
    scalar_hint_of, BailReason, CallResolver, MethodDispatch, NoResolver, OwnedScalarHint, Scope,
};
use super::value::Value;
use crate::mago_bridge::MagoProject;

/// The receiver class's declared SCALAR property hints, keyed by bare property
/// name, carried OWNED from the arena-scoped class AST into the constructor/setUp
/// scope so a typed `$this->prop = …` write re-checks scalar coercion at the lazy
/// write site (round 3 Task A). A property absent here is untyped or non-scalar →
/// stored verbatim (no coercion, no bail).
type PropHints = HashMap<Vec<u8>, OwnedScalarHint>;

/// A [`CallResolver`] that inlines user functions resolved through a loaded
/// [`MagoProject`]. Holds a recursion-depth guard so a (mutually) recursive user
/// function bails instead of re-parsing unboundedly.
pub struct BridgeResolver<'p> {
    project: &'p MagoProject,
    depth: Cell<u32>,
    max_depth: u32,
}

impl<'p> BridgeResolver<'p> {
    pub fn new(project: &'p MagoProject) -> Self {
        Self {
            project,
            depth: Cell::new(0),
            // Deep call graphs bail rather than re-parsing forever (spec §12.3).
            max_depth: 64,
        }
    }
}

impl CallResolver for BridgeResolver<'_> {
    fn resolve_function(&self, name: &[u8], args: &[Value]) -> Result<Option<Value>, BailReason> {
        // Is it a user function mago knows about? If not, `Ok(None)` (the caller
        // then reports an unknown call).
        let Some(meta) = self.project.codebase().get_function(name) else {
            return Ok(None);
        };

        // Recursion guard.
        let d = self.depth.get();
        if d >= self.max_depth {
            return Err(BailReason::Other("recursion depth cap".into()));
        }
        self.depth.set(d + 1);
        let result = self.inline_function(meta, name, args);
        self.depth.set(d);
        result
    }

    fn resolve_instance_method(
        &self,
        this: &Value,
        method: &[u8],
        args: &[Value],
    ) -> Result<Option<MethodDispatch>, BailReason> {
        // The class comes from the RUNTIME receiver record (never a static type).
        let Value::Object { class, .. } = this else {
            return Ok(None);
        };
        self.with_depth(|s| s.inline_instance_method(class, method, this.clone(), args))
    }

    fn resolve_static_method(
        &self,
        class: &[u8],
        method: &[u8],
        args: &[Value],
    ) -> Result<Option<Value>, BailReason> {
        self.with_depth(|s| s.inline_method(class, method, None, args))
    }

    fn construct(&self, class: &[u8], args: &[Value]) -> Result<Option<Value>, BailReason> {
        self.with_depth(|s| s.construct_object(class, args))
    }

    fn resolve_class_constant(
        &self,
        class: &[u8],
        const_name: &[u8],
    ) -> Result<Option<Value>, BailReason> {
        self.fold_class_constant(class, const_name)
    }
}

impl BridgeResolver<'_> {
    /// Locate the declaring file for `meta`, re-parse it, find the function AST,
    /// bind params from `args` (+ defaults), and run the body to a [`Value`].
    fn inline_function(
        &self,
        meta: &mago_codex::metadata::function_like::FunctionLikeMetadata,
        name: &[u8],
        args: &[Value],
    ) -> Result<Option<Value>, BailReason> {
        let file = self
            .project
            .file_of_span(&meta.span)
            .ok_or_else(|| BailReason::Other("callee's declaring file not loaded".into()))?;
        let logical_name = String::from_utf8_lossy(&file.name).into_owned();

        // Everything below runs INSIDE the closure — the AST is arena-scoped.
        let outcome = self
            .project
            .with_program(&logical_name, |program, file, names| {
                let func = find_function(program, name)
                    .ok_or_else(|| BailReason::UnknownCall(String::from_utf8_lossy(name).into()))?;
                let bindings = bind_params(func, args)?;
                // Recurse through THIS resolver so nested user calls inline too;
                // names = the callee file's table (FQCN resolution for `new C`).
                // source = the callee file (so a closure RETURNED here owns its bytes).
                let ret = run_body_returning_with_names(
                    &func.body,
                    bindings,
                    self,
                    names,
                    &file.contents,
                )?;
                // PHP coerces the return to a declared scalar type; we don't model
                // that, so a mismatch bails (fail-closed) rather than returning the
                // un-coerced value.
                bail_if_scalar_return_coerces(func.return_type_hint.as_ref(), &ret)?;
                Ok(Some(ret))
            });

        match outcome {
            Some(r) => r,
            // `with_program` returned None → the file index lookup missed.
            None => Err(BailReason::Other("could not re-parse callee file".into())),
        }
    }

    /// Run `f` under the recursion-depth guard (shared by every inlining entry).
    /// Generic over the success type so it serves both the value-returning inlines
    /// (`Option<Value>`) and the instance-dispatch inline (`Option<MethodDispatch>`).
    fn with_depth<R>(
        &self,
        f: impl FnOnce(&Self) -> Result<Option<R>, BailReason>,
    ) -> Result<Option<R>, BailReason> {
        let d = self.depth.get();
        if d >= self.max_depth {
            return Err(BailReason::Other("recursion depth cap".into()));
        }
        self.depth.set(d + 1);
        let result = f(self);
        self.depth.set(d);
        result
    }

    /// Inline `class::method(args)` (with `$this` bound for an instance call).
    /// Resolution is FQN/namespace-aware (Task C CRITICAL): the DECLARING class is
    /// resolved through mago's inheritance-following `get_declaring_method_class`,
    /// then the body AST is located by matching the FULLY-QUALIFIED class name —
    /// never a bare simple-name match (which could bind the wrong body across
    /// namespaces, a silent divergence). Abstract bodies and mutators bail.
    fn inline_method(
        &self,
        class: &[u8],
        method: &[u8],
        this: Option<Value>,
        args: &[Value],
    ) -> Result<Option<Value>, BailReason> {
        let codebase = self.project.codebase();
        // Strip a leading `\` so a fully-qualified `\App\Calc` matches mago's key.
        let class = normalize_fqcn(class);
        let class = class.as_slice();
        // Resolve the concrete declaring method (follows inheritance/traits).
        let Some(meta) = codebase.get_declaring_method(class, method) else {
            return Ok(None);
        };
        // Abstract dispatch → bail (frontier §5).
        if meta.method_metadata.as_ref().is_some_and(|m| m.is_abstract) {
            return Err(BailReason::UnsupportedConstruct(
                "abstract method dispatch".into(),
            ));
        }
        // The exact FQCN of the class that DECLARES the body (not the receiver's).
        let declaring_fqcn = codebase
            .get_declaring_method_class(class, method)
            .map(|w| w.as_bytes().to_vec())
            .unwrap_or_else(|| class.to_vec());

        let file = self
            .project
            .file_of_span(&meta.span)
            .ok_or_else(|| BailReason::Other("method's declaring file not loaded".into()))?;
        let logical_name = String::from_utf8_lossy(&file.name).into_owned();

        let outcome = self
            .project
            .with_program(&logical_name, |program, file, names| {
                let m = find_class_method(program, &declaring_fqcn, method).ok_or_else(|| {
                    BailReason::UnknownCall(format!(
                        "{}::{}",
                        String::from_utf8_lossy(&declaring_fqcn),
                        String::from_utf8_lossy(method)
                    ))
                })?;
                let MethodBody::Concrete(block) = &m.body else {
                    return Err(BailReason::UnsupportedConstruct(
                        "abstract/interface method body".into(),
                    ));
                };
                let mut bindings = bind_method_params(&m.parameter_list, args)?;
                if let Some(this_val) = this {
                    bindings.insert(b"this".to_vec(), this_val);
                }
                // A mutator (`$this->prop = ...`) in a non-constructor body bails
                // automatically: `run_body_returning` does NOT enable property
                // writes, so the assignment handler rejects it (frontier §2).
                // names = the declaring file's table (resolves `new C` in the body).
                // source = the declaring file (so a closure returned here owns its bytes).
                let ret =
                    run_body_returning_with_names(block, bindings, self, names, &file.contents)?;
                // PHP coerces the return to a declared scalar type; a mismatch bails.
                bail_if_scalar_return_coerces(m.return_type_hint.as_ref(), &ret)?;
                Ok(Some(ret))
            });
        outcome.unwrap_or_else(|| Err(BailReason::Other("could not re-parse method file".into())))
    }

    /// Inline an INSTANCE method `$this->m(args)` (Inc-5 Task 3), allowing
    /// `$this->prop = …` writes, and report BOTH the return value and whether the
    /// body mutated `$this` (so the eval-layer dispatch can write the mutation back
    /// to a uniquely-owned `$var` receiver, or BAIL on an aliased/non-assignable
    /// one). Resolution mirrors [`BridgeResolver::inline_method`] (FQN-aware via
    /// `get_declaring_method_class`); abstract/interface bodies bail.
    fn inline_instance_method(
        &self,
        class: &[u8],
        method: &[u8],
        this: Value,
        args: &[Value],
    ) -> Result<Option<MethodDispatch>, BailReason> {
        let codebase = self.project.codebase();
        let class = normalize_fqcn(class);
        let class = class.as_slice();
        let Some(meta) = codebase.get_declaring_method(class, method) else {
            return Ok(None);
        };
        if meta.method_metadata.as_ref().is_some_and(|m| m.is_abstract) {
            return Err(BailReason::UnsupportedConstruct(
                "abstract method dispatch".into(),
            ));
        }
        let declaring_fqcn = codebase
            .get_declaring_method_class(class, method)
            .map(|w| w.as_bytes().to_vec())
            .unwrap_or_else(|| class.to_vec());

        // The receiver class's declared scalar property hints, across its full
        // ancestor chain + traits — so a typed `$this->prop = …` write in the body
        // re-checks scalar coercion (round 3 Task A); symmetric with construct.
        let prop_hints = self.collect_instance_prop_hints(class);

        // The pre-body `$this` props, to diff against after the body runs (mutation
        // detection). The class is invariant, so comparing props suffices.
        let pre_props = match &this {
            Value::Object { props, .. } => props.clone(),
            _ => {
                return Err(BailReason::Other(
                    "instance dispatch receiver is not an object".into(),
                ))
            }
        };

        let file = self
            .project
            .file_of_span(&meta.span)
            .ok_or_else(|| BailReason::Other("method's declaring file not loaded".into()))?;
        let logical_name = String::from_utf8_lossy(&file.name).into_owned();

        let outcome = self
            .project
            .with_program(&logical_name, |program, file, names| {
                let m = find_class_method(program, &declaring_fqcn, method).ok_or_else(|| {
                    BailReason::UnknownCall(format!(
                        "{}::{}",
                        String::from_utf8_lossy(&declaring_fqcn),
                        String::from_utf8_lossy(method)
                    ))
                })?;
                let MethodBody::Concrete(block) = &m.body else {
                    return Err(BailReason::UnsupportedConstruct(
                        "abstract/interface method body".into(),
                    ));
                };
                let mut bindings = bind_method_params(&m.parameter_list, args)?;
                bindings.insert(b"this".to_vec(), this.clone());
                let (ret, final_this) = run_method_with_this_writes(
                    block,
                    bindings,
                    self,
                    names,
                    &file.contents,
                    prop_hints.clone(),
                )?;
                bail_if_scalar_return_coerces(m.return_type_hint.as_ref(), &ret)?;

                // Did the body write to `$this`? Compare the post-body props to the
                // pre-body props (class is invariant; `Value`'s PartialEq ignores the
                // aliasing flag, so only real value changes count as a mutation).
                let mutated = match &final_this {
                    Value::Object { props, .. } => props != &pre_props,
                    _ => {
                        return Err(BailReason::Other(
                            "instance method lost its object \\$this".into(),
                        ))
                    }
                };
                Ok(Some(MethodDispatch {
                    ret,
                    mutated_this: if mutated { Some(final_this) } else { None },
                }))
            });
        outcome.unwrap_or_else(|| Err(BailReason::Other("could not re-parse method file".into())))
    }

    /// Collect the receiver class's declared SCALAR property hints across its full
    /// parent chain + used traits (parents-first so a child override wins), keyed by
    /// bare property name. Mirrors the hint-collection in [`BridgeResolver::construct_object`]
    /// so a typed `$this->prop = …` write in an instance method re-checks coercion.
    fn collect_instance_prop_hints(&self, class: &[u8]) -> PropHints {
        let codebase = self.project.codebase();
        let Some(class_meta) = codebase.get_class_like(class) else {
            return PropHints::new();
        };
        let mut hint_chain: Vec<Vec<u8>> = class_meta
            .all_parent_classes
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        hint_chain.reverse();
        hint_chain.push(class_meta.name.as_bytes().to_vec());
        let mut prop_hints = PropHints::new();
        for fqcn in &hint_chain {
            let mut seen: Vec<Vec<u8>> = Vec::new();
            self.collect_trait_scalar_property_hints(fqcn, &mut prop_hints, &mut seen);
            self.collect_class_scalar_property_hints(fqcn, &mut prop_hints);
        }
        prop_hints
    }

    /// Fold a class constant `Class::CONST` (Inc-5 Task 4) to a literal [`Value`].
    /// Looks the constant up on the class then its parent classes/interfaces
    /// (most-derived first, matching PHP const resolution), re-parses the declaring
    /// class AST, finds the `const CONST = <init>` item, and evaluates its
    /// initializer with [`NoResolver`] (LITERAL / already-modelled expressions only;
    /// a computed/call/`new`/unresolved initializer BAILS via `eval_default`). An
    /// enum bails (enum cases are deferred). `Ok(None)` → class not in the codebase
    /// OR constant not found (caller bails with an UnknownCall).
    fn fold_class_constant(
        &self,
        class: &[u8],
        const_name: &[u8],
    ) -> Result<Option<Value>, BailReason> {
        let codebase = self.project.codebase();
        let class = normalize_fqcn(class);
        let class = class.as_slice();
        let Some(class_meta) = codebase.get_class_like(class) else {
            return Ok(None);
        };
        // Enum constant/case access is deferred (frontier) — fail-closed.
        if class_meta.kind.is_enum() {
            return Err(BailReason::UnsupportedConstruct(
                "enum constant/case access (deferred)".into(),
            ));
        }

        // Lookup chain: the class itself, then its parent classes, then parent
        // interfaces (interface consts are inherited). Most-derived first.
        let mut chain: Vec<Vec<u8>> = vec![class_meta.name.as_bytes().to_vec()];
        chain.extend(
            class_meta
                .all_parent_classes
                .iter()
                .map(|w| w.as_bytes().to_vec()),
        );
        chain.extend(
            class_meta
                .all_parent_interfaces
                .iter()
                .map(|w| w.as_bytes().to_vec()),
        );

        for fqcn in &chain {
            // The enum check above only covered the entry class; a parent enum is
            // impossible (you cannot extend an enum), so no re-check is needed.
            if let Some(value) = self.fold_constant_in_class(fqcn, const_name)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    /// Look up `const_name` in the single class `fqcn`'s OWN AST and fold its
    /// literal initializer. `Ok(None)` → the class declares no such constant (try
    /// the next in the chain). A typed constant re-checks scalar coercion.
    fn fold_constant_in_class(
        &self,
        fqcn: &[u8],
        const_name: &[u8],
    ) -> Result<Option<Value>, BailReason> {
        let codebase = self.project.codebase();
        let Some(meta) = codebase.get_class_like(fqcn) else {
            return Ok(None);
        };
        let Some(file) = self.project.file_of_span(&meta.span) else {
            return Ok(None);
        };
        let logical = String::from_utf8_lossy(&file.name).into_owned();
        let class_fqcn = meta.name.as_bytes().to_vec();
        self.project
            .with_program(&logical, |program, _file, _names| {
                let Some(class_node) = find_class(program, &class_fqcn) else {
                    return Ok(None);
                };
                find_class_constant_value(class_node, const_name)
            })
            .unwrap_or(Ok(None))
    }

    /// Construct `new class(args)` (Task B): seed props from plain literal property
    /// defaults + promoted params, then run the constructor body (property writes
    /// permitted) and return the populated record. The constructor may be inherited
    /// (resolved via its own declaring class), so it is run in its declaring file.
    fn construct_object(&self, class: &[u8], args: &[Value]) -> Result<Option<Value>, BailReason> {
        let codebase = self.project.codebase();
        // Strip a leading `\` so a fully-qualified `\App\Calc` matches mago's key.
        let class = normalize_fqcn(class);
        let class = class.as_slice();
        // The original-cased FQCN for the record's `class` tag (so object `==`
        // compares the real class name, not a lowercased key).
        let class_meta = match codebase.get_class_like(class) {
            Some(m) => m,
            None => return Ok(None),
        };
        let record_class = class_meta.original_name.as_bytes().to_vec();
        let class_file = self
            .project
            .file_of_span(&class_meta.span)
            .ok_or_else(|| BailReason::Other("class declaring file not loaded".into()))?;
        let class_logical = String::from_utf8_lossy(&class_file.name).into_owned();
        let class_fqcn = class_meta.name.as_bytes().to_vec();

        // 1) Seed plain (non-promoted) property declarations with literal defaults
        //    from THIS class's own AST. Inherited-property defaults are intentionally
        //    NOT seeded (seed_plain_property_defaults is leaf-only) — a read of an
        //    unseeded inherited prop bails, fail-closed (no divergence).
        let resolver = self;
        let mut props: Vec<(Vec<u8>, Value)> = self
            .project
            .with_program(&class_logical, |program, _file, _names| {
                let Some(class_node) = find_class(program, &class_fqcn) else {
                    return Err(BailReason::Other(
                        "class AST not found after re-parse".into(),
                    ));
                };
                let mut p: Vec<(Vec<u8>, Value)> = Vec::new();
                seed_plain_property_defaults(class_node, &mut p, resolver)?;
                Ok(p)
            })
            .unwrap_or_else(|| Err(BailReason::Other("could not re-parse class file".into())))?;

        // Collect the declared SCALAR property hints (round 3 Task A) across the
        // FULL ancestor chain — symmetric with build_test_case_this — so a typed
        // `$this->prop = …` write in the ctor body re-checks coercion even when the
        // property is DECLARED IN A PARENT. (Leaf-only collection missed inherited
        // hints, letting an un-coerced value through → a divergent Pass/Fail.)
        // Walk parents-first so a child override wins over a parent on a dup name.
        let mut hint_chain: Vec<Vec<u8>> = class_meta
            .all_parent_classes
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        hint_chain.reverse();
        hint_chain.push(class_meta.name.as_bytes().to_vec());
        let mut prop_hints = PropHints::new();
        for fqcn in &hint_chain {
            // A class's used traits BEFORE its own hints (round 5: trait-declared
            // typed scalar props are a SEPARATE set, never in all_parent_classes),
            // so a class-level redeclare wins over a trait's hint.
            let mut seen: Vec<Vec<u8>> = Vec::new();
            self.collect_trait_scalar_property_hints(fqcn, &mut prop_hints, &mut seen);
            self.collect_class_scalar_property_hints(fqcn, &mut prop_hints);
        }

        // 2) Run the constructor (promoted-param seeding + body). Resolved through
        //    the DECLARING class so an inherited constructor inlines correctly.
        let ctor_meta = codebase.get_declaring_method(class, b"__construct");
        match ctor_meta {
            Some(meta) => {
                if meta.method_metadata.as_ref().is_some_and(|m| m.is_abstract) {
                    return Err(BailReason::UnsupportedConstruct(
                        "abstract constructor".into(),
                    ));
                }
                let ctor_class = codebase
                    .get_declaring_method_class(class, b"__construct")
                    .map(|w| w.as_bytes().to_vec())
                    .unwrap_or_else(|| class_fqcn.clone());
                let ctor_file = self
                    .project
                    .file_of_span(&meta.span)
                    .ok_or_else(|| BailReason::Other("constructor file not loaded".into()))?;
                let ctor_logical = String::from_utf8_lossy(&ctor_file.name).into_owned();

                let this = make_object(record_class.clone(), props);
                let built = self
                    .project
                    .with_program(&ctor_logical, |program, file, names| {
                        let ctor = find_class_method(program, &ctor_class, b"__construct")
                            .ok_or_else(|| {
                                BailReason::Other("constructor AST not found after re-parse".into())
                            })?;
                        run_constructor(
                            resolver,
                            ctor,
                            this.clone(),
                            args,
                            names,
                            &file.contents,
                            prop_hints.clone(),
                        )
                    })
                    .unwrap_or_else(|| {
                        Err(BailReason::Other(
                            "could not re-parse constructor file".into(),
                        ))
                    })?;
                Ok(Some(built))
            }
            None => {
                if !args.is_empty() {
                    // No constructor but args were passed → PHP would error.
                    return Err(BailReason::TypeError(
                        "arguments passed to a class with no constructor".into(),
                    ));
                }
                Ok(Some(make_object(record_class, std::mem::take(&mut props))))
            }
        }
    }

    /// Build the TEST-CASE `$this` record (Inc-3 Tasks A+B): a `Value::Object`
    /// whose `class` is the test-case FQCN, seeded with the class chain's plain
    /// literal property defaults, then run through `setUp()` (the seeding phase,
    /// property writes permitted) if one is declared anywhere up the parent chain.
    ///
    /// Fail-closed: a `setUp` that hits any unmodelled/impure construct propagates
    /// its bail (incomplete Givens → bail the whole test). `setUpBeforeClass` is
    /// the DRIVER's concern (deferred → bail there); this only runs `setUp`.
    ///
    /// `Ok(None)` when the class is not in the codebase (the driver then bails).
    pub fn build_test_case_this(&self, class: &str) -> Result<Option<Value>, BailReason> {
        let codebase = self.project.codebase();
        let class_key = class.to_lowercase();
        let Some(class_meta) = codebase.get_class_like(class_key.as_bytes()) else {
            return Ok(None);
        };
        let record_class = class_meta.original_name.as_bytes().to_vec();

        // 1) Seed plain literal property defaults from the class AND every ancestor
        //    (a test-case fixture property may be declared in a parent TestCase).
        //    Each ancestor is seeded from its OWN class AST; a child default
        //    overrides a parent's (so seed parents first, then the class itself).
        let mut props: Vec<(Vec<u8>, Value)> = Vec::new();
        let mut chain: Vec<Vec<u8>> = class_meta
            .all_parent_classes
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        // Parents first, then the class itself (child wins on a duplicate prop).
        chain.reverse();
        chain.push(class_meta.name.as_bytes().to_vec());
        // Collect the chain's declared scalar property hints too (round 3 Task A),
        // so a typed `$this->prop = …` write inside setUp re-checks coercion.
        let mut prop_hints = PropHints::new();
        for fqcn in &chain {
            self.seed_class_property_defaults(fqcn, &mut props)?;
            // A class's used traits BEFORE its own hints (round 5: trait-declared
            // typed scalar props are a SEPARATE set, never in all_parent_classes),
            // so a class-level redeclare wins over a trait's hint.
            let mut seen: Vec<Vec<u8>> = Vec::new();
            self.collect_trait_scalar_property_hints(fqcn, &mut prop_hints, &mut seen);
            self.collect_class_scalar_property_hints(fqcn, &mut prop_hints);
        }

        // 2) Run setUp() if it is declared anywhere up the chain (writes allowed).
        let mut this = make_object(record_class, props);
        if let Some(meta) = codebase.get_declaring_method(class_key.as_bytes(), b"setup") {
            if meta.method_metadata.as_ref().is_some_and(|m| m.is_abstract) {
                return Err(BailReason::UnsupportedConstruct("abstract setUp".into()));
            }
            let setup_class = codebase
                .get_declaring_method_class(class_key.as_bytes(), b"setup")
                .map(|w| w.as_bytes().to_vec())
                .unwrap_or_else(|| class_meta.name.as_bytes().to_vec());
            let file = self
                .project
                .file_of_span(&meta.span)
                .ok_or_else(|| BailReason::Other("setUp declaring file not loaded".into()))?;
            let logical = String::from_utf8_lossy(&file.name).into_owned();
            let resolver = self;
            this =
                self.project
                    .with_program(&logical, |program, file, names| {
                        let m = find_class_method(program, &setup_class, b"setUp").ok_or_else(
                            || BailReason::Other("setUp AST not found after re-parse".into()),
                        )?;
                        let MethodBody::Concrete(block) = &m.body else {
                            return Err(BailReason::UnsupportedConstruct(
                                "abstract/interface setUp body".into(),
                            ));
                        };
                        // setUp takes no positional args; bind only `$this`.
                        let mut bindings: HashMap<Vec<u8>, Value> = HashMap::new();
                        bindings.insert(b"this".to_vec(), this.clone());
                        // source = the setUp file (so a closure stored into $this owns its bytes).
                        super::eval::run_ctor_body_with_names(
                            block,
                            bindings,
                            resolver,
                            names,
                            &file.contents,
                            prop_hints.clone(),
                        )
                    })
                    .unwrap_or_else(|| {
                        Err(BailReason::Other("could not re-parse setUp file".into()))
                    })?;
        }
        Ok(Some(this))
    }

    /// Seed one class's OWN plain literal property defaults into `props` (used by
    /// the test-case `$this` builder to walk the ancestor chain). A class not on
    /// disk is silently skipped (its props stay unseeded → a read bails later).
    fn seed_class_property_defaults(
        &self,
        fqcn: &[u8],
        props: &mut Vec<(Vec<u8>, Value)>,
    ) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let key = normalize_fqcn(fqcn);
        let Some(class_meta) = codebase.get_class_like(&key.to_ascii_lowercase()) else {
            return Ok(());
        };
        let Some(file) = self.project.file_of_span(&class_meta.span) else {
            return Ok(());
        };
        let logical = String::from_utf8_lossy(&file.name).into_owned();
        let class_fqcn = class_meta.name.as_bytes().to_vec();
        self.project
            .with_program(&logical, |program, _file, _names| {
                let Some(class_node) = find_class(program, &class_fqcn) else {
                    return Ok(());
                };
                seed_plain_property_defaults_tolerant(class_node, props)
            })
            .unwrap_or(Ok(()))
    }

    /// Collect one class's OWN declared scalar property hints into `hints` (round 3
    /// Task A; used by the test-case `$this` builder to walk the ancestor chain so a
    /// typed `$this->prop = …` write in setUp re-checks coercion). A child class's
    /// hint overrides a parent's on a duplicate name (the chain is walked
    /// parents-first). A class not on disk is silently skipped.
    fn collect_class_scalar_property_hints(&self, fqcn: &[u8], hints: &mut PropHints) {
        let codebase = self.project.codebase();
        let key = normalize_fqcn(fqcn);
        let Some(class_meta) = codebase.get_class_like(&key.to_ascii_lowercase()) else {
            return;
        };
        let Some(file) = self.project.file_of_span(&class_meta.span) else {
            return;
        };
        let logical = String::from_utf8_lossy(&file.name).into_owned();
        let class_fqcn = class_meta.name.as_bytes().to_vec();
        let _ = self
            .project
            .with_program(&logical, |program, _file, _names| {
                // A class-like is either a `class` or a `trait` AST node; both carry
                // the same `members` shape, so the same `&Hint` collection applies.
                if let Some(class_node) = find_class(program, &class_fqcn) {
                    hints.extend(collect_scalar_property_hints(class_node.members.iter()));
                } else if let Some(trait_node) = find_trait(program, &class_fqcn) {
                    hints.extend(collect_scalar_property_hints(trait_node.members.iter()));
                }
            });
    }

    /// Collect a class's USED-TRAIT scalar property hints into `hints` (round 5),
    /// recursing into traits a trait itself `use`s, so a `$this->prop = …` write to
    /// a TRAIT-declared typed scalar property re-checks coercion. Traits are a
    /// SEPARATE set in mago (`used_traits`, never an `all_parent_classes` member),
    /// so the ancestor-chain walk alone missed them — a coercing write to a
    /// trait-declared typed scalar property escaped the guard and was stored
    /// verbatim (a 0-divergence break). Each trait's OWN hints are collected via the
    /// same AST `&Hint` path as a class, so the verified coercion predicate is
    /// reused byte-for-byte. A used trait is collected BEFORE the using class's own
    /// hints by the caller, so a class-level redeclare wins over a trait's hint.
    /// `seen` guards against a (malformed) trait-use cycle. A trait not on disk is
    /// silently skipped (its props stay unhinted → stored verbatim, fail-closed).
    fn collect_trait_scalar_property_hints(
        &self,
        fqcn: &[u8],
        hints: &mut PropHints,
        seen: &mut Vec<Vec<u8>>,
    ) {
        let codebase = self.project.codebase();
        let key = normalize_fqcn(fqcn);
        let lower = key.to_ascii_lowercase();
        if seen.iter().any(|s| s == &lower) {
            return;
        }
        seen.push(lower.clone());
        let Some(class_meta) = codebase.get_class_like(&lower) else {
            return;
        };
        // Snapshot the used-trait FQCNs (the borrow of `class_meta` ends here so the
        // recursive call can re-borrow the codebase).
        let traits: Vec<Vec<u8>> = class_meta
            .used_traits
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        for trait_fqcn in &traits {
            // A trait can `use` another trait: recurse first (nested traits before
            // the using trait), then collect the trait's own declared hints so a
            // trait that redeclares a nested-trait prop wins.
            self.collect_trait_scalar_property_hints(trait_fqcn, hints, seen);
            self.collect_class_scalar_property_hints(trait_fqcn, hints);
        }
    }
}

/// Seed plain literal property defaults for a TEST-CASE ancestor class, TOLERATING
/// (skipping) static / hooked / non-literal-default properties instead of bailing.
///
/// Rationale (frontier, fail-closed-preserving): a base PHPUnit `TestCase` carries
/// static props and the test-case chain may use property hooks the reducer cannot
/// model — but those are not part of the modelled INSTANCE record. Skipping them
/// here is still sound: a test that actually READS an unseeded instance property
/// bails at the read site (`eval_access` → "read of unset property"). Only a plain,
/// non-static property carrying a LITERAL default is seeded.
fn seed_plain_property_defaults_tolerant(
    class_node: &mago_syntax::ast::ast::class_like::Class,
    props: &mut Vec<(Vec<u8>, Value)>,
) -> Result<(), BailReason> {
    use mago_syntax::ast::ast::class_like::property::{Property, PropertyItem};
    use mago_syntax::ast::ast::modifier::Modifier;

    for member in class_node.members.iter() {
        let ClassLikeMember::Property(Property::Plain(plain)) = member else {
            continue; // hooked properties / non-properties: skip (tolerant).
        };
        // Static properties are not part of an instance record → skip.
        if plain
            .modifiers
            .iter()
            .any(|m| matches!(m, Modifier::Static(_)))
        {
            continue;
        }
        for item in plain.items.iter() {
            let PropertyItem::Concrete(c) = item else {
                continue; // no default → leave unset (read-before-init bails later).
            };
            // A non-literal default (call, new, …) is skipped here, not bailed: the
            // prop stays unseeded and a later read bails fail-closed.
            let mut scope = Scope::new(HashMap::new(), &NoResolver);
            if let Ok(v) = super::eval::eval_default(c.value, &mut scope) {
                // A typed scalar property would coerce its default in PHP; storing
                // the un-coerced value diverges. Tolerant fail-closed: SKIP seeding
                // (leave unset → a later read bails) rather than store a wrong value.
                if let Some(hint) = &plain.hint {
                    if bail_if_scalar_hint_coerces(hint, &v, "property default").is_err() {
                        continue;
                    }
                }
                set_prop(props, strip_dollar(c.variable.name), v);
            }
        }
    }
    Ok(())
}

/// Run a constructor AST over a fresh `$this` record: seed promoted params, bind
/// plain params, then run the body with property writes enabled. Returns the
/// populated record.
fn run_constructor(
    resolver: &BridgeResolver,
    ctor: &Method,
    this: Value,
    args: &[Value],
    names: &mago_names::ResolvedNames,
    source: &[u8],
    mut prop_hints: PropHints,
) -> Result<Value, BailReason> {
    let Value::Object {
        class, mut props, ..
    } = this
    else {
        return Err(BailReason::Other(
            "constructor $this is not an object".into(),
        ));
    };

    let params: Vec<&FunctionLikeParameter> = ctor.parameter_list.parameters.iter().collect();
    if args.len() > params.len() {
        return Err(BailReason::Other(
            "more constructor arguments than parameters (variadic?)".into(),
        ));
    }

    let mut bindings: HashMap<Vec<u8>, Value> = HashMap::new();
    for (i, param) in params.iter().enumerate() {
        if param.ellipsis.is_some() {
            return Err(BailReason::UnsupportedConstruct(
                "variadic constructor parameter".into(),
            ));
        }
        if param.ampersand.is_some() {
            return Err(BailReason::UnsupportedConstruct(
                "by-reference constructor parameter".into(),
            ));
        }
        let bare = strip_dollar(param.variable.name);
        let value = match args.get(i) {
            Some(a) => a.clone(),
            None => match &param.default_value {
                Some(default) => {
                    let mut scope = Scope::new(HashMap::new(), &NoResolver);
                    super::eval::eval_default(default.value, &mut scope)?
                }
                None => {
                    return Err(BailReason::Other(
                        "constructor parameter has no argument and no default".into(),
                    ))
                }
            },
        };
        bail_if_scalar_param_coerces(param, &value)?;
        // A promoted property seeds `$this->name` directly (PHP semantics). Its
        // scalar coercion is already enforced by `bail_if_scalar_param_coerces`
        // above (the promoted-param IS the property), but record its hint too so a
        // LATER `$this->name = …` write in the body is re-checked (round 3 Task A).
        if param.is_promoted_property() {
            if has_readonly_modifier(param) {
                // readonly is fine for a write-once seed at construction time, so
                // we allow it here; any LATER mutation of the prop bails because
                // mutator methods bail. (No special handling needed.)
            }
            if let Some(hint) = &param.hint {
                if let Some(sh) = super::eval::scalar_hint_of(hint) {
                    prop_hints.insert(bare.clone(), sh);
                }
            }
            set_prop(&mut props, bare.clone(), value.clone());
        }
        bindings.insert(bare, value);
    }
    bindings.insert(b"this".to_vec(), make_object(class, props));

    let MethodBody::Concrete(block) = &ctor.body else {
        return Err(BailReason::UnsupportedConstruct(
            "abstract constructor body".into(),
        ));
    };
    run_ctor_body_with_names(block, bindings, resolver, names, source, prop_hints)
}

/// Collect a class's declared SCALAR property hints (round 3 Task A), keyed by
/// bare property name. Reads plain (non-promoted) property declarations off THIS
/// class's AST; promoted-constructor-param hints are merged in `run_constructor`.
/// Only coercion-relevant scalar hints are recorded (`scalar_hint_of` returns
/// `None` for untyped / non-scalar hints — those are stored verbatim, never bail).
/// Static/hooked properties are skipped (the seeding path bails on them anyway).
/// Collect the declared scalar property hints from a class-like's MEMBERS. Taking
/// the member sequence (not a `&Class`) lets the SAME `&Hint`-based collection serve
/// both a `class`/`trait` AST node (round 5: a trait's typed scalar props need the
/// identical verified coercion classification as a class's).
fn collect_scalar_property_hints<'a>(
    members: impl Iterator<Item = &'a ClassLikeMember<'a>>,
) -> PropHints {
    use mago_syntax::ast::ast::class_like::property::Property;
    use mago_syntax::ast::ast::modifier::Modifier;

    let mut hints = PropHints::new();
    for member in members {
        let ClassLikeMember::Property(Property::Plain(plain)) = member else {
            continue;
        };
        if plain
            .modifiers
            .iter()
            .any(|m| matches!(m, Modifier::Static(_)))
        {
            continue;
        }
        let Some(hint) = &plain.hint else {
            continue; // untyped property → stored verbatim, no coercion.
        };
        let Some(sh) = scalar_hint_of(hint) else {
            continue; // non-scalar typed property → no scalar coercion.
        };
        // One hint applies to every item in `int $a, $b;`.
        for item in plain.items.iter() {
            let name = strip_dollar(item.variable().name);
            hints.insert(name, sh.clone());
        }
    }
    hints
}

/// Seed plain (non-promoted) property declarations carrying a literal default,
/// e.g. `public int $x = 5;`. Static / readonly / hooked / non-literal-default
/// properties BAIL (frontier §4). Properties with no default are left unset (a
/// later read of one bails).
fn seed_plain_property_defaults(
    class_node: &mago_syntax::ast::ast::class_like::Class,
    props: &mut Vec<(Vec<u8>, Value)>,
    _resolver: &BridgeResolver,
) -> Result<(), BailReason> {
    use mago_syntax::ast::ast::class_like::property::{Property, PropertyItem};
    use mago_syntax::ast::ast::modifier::Modifier;

    for member in class_node.members.iter() {
        let ClassLikeMember::Property(property) = member else {
            continue;
        };
        match property {
            Property::Plain(plain) => {
                // Static properties are not part of an instance record → bail if
                // present (the singleton-memo pattern must never be emulated).
                for m in plain.modifiers.iter() {
                    if matches!(m, Modifier::Static(_)) {
                        return Err(BailReason::UnsupportedConstruct("static property".into()));
                    }
                }
                for item in plain.items.iter() {
                    match item {
                        // No default → leave unset (read-before-init bails later).
                        PropertyItem::Abstract(_) => {}
                        PropertyItem::Concrete(c) => {
                            // A non-literal default (function call, new, etc.) bails.
                            let mut scope = Scope::new(HashMap::new(), &NoResolver);
                            let v = super::eval::eval_default(c.value, &mut scope)?;
                            // A typed scalar property coerces its default in PHP
                            // (`public float $x = 5;` stores `float(5.0)`); the
                            // reducer keeps `Int(5)` → fail-closed BAIL on mismatch.
                            if let Some(hint) = &plain.hint {
                                bail_if_scalar_hint_coerces(hint, &v, "property default")?;
                            }
                            let name = strip_dollar(c.variable.name);
                            set_prop(props, name, v);
                        }
                    }
                }
            }
            // Property hooks (get/set) change read/write semantics → bail.
            Property::Hooked(_) => {
                return Err(BailReason::UnsupportedConstruct("property hooks".into()))
            }
        }
    }
    Ok(())
}

/// Find a class constant `const NAME = <init>` in `class_node`'s OWN members and
/// fold its initializer to a literal [`Value`] (Inc-5 Task 4). `Ok(None)` → no such
/// constant declared here. A non-literal initializer bails via `eval_default`; a
/// typed constant (`const int X = 10`) re-checks scalar coercion (fail-closed).
fn find_class_constant_value(
    class_node: &mago_syntax::ast::ast::class_like::Class,
    const_name: &[u8],
) -> Result<Option<Value>, BailReason> {
    for member in class_node.members.iter() {
        let ClassLikeMember::Constant(konst) = member else {
            continue;
        };
        for item in konst.items.iter() {
            if item.name.value != const_name {
                continue;
            }
            // Literal-only fold (computed/call/new/unresolved → bail).
            let mut scope = Scope::new(HashMap::new(), &NoResolver);
            let v = super::eval::eval_default(item.value, &mut scope)?;
            // A typed scalar constant coerces its value in PHP (`const float X = 5;`
            // stores `5.0`); the reducer keeps `Int(5)` → fail-closed BAIL on a
            // mismatch (symmetric with `seed_plain_property_defaults`).
            if let Some(hint) = &konst.hint {
                bail_if_scalar_hint_coerces(hint, &v, "class constant")?;
            }
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// Whether a parameter carries the `readonly` modifier.
fn has_readonly_modifier(param: &FunctionLikeParameter) -> bool {
    use mago_syntax::ast::ast::modifier::Modifier;
    param
        .modifiers
        .iter()
        .any(|m| matches!(m, Modifier::Readonly(_)))
}

/// Strip a leading `$` from a variable token to get the bare property/param name.
fn strip_dollar(name: &[u8]) -> Vec<u8> {
    name.strip_prefix(b"$").unwrap_or(name).to_vec()
}

/// Set-or-update a prop in the insertion-ordered record (last write wins).
fn set_prop(props: &mut Vec<(Vec<u8>, Value)>, name: Vec<u8>, val: Value) {
    match props.iter_mut().find(|(k, _)| *k == name) {
        Some(slot) => slot.1 = val,
        None => props.push((name, val)),
    }
}

/// Bind positional `args` to a method's parameters (+ defaults). Variadic/by-ref,
/// or a parameter with neither an argument nor a computable default, bail.
fn bind_method_params(
    param_list: &mago_syntax::ast::ast::function_like::parameter::FunctionLikeParameterList,
    args: &[Value],
) -> Result<HashMap<Vec<u8>, Value>, BailReason> {
    let params: Vec<&FunctionLikeParameter> = param_list.parameters.iter().collect();
    if args.len() > params.len() {
        return Err(BailReason::Other(
            "more arguments than method parameters (variadic call?)".into(),
        ));
    }
    let mut bindings = HashMap::new();
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
        let value = match args.get(i) {
            Some(a) => a.clone(),
            None => match &param.default_value {
                Some(default) => {
                    let mut scope = Scope::new(HashMap::new(), &NoResolver);
                    super::eval::eval_default(default.value, &mut scope)?
                }
                None => {
                    return Err(BailReason::Other(
                        "parameter has no argument and no default".into(),
                    ))
                }
            },
        };
        bail_if_scalar_param_coerces(param, &value)?;
        bindings.insert(key, value);
    }
    Ok(bindings)
}

// ─── FQN-aware class / method resolution (Task C CRITICAL) ────────────────────
//
// The shared simple-name `find_method` matches a class by its bare tail, which can
// bind the WRONG body across namespaces (a silent divergence). These finders build
// each class's FULLY-QUALIFIED name from the enclosing namespace and compare it
// (case-insensitively) against the target FQCN that mago resolved.

/// Find the class node whose fully-qualified name equals `fqcn` (case-insensitive),
/// descending through namespaces and building the FQN from the namespace prefix.
fn find_class<'a>(
    program: &'a Program<'a>,
    fqcn: &[u8],
) -> Option<&'a mago_syntax::ast::ast::class_like::Class<'a>> {
    let target = normalize_fqcn(fqcn);
    find_class_in(program.statements.iter(), &[], &target)
}

fn find_class_in<'a, 's>(
    stmts: impl Iterator<Item = &'s Statement<'s>>,
    ns: &[u8],
    target: &[u8],
) -> Option<&'s mago_syntax::ast::ast::class_like::Class<'s>>
where
    's: 'a,
{
    use mago_syntax::ast::ast::namespace::NamespaceBody;
    for stmt in stmts {
        match stmt {
            Statement::Class(class) => {
                if qualified_name(ns, class.name.value).eq_ignore_ascii_case(target) {
                    return Some(class);
                }
            }
            Statement::Namespace(nsd) => {
                let inner = match nsd.name {
                    Some(n) => qualified_name(ns, n.value()),
                    None => ns.to_vec(),
                };
                let found = match &nsd.body {
                    NamespaceBody::Implicit(b) => {
                        find_class_in(b.statements.iter(), &inner, target)
                    }
                    NamespaceBody::BraceDelimited(b) => {
                        find_class_in(b.statements.iter(), &inner, target)
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

/// FQN-aware lookup of a `trait` AST node (mirror of [`find_class`] for
/// `Statement::Trait`). Used by round-5 trait-hint collection: a trait is a
/// distinct AST node from a class, but carries the same `members` shape.
fn find_trait<'a>(
    program: &'a Program<'a>,
    fqcn: &[u8],
) -> Option<&'a mago_syntax::ast::ast::class_like::Trait<'a>> {
    let target = normalize_fqcn(fqcn);
    find_trait_in(program.statements.iter(), &[], &target)
}

fn find_trait_in<'a, 's>(
    stmts: impl Iterator<Item = &'s Statement<'s>>,
    ns: &[u8],
    target: &[u8],
) -> Option<&'s mago_syntax::ast::ast::class_like::Trait<'s>>
where
    's: 'a,
{
    use mago_syntax::ast::ast::namespace::NamespaceBody;
    for stmt in stmts {
        match stmt {
            Statement::Trait(t) => {
                if qualified_name(ns, t.name.value).eq_ignore_ascii_case(target) {
                    return Some(t);
                }
            }
            Statement::Namespace(nsd) => {
                let inner = match nsd.name {
                    Some(n) => qualified_name(ns, n.value()),
                    None => ns.to_vec(),
                };
                let found = match &nsd.body {
                    NamespaceBody::Implicit(b) => {
                        find_trait_in(b.statements.iter(), &inner, target)
                    }
                    NamespaceBody::BraceDelimited(b) => {
                        find_trait_in(b.statements.iter(), &inner, target)
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

/// Find `class_fqcn::method` (FQN-aware) and return the method AST.
fn find_class_method<'a>(
    program: &'a Program<'a>,
    class_fqcn: &[u8],
    method: &[u8],
) -> Option<&'a Method<'a>> {
    let class = find_class(program, class_fqcn)?;
    for member in class.members.iter() {
        if let ClassLikeMember::Method(m) = member {
            if m.name.value.eq_ignore_ascii_case(method) {
                return Some(m);
            }
        }
    }
    None
}

/// Join a namespace prefix with a (possibly already-qualified) name.
fn qualified_name(ns: &[u8], name: &[u8]) -> Vec<u8> {
    let name = name.strip_prefix(b"\\").unwrap_or(name);
    if ns.is_empty() {
        name.to_vec()
    } else {
        let mut out = ns.to_vec();
        out.push(b'\\');
        out.extend_from_slice(name);
        out
    }
}

/// Strip a leading `\` so a `\Foo\Bar` FQCN compares equal to `Foo\Bar`.
fn normalize_fqcn(fqcn: &[u8]) -> Vec<u8> {
    fqcn.strip_prefix(b"\\").unwrap_or(fqcn).to_vec()
}

/// Find a top-level `function <name>(...) {...}`, descending through namespaces.
fn find_function<'a>(program: &'a Program<'a>, name: &[u8]) -> Option<&'a Function<'a>> {
    // The call name may be namespaced (`Foo\bar`); match on the simple tail since
    // the AST `Function.name` is a `LocalIdentifier` (unqualified).
    let simple = name.rsplit(|b| *b == b'\\').next().unwrap_or(name);
    find_function_in(program.statements.iter(), simple)
}

fn find_function_in<'a, 's, I>(stmts: I, simple: &[u8]) -> Option<&'s Function<'s>>
where
    's: 'a,
    I: Iterator<Item = &'s Statement<'s>>,
{
    for stmt in stmts {
        match stmt {
            Statement::Function(f) if f.name.value.eq_ignore_ascii_case(simple) => {
                return Some(f);
            }
            Statement::Namespace(ns) => {
                use mago_syntax::ast::ast::namespace::NamespaceBody;
                let found = match &ns.body {
                    NamespaceBody::Implicit(b) => find_function_in(b.statements.iter(), simple),
                    NamespaceBody::BraceDelimited(b) => {
                        find_function_in(b.statements.iter(), simple)
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

/// Bind positional `args` to the function's parameters. Variadic / by-ref params,
/// or a parameter with neither an argument nor a computable default, bail.
fn bind_params(
    func: &Function,
    args: &[Value],
) -> Result<std::collections::HashMap<Vec<u8>, Value>, BailReason> {
    let mut bindings = std::collections::HashMap::new();
    let params: Vec<_> = func.parameter_list.parameters.iter().collect();

    if args.len() > params.len() {
        return Err(BailReason::Other(
            "more arguments than parameters (variadic call?)".into(),
        ));
    }

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
        let key = param
            .variable
            .name
            .strip_prefix(b"$")
            .unwrap_or(param.variable.name)
            .to_vec();

        let value = if let Some(arg) = args.get(i) {
            arg.clone()
        } else if let Some(default) = &param.default_value {
            // Evaluate the default-value EXPRESSION in an empty scope (defaults are
            // constant-ish; a default that references a variable would bail there).
            let mut scope = Scope::new(std::collections::HashMap::new(), &NoResolver);
            super::eval::eval_default(default.value, &mut scope)?
        } else {
            return Err(BailReason::Other(
                "parameter has no argument and no default".into(),
            ));
        };
        bail_if_scalar_param_coerces(param, &value)?;
        bindings.insert(key, value);
    }

    Ok(bindings)
}

/// PHP coerces a scalar argument to a declared bare-scalar parameter type at the
/// call boundary (weak mode) or throws `TypeError` (strict). Neither is modelled,
/// so a mismatch BAILS (fail-closed). Routes through the shared scalar guard so
/// `?scalar` / `scalar|…` parameter hints are enforced the same as returns.
fn bail_if_scalar_param_coerces(
    param: &FunctionLikeParameter,
    value: &Value,
) -> Result<(), BailReason> {
    if let Some(hint) = &param.hint {
        bail_if_scalar_hint_coerces(hint, value, "parameter")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reduce::eval::Outcome;
    use std::collections::HashMap;

    /// Build a project from a single test file source, run the named test method's
    /// body (in class `class_name`) through the evaluator with the
    /// `BridgeResolver`, and return the outcome.
    fn reduce_with_subst(
        src: &str,
        class_name: &str,
        method: &str,
        givens: Vec<(&str, Value)>,
    ) -> Outcome {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Code.php"), src).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        let resolver = BridgeResolver::new(&project);

        // Derive the logical name the same way the bridge stores it: from the
        // class's declaring file (matches data_provider's resolution path).
        let class_meta = project.find_class(class_name).expect("class in codebase");
        let file = project
            .file_of_span(&class_meta.span)
            .expect("declaring file");
        let logical = String::from_utf8_lossy(&file.name).into_owned();

        let given_map: HashMap<Vec<u8>, Value> = givens
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v))
            .collect();

        project
            .with_program(&logical, |program, file, names| {
                let block = find_method_block(program, method).expect("method block");
                super::super::eval::run_method_body_with_names(
                    block,
                    given_map,
                    &resolver,
                    names,
                    &file.contents,
                )
            })
            .expect("with_program")
    }

    fn find_method_block<'a>(
        program: &'a mago_syntax::ast::Program<'a>,
        method: &str,
    ) -> Option<&'a mago_syntax::ast::ast::block::Block<'a>> {
        find_method_block_in(program.statements.iter(), method)
    }

    fn find_method_block_in<'s>(
        stmts: impl Iterator<Item = &'s Statement<'s>>,
        method: &str,
    ) -> Option<&'s mago_syntax::ast::ast::block::Block<'s>> {
        use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
        use mago_syntax::ast::ast::class_like::method::MethodBody;
        use mago_syntax::ast::ast::namespace::NamespaceBody;
        for stmt in stmts {
            match stmt {
                Statement::Class(class) => {
                    for member in class.members.iter() {
                        if let ClassLikeMember::Method(m) = member {
                            if m.name.value.eq_ignore_ascii_case(method.as_bytes()) {
                                if let MethodBody::Concrete(block) = &m.body {
                                    return Some(block);
                                }
                            }
                        }
                    }
                }
                Statement::Namespace(ns) => {
                    let found = match &ns.body {
                        NamespaceBody::Implicit(b) => {
                            find_method_block_in(b.statements.iter(), method)
                        }
                        NamespaceBody::BraceDelimited(b) => {
                            find_method_block_in(b.statements.iter(), method)
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

    #[test]
    fn inlines_a_same_file_user_function() {
        // A test that calls a free user function `add` and asserts on its result.
        let src = r#"<?php
function add(int $a, int $b): int { return $a + $b; }
class CalcTest {
    public function testAdd(): void {
        $this->assertSame(5, add(2, 3));
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "CalcTest", "testAdd", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn inlines_with_control_flow_and_default_param() {
        // abs-like helper with a branch + a defaulted parameter.
        let src = r#"<?php
function clamp_low(int $n, int $low = 0): int {
    if ($n < $low) { return $low; }
    return $n;
}
class T {
    public function testClamp(): void {
        $this->assertSame(0, clamp_low(-5));
        $this->assertSame(7, clamp_low(7, 3));
        $this->assertSame(3, clamp_low(1, 3));
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testClamp", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn nested_user_calls_inline() {
        let src = r#"<?php
function inc(int $x): int { return $x + 1; }
function twice(int $x): int { return inc(inc($x)); }
class T {
    public function testNested(): void {
        $this->assertSame(5, twice(3));
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testNested", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn variadic_param_bails() {
        let src = r#"<?php
function sum(int ...$xs): int { return 0; }
class T {
    public function testV(): void {
        $this->assertSame(0, sum(1, 2, 3));
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testV", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn scalar_param_coercion_bails() {
        // PHP coerces `"5"` to int `5` at the `int $x` parameter boundary (weak
        // mode) or throws TypeError (strict). Neither is modelled → BAIL rather
        // than bind `Str("5")` raw and return it (which would diverge: the test
        // would falsely PASS `assertSame(5, "5")` → Fail in real PHP). The `: mixed`
        // return hint isolates the PARAMETER as the only scalar-coercion site (a
        // scalar return hint would otherwise mask the param bug on the return path).
        let src = r#"<?php
function identity(int $x): mixed { return $x; }
class T {
    public function testCoerce(): void {
        $this->assertSame(5, identity("5"));
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testCoerce", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn matching_scalar_param_does_not_bail() {
        // An exactly-typed argument (Int → `int $x`) needs no coercion; the inline
        // must still PASS so the param-coercion guard does not over-bail.
        let src = r#"<?php
function identity(int $x): mixed { return $x; }
class T {
    public function testExact(): void {
        $this->assertSame(5, identity(5));
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testExact", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn nullable_scalar_return_coercion_bails() {
        // `?string` with a `true` return coerces to `"1"` in PHP; unmodelled → BAIL.
        // (The bonus fix only covered the BARE `: string` hint; `?string` slipped
        // through the `_ => Ok(())` arm and returned `Bool(true)` → divergence.)
        let src = r#"<?php
function r(): ?string { return true; }
class T {
    public function testNullable(): void {
        $this->assertSame("1", r());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testNullable", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn nullable_scalar_return_null_value_does_not_bail() {
        // A genuine `null` return under `?string` is no coercion → must not bail.
        let src = r#"<?php
function r(): ?string { return null; }
class T {
    public function testNullOk(): void {
        $this->assertNull(r());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testNullOk", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn union_scalar_return_coercion_bails() {
        // `int|string` with a `true` return matches NEITHER member exactly and PHP
        // coerces (to `1`/`"1"` depending on context) → BAIL.
        let src = r#"<?php
function u(): int|string { return true; }
class T {
    public function testUnion(): void {
        $this->assertSame(1, u());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testUnion", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn union_scalar_return_matching_member_does_not_bail() {
        // An `int` value already matches the `int` member of `int|string` → no
        // coercion, must not over-bail.
        let src = r#"<?php
function u(): int|string { return 7; }
class T {
    public function testUnionOk(): void {
        $this->assertSame(7, u());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testUnionOk", vec![]),
            Outcome::Pass
        );
    }

    // ── Increment 2: objects + methods (the Point fixture) ──

    /// The `Point` value object — exercises every inc-2 mechanic: `new` with a
    /// promoted-param constructor, `$this` bind, instance-method inline,
    /// `$this->x` read, a fresh-object return, and a scalar `assertSame`.
    const POINT_SRC: &str = r#"<?php
final class Point {
    public function __construct(public int $x, public int $y) {}
    public function plus(Point $p): Point { return new Point($this->x + $p->x, $this->y + $p->y); }
    public function getX(): int { return $this->x; }
    public function getY(): int { return $this->y; }
}
class PointTest {
    public function testPlusGetX(): void {
        $this->assertSame(4, (new Point(1, 2))->plus(new Point(3, 0))->getX());
    }
    public function testPromotedReadback(): void {
        $p = new Point(7, 9);
        $this->assertSame(7, $p->getX());
        $this->assertSame(9, $p->getY());
    }
    public function testFreshObjectIsImmutable(): void {
        $a = new Point(1, 2);
        $b = $a->plus(new Point(10, 20));
        $this->assertSame(1, $a->getX());
        $this->assertSame(11, $b->getX());
        $this->assertSame(22, $b->getY());
    }
    public function testStructuralEquals(): void {
        $this->assertEquals(new Point(1, 2), new Point(1, 2));
        $this->assertNotEquals(new Point(1, 2), new Point(9, 2));
    }
}
"#;

    #[test]
    fn point_plus_get_x_reduces() {
        assert_eq!(
            reduce_with_subst(POINT_SRC, "PointTest", "testPlusGetX", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn point_promoted_readback_reduces() {
        assert_eq!(
            reduce_with_subst(POINT_SRC, "PointTest", "testPromotedReadback", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn point_fresh_object_is_immutable() {
        assert_eq!(
            reduce_with_subst(POINT_SRC, "PointTest", "testFreshObjectIsImmutable", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn point_structural_equals_reduces() {
        assert_eq!(
            reduce_with_subst(POINT_SRC, "PointTest", "testStructuralEquals", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn assert_same_on_object_bails() {
        // assertSame on objects is REFERENCE identity — the reducer has no heap
        // model, so it MUST bail (frontier §1), not guess structural equality.
        let src = r#"<?php
final class Box { public function __construct(public int $v) {} }
class BoxTest {
    public function testSame(): void {
        $this->assertSame(new Box(1), new Box(1));
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "BoxTest", "testSame", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn mutator_method_bails() {
        // A mutator dispatched on a NON-assignable receiver (a `(new …)` temporary)
        // BAILS: there is no binding to write the mutated `$this` back to, so we
        // cannot guarantee soundness (Inc-5 Task 3 — mutation needs a writeback
        // target). A mutator on a uniquely-owned `$var` receiver is sound (see
        // `mutator_on_unique_owner_reduces`).
        let src = r#"<?php
final class Counter {
    public function __construct(public int $n) {}
    public function bump(): int { $this->n = $this->n + 1; return $this->n; }
}
class CounterTest {
    public function testBump(): void {
        $this->assertSame(2, (new Counter(1))->bump());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "CounterTest", "testBump", vec![]),
            Outcome::Bailed(_)
        ));
    }

    // ── Inc-5 Task 3: instance dispatch + $this-mutation (aliasing-fail-closed) ──

    #[test]
    fn readonly_getter_on_variable_receiver_reduces() {
        // Construct + read-only dispatch on a `$var` receiver — no mutation, no
        // aliasing. Must Pass (the whole vertical slice for a plain value object).
        let src = r#"<?php
final class Money {
    public function __construct(public int $cents) {}
    public function cents(): int { return $this->cents; }
}
class MoneyTest {
    public function testCents(): void {
        $m = new Money(500);
        $this->assertSame(500, $m->cents());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "MoneyTest", "testCents", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn mutator_on_unique_owner_reduces() {
        // A mutator (`$this->n = …`) called on a FRESHLY-constructed, NON-aliased
        // `$var` receiver: the object is uniquely owned, so we mutate it and write
        // the result back to `$c`. A subsequent read sees the mutation. Must Pass.
        let src = r#"<?php
final class Counter {
    public function __construct(public int $n) {}
    public function inc(): void { $this->n = $this->n + 1; }
    public function value(): int { return $this->n; }
}
class CounterTest {
    public function testInc(): void {
        $c = new Counter(0);
        $c->inc();
        $this->assertSame(1, $c->value());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "CounterTest", "testInc", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn mutation_of_aliased_object_bails() {
        // THE divergence-prevention test. `$b = $a` makes `$b` and `$a` the SAME
        // object in PHP, so `$a->inc()` is visible via `$b`. Our by-value model
        // would update only `$a` → `$b->value()` would still read 0 → a WRONG Pass
        // (PHP: assertSame(1, 1) Pass; reducer-by-value: assertSame(1, 0) Fail).
        // The aliasing guard must catch this and BAIL on the mutating `$a->inc()`
        // because `$a` is aliased — NEVER a wrong Pass, NEVER a Fail.
        let src = r#"<?php
final class Counter {
    public function __construct(public int $n) {}
    public function inc(): void { $this->n = $this->n + 1; }
    public function value(): int { return $this->n; }
}
class CounterTest {
    public function testAlias(): void {
        $a = new Counter(0);
        $b = $a;
        $a->inc();
        $this->assertSame(1, $b->value());
    }
}
"#;
        let outcome = reduce_with_subst(src, "CounterTest", "testAlias", vec![]);
        match outcome {
            Outcome::Bailed(super::super::eval::BailReason::UnsupportedConstruct(ref m)) => {
                assert!(
                    m.contains("aliased"),
                    "must bail specifically on the aliasing guard; got {m:?}"
                );
            }
            other => panic!("mutation of an aliased object MUST bail on the aliasing guard, never diverge; got {other:?}"),
        }
    }

    // ── Adversarial aliasing battery: every escape route a mutation could leak
    //    through MUST bail (never a wrong Pass/Fail). PHP shares the handle in all
    //    of these; our by-value model would diverge, so the guard must fire. ──

    const COUNTER_SRC: &str = r#"<?php
final class Counter {
    public function __construct(public int $n) {}
    public function inc(): void { $this->n = $this->n + 1; }
    public function value(): int { return $this->n; }
}
"#;

    fn counter_test(body: &str) -> String {
        format!("{COUNTER_SRC}class CounterTest {{ public function testX(): void {{ {body} }} }}\n")
    }

    #[test]
    fn mutation_via_helper_that_received_object_bails() {
        // Object passed to a helper that mutates it: PHP mutates the SHARED handle,
        // visible back in the caller. Passing as an arg marks `$c` aliased, so the
        // mutation inside the helper bails (and the caller's later read can't diverge).
        let src = format!(
            "{COUNTER_SRC}\
function bump(Counter $x): void {{ $x->inc(); }}\n\
class CounterTest {{ public function testX(): void {{ \
$c = new Counter(0); bump($c); $this->assertSame(1, $c->value()); }} }}\n"
        );
        assert!(
            matches!(
                reduce_with_subst(&src, "CounterTest", "testX", vec![]),
                Outcome::Bailed(_)
            ),
            "mutation of an arg-passed (aliased) object must bail"
        );
    }

    #[test]
    fn mutation_of_object_stored_in_array_bails() {
        // Object stored into an array then mutated through the array element.
        let src = counter_test(
            "$c = new Counter(0); $arr = [$c]; $arr[0]->inc(); $this->assertSame(1, $c->value());",
        );
        assert!(
            matches!(
                reduce_with_subst(&src, "CounterTest", "testX", vec![]),
                Outcome::Bailed(_)
            ),
            "mutation through an array element (non-assignable receiver / aliased) must bail"
        );
    }

    #[test]
    fn mutation_after_array_store_then_var_mutate_bails() {
        // Store the object into an array (aliasing it), THEN mutate the original
        // `$var` — PHP sees the change in the array too; our writeback to `$c`
        // would leave the array stale → must bail because `$c` is now aliased.
        let src = counter_test(
            "$c = new Counter(0); $arr = [$c]; $c->inc(); $this->assertSame(0, $arr[0]->value());",
        );
        assert!(
            matches!(
                reduce_with_subst(&src, "CounterTest", "testX", vec![]),
                Outcome::Bailed(_)
            ),
            "mutating a $var after it was stored into an array (aliased) must bail"
        );
    }

    #[test]
    fn mutation_of_returned_alias_bails() {
        // A helper returns its argument (a shared handle); mutating the returned
        // object then reading the original would diverge → must bail.
        let src = format!(
            "{COUNTER_SRC}\
function passthru(Counter $x): Counter {{ return $x; }}\n\
class CounterTest {{ public function testX(): void {{ \
$c = new Counter(0); $d = passthru($c); $d->inc(); $this->assertSame(1, $c->value()); }} }}\n"
        );
        assert!(
            matches!(
                reduce_with_subst(&src, "CounterTest", "testX", vec![]),
                Outcome::Bailed(_)
            ),
            "mutation of a returned (aliased) object must bail"
        );
    }

    #[test]
    fn mutation_via_indirect_this_copy_bails() {
        // A method copies `$this` to a local then mutates the copy. PHP: the copy is
        // the SAME object → `$this` changes too. `$x = $this` marks both aliased, so
        // `$x->inc()` inside the body bails.
        let src = format!(
            "{COUNTER_SRC}\
final class Tricky extends Counter {{ public function sneaky(): void {{ $x = $this; $x->inc(); }} }}\n\
class CounterTest {{ public function testX(): void {{ \
$t = new Tricky(0); $t->sneaky(); $this->assertSame(1, $t->value()); }} }}\n"
        );
        assert!(
            matches!(
                reduce_with_subst(&src, "CounterTest", "testX", vec![]),
                Outcome::Bailed(_)
            ),
            "mutation via an indirect $this copy must bail"
        );
    }

    #[test]
    fn two_independent_objects_each_mutate_soundly() {
        // No aliasing: two distinct `$var` receivers, each mutated in place. Both
        // are uniquely owned → both writebacks are sound → Pass (no over-bail).
        let src = counter_test(
            "$a = new Counter(0); $b = new Counter(10); $a->inc(); $b->inc(); \
$this->assertSame(1, $a->value()); $this->assertSame(11, $b->value());",
        );
        assert_eq!(
            reduce_with_subst(&src, "CounterTest", "testX", vec![]),
            Outcome::Pass,
            "two independently-owned objects must each mutate soundly (no over-bail)"
        );
    }

    #[test]
    fn mutation_of_chained_assignment_alias_bails() {
        // Round 6 finding 1: chained assignment `$a = $b = new Counter(0)`. The
        // inner `$b = new Counter(0)` is a fresh instantiation → `$b` non-aliased;
        // the outer `$a = $b` aliases the SAME object. PHP: `$b->inc()` is visible
        // via `$a` (assertSame(1, 1) Pass). Our by-value model would mutate only
        // `$b`'s binding while `$a` stays at 0 → assertSame(1, 0) Fail = DIVERGENCE.
        // The whitelist guard must mark the inner just-bound `$b` aliased (its value
        // is reused by the enclosing assignment) so `$b->inc()` BAILS.
        let src =
            counter_test("$a = $b = new Counter(0); $b->inc(); $this->assertSame(1, $a->value());");
        let outcome = reduce_with_subst(&src, "CounterTest", "testX", vec![]);
        assert!(
            matches!(outcome, Outcome::Bailed(_)),
            "mutation through a chained-assignment alias must bail, never diverge; got {outcome:?}"
        );
    }

    #[test]
    fn mutation_after_fluent_self_return_bails() {
        // Round 6 finding 2: fluent `$d = $c->bump()` where `bump()` mutates `$this`
        // and `return $this`. PHP: `$d` and `$c` are the SAME object; the second
        // `$c->bump()` is visible via `$d` (assertSame(2, 2) Pass). Our by-value
        // model writes the mutation back to `$c` but `$d` is an unswept copy → the
        // second mutation lands only on `$c`, `$d->value()` reads 1 → assertSame(2, 1)
        // Fail = DIVERGENCE. A mutating dispatch that returns a Value::Object must
        // mark BOTH the written-back receiver and the returned value aliased so the
        // second `$c->bump()` BAILS.
        let src = format!(
            "{COUNTER_SRC_FLUENT}class CounterTest {{ public function testX(): void {{ \
$c = new Counter(0); $d = $c->bump(); $c->bump(); $this->assertSame(2, $d->value()); }} }}\n"
        );
        let outcome = reduce_with_subst(&src, "CounterTest", "testX", vec![]);
        assert!(
            matches!(outcome, Outcome::Bailed(_)),
            "mutation after a fluent self-return alias must bail, never diverge; got {outcome:?}"
        );
    }

    // Counter with a fluent mutator `bump(): static { ...; return $this; }`.
    const COUNTER_SRC_FLUENT: &str = r#"<?php
final class Counter {
    public function __construct(public int $n) {}
    public function inc(): void { $this->n = $this->n + 1; }
    public function bump(): static { $this->n = $this->n + 1; return $this; }
    public function value(): int { return $this->n; }
}
"#;

    // ── Inc-5 Task 4: class-constant access (literal const table + ::class) ──

    #[test]
    fn class_constant_and_class_magic_resolve() {
        // `C::LIMIT` reads a literal const; `C::class` folds to the FQCN string.
        let src = r#"<?php
class C {
    const int LIMIT = 10;
    const NAME = 'widget';
}
class CTest {
    public function testConst(): void {
        $this->assertSame(10, C::LIMIT);
        $this->assertSame('widget', C::NAME);
        $this->assertSame('C', C::class);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "CTest", "testConst", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn self_class_constant_bails_no_lexical_context() {
        // Round 6: `self::CAP` inside a method binds to the LEXICAL defining class,
        // which the Scope does not carry (only the runtime `$this` class). Even when
        // it would resolve correctly here (Box is final, const not inherited), the
        // reducer cannot prove the lexical == runtime class in general → BAIL
        // (fail-closed; over-bail is safe, the runtime-class fold is unsound). The
        // explicit-class path (`Foo::CAP`) still resolves — see
        // `explicit_class_constant_still_resolves`.
        let src = r#"<?php
final class Box {
    const int CAP = 7;
    public function __construct(public int $v) {}
    public function capped(): int { return self::CAP; }
}
class BoxTest {
    public function testSelf(): void {
        $b = new Box(0);
        $this->assertSame(7, $b->capped());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "BoxTest", "testSelf", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn self_class_constant_in_inherited_method_bails() {
        // Round 6 finding: `self::`/`parent::`/`static::` bind to the LEXICAL
        // defining class of the method, NOT the runtime class. `Base::tag()` returns
        // `self::class`; called on a `Child extends Base`, PHP yields 'Base' (the
        // lexical class where `tag` is declared) → assertSame('Base', 'Base') Pass.
        // The reducer has no lexical-defining-class context — it folds `self::class`
        // from the runtime `$this` class 'Child' → assertSame('Base', 'Child') Fail
        // = DIVERGENCE. With no sound way to recover the lexical class, this MUST
        // BAIL.
        let src = r#"<?php
class Base {
    const TAG = 'base';
    public function tag(): mixed { return self::class; }
}
class Child extends Base {}
class TagTest {
    public function testTag(): void {
        $c = new Child();
        $this->assertSame('Base', $c->tag());
    }
}
"#;
        let outcome = reduce_with_subst(src, "TagTest", "testTag", vec![]);
        assert!(
            matches!(outcome, Outcome::Bailed(_)),
            "self::class in an inherited method has no lexical-class context → must bail, never diverge; got {outcome:?}"
        );
    }

    #[test]
    fn explicit_class_constant_still_resolves() {
        // No-over-bail control for the self/parent/static bail: an EXPLICIT named
        // class `Foo::CONST` / `Foo::class` is unambiguous (the class is named, not
        // self/parent/static) → it stays resolved (that is sound). Only the
        // self/parent/static-qualified forms bail.
        let src = r#"<?php
class Foo {
    const int LIMIT = 42;
}
class FooTest {
    public function testFoo(): void {
        $this->assertSame(42, Foo::LIMIT);
        $this->assertSame('Foo', Foo::class);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "FooTest", "testFoo", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn computed_class_constant_bails() {
        // A const whose initializer is a runtime call (non-literal/non-modelled) is
        // not foldable → must BAIL, never guess.
        let src = r#"<?php
class C {
    const VAL = STR_PAD_LEFT;
    public static function compute(): int { return random_int(1, 9); }
}
class CTest {
    public function testComputed(): void {
        $this->assertSame(5, C::computed());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "CTest", "testComputed", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn enum_case_access_bails() {
        // Enum cases (`Suit::Hearts`) are deferred — must BAIL (never fold to a guess).
        let src = r#"<?php
enum Suit: string {
    case Hearts = 'H';
    case Spades = 'S';
}
class SuitTest {
    public function testEnum(): void {
        $this->assertSame('H', Suit::Hearts->value);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "SuitTest", "testEnum", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn static_method_inlines() {
        // `Class::make(...)` (a static factory) inlines without `$this`.
        let src = r#"<?php
final class Money {
    public function __construct(public int $cents) {}
    public static function fromDollars(int $d): Money { return new Money($d * 100); }
    public function cents(): int { return $this->cents; }
}
class MoneyTest {
    public function testFactory(): void {
        $this->assertSame(500, Money::fromDollars(5)->cents());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "MoneyTest", "testFactory", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn self_static_reference_bails() {
        // `self::` / `static::` have no enclosing-class context → bail (frontier §3).
        let src = r#"<?php
final class Maker {
    public function __construct(public int $v) {}
    public static function zero(): Maker { return new Maker(0); }
    public static function viaSelf(): Maker { return self::zero(); }
    public function v(): int { return $this->v; }
}
class MakerTest {
    public function testSelf(): void {
        $this->assertSame(0, Maker::viaSelf()->v());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "MakerTest", "testSelf", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn plain_property_default_seeds() {
        // A non-promoted property with a literal default + a ctor body write.
        let src = r#"<?php
final class Config {
    public int $level = 3;
    public string $name;
    public function __construct(string $name) { $this->name = $name; }
    public function level(): int { return $this->level; }
    public function name(): string { return $this->name; }
}
class ConfigTest {
    public function testDefaults(): void {
        $c = new Config('prod');
        $this->assertSame(3, $c->level());
        $this->assertSame('prod', $c->name());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "ConfigTest", "testDefaults", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn fqn_aware_resolution_binds_the_right_namespace_body() {
        // Two classes with the SAME simple name `Calc` in different namespaces,
        // each with a `value()` returning a DIFFERENT constant. The method resolver
        // is FQN-aware: a fully-qualified `\App\Calc` must bind `App\Calc::value`
        // → 100, never `Lib\Calc::value` → 1 (the simple-name match it replaced
        // could have bound the wrong body — a silent divergence). The class is
        // referenced fully-qualified so it resolves without `use`-alias name
        // resolution (unqualified-name resolution at the call site is inc-3).
        let src = r#"<?php
namespace Lib { final class Calc { public function value(): int { return 1; } } }
namespace App {
    final class Calc { public function value(): int { return 100; } }
}
namespace {
    class CalcTest {
        public function testValue(): void {
            $this->assertSame(100, (new \App\Calc())->value());
            $this->assertSame(1, (new \Lib\Calc())->value());
        }
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "CalcTest", "testValue", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn fluent_return_this_and_method_chain_on_this() {
        // `return $this;` (no mutation) + a method that chains `$this->m()->n()`.
        // Exercises $this read + nested instance-method inline with $this rebound.
        let src = r#"<?php
final class Builder {
    public function __construct(public int $a, public int $b) {}
    public function self(): Builder { return $this; }
    public function sum(): int { return $this->a + $this->b; }
    public function viaThis(): int { return $this->self()->sum(); }
}
class BuilderTest {
    public function testChain(): void {
        $this->assertSame(7, (new Builder(3, 4))->viaThis());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "BuilderTest", "testChain", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn nested_object_structural_equals() {
        // assertEquals over objects whose props are themselves objects (recursive
        // per-prop loose compare).
        let src = r#"<?php
final class Inner { public function __construct(public int $v) {} }
final class Outer { public function __construct(public Inner $a, public Inner $b) {} }
class NestTest {
    public function testEquals(): void {
        $this->assertEquals(
            new Outer(new Inner(1), new Inner(2)),
            new Outer(new Inner(1), new Inner(2))
        );
        $this->assertNotEquals(
            new Outer(new Inner(1), new Inner(2)),
            new Outer(new Inner(9), new Inner(2))
        );
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "NestTest", "testEquals", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn read_of_unseeded_property_bails() {
        // A constructor that does not seed a (non-default) property, then a method
        // reads it: PHP would warn + return null; the reducer bails (fail-closed).
        let src = r#"<?php
final class Partial {
    public int $a;
    public int $b;
    public function __construct(int $a) { $this->a = $a; }
    public function b(): int { return $this->b; }
}
class PartialTest {
    public function testReadUnset(): void {
        $this->assertSame(0, (new Partial(1))->b());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "PartialTest", "testReadUnset", vec![]),
            Outcome::Bailed(_)
        ));
    }

    // ── Increment 3: static-form assertions (doctrine uses `self::assertSame`) ──

    #[test]
    fn self_static_assertion_is_intercepted() {
        // doctrine/collections asserts via `self::assertSame(...)`, not
        // `$this->assertSame(...)`. The static form must be intercepted as an
        // assertion (self/static/parent receiver) — never dispatched as a method
        // call (which would bail on `self::`).
        let src = r#"<?php
class T {
    public function testStatic(): void {
        self::assertSame(4, 2 + 2);
        static::assertTrue(1 === 1);
        self::assertCount(3, [1, 2, 3]);
        self::assertIsArray([1, 2]);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testStatic", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn unqualified_new_resolves_via_use_alias() {
        // Inc-3 name resolution: a `use`-imported class referenced by its SIMPLE
        // name in a `new` must resolve to its FQCN (the codebase key), not bail on
        // the bare identifier. Two namespaces declare `Box`; the test imports the
        // App one, so `new Box(7)` must build App\Box, never Lib\Box.
        let src = r#"<?php
namespace Lib { final class Box { public function __construct(public int $v) {} public function v(): int { return $this->v; } } }
namespace App { final class Box { public function __construct(public int $v) {} public function v(): int { return $this->v + 1000; } } }
namespace Test {
    use App\Box;
    class BoxTest {
        public function testNew(): void {
            $this->assertSame(1007, (new Box(7))->v());
        }
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "Test\\BoxTest", "testNew", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn self_static_assertion_fails_when_wrong() {
        let src = r#"<?php
class T {
    public function testStatic(): void {
        self::assertSame(5, 2 + 2);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testStatic", vec![]),
            Outcome::Fail(_)
        ));
    }

    // ── Round 3: scalar-coercion sink matrix (the completeness lock) ──
    //
    // EVERY typed scalar boundary the reducer can reach must route a crossing
    // value through `bail_if_scalar_hint_coerces` BEFORE storing/using it, so a
    // weak-mode coercion (PHP silently converts) or strict-mode TypeError (PHP
    // throws) — neither modelled — fails CLOSED (BAIL) rather than diverging.
    // For each sink there is a *bails* test (mismatch → Bailed) and a
    // *no-over-bail* test (matching type → Pass). The enumerated sink set is
    // documented at `bail_if_scalar_hint_coerces`.

    // Sink: typed PROPERTY WRITE (`$this->n = $rhs` in a constructor body).
    #[test]
    fn property_write_scalar_coercion_bails() {
        // PHP coerces the `"10"` string to `int(10)` at the `int $n` property
        // boundary → `$p->n === int(10)`, assertSame PASSES. The reducer stores
        // `Str("10")` verbatim and would model `Int(10) === Str("10")` → false →
        // FAIL = divergence. Fail-closed: BAIL.
        let src = r#"<?php
class P {
    public int $n;
    public function __construct() { $v = "10"; $this->n = $v; }
}
class T {
    public function testPropWrite(): void {
        $p = new P();
        $this->assertSame(10, $p->n);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testPropWrite", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn property_write_matching_type_does_not_bail() {
        // A matching `int` write needs no coercion → must still PASS (no over-bail).
        let src = r#"<?php
class P {
    public int $n;
    public function __construct() { $this->n = 10; }
}
class T {
    public function testPropWriteOk(): void {
        $p = new P();
        $this->assertSame(10, $p->n);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testPropWriteOk", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn untyped_property_write_does_not_bail() {
        // An UNtyped property carries no coercion → the value is stored verbatim,
        // no bail (guards against over-bailing untyped writes).
        let src = r#"<?php
class P {
    public $n;
    public function __construct() { $v = "10"; $this->n = $v; }
}
class T {
    public function testUntyped(): void {
        $p = new P();
        $this->assertSame("10", $p->n);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testUntyped", vec![]),
            Outcome::Pass
        );
    }

    // Sink: typed PROPERTY DEFAULT (`public float $x = 5;`).
    #[test]
    fn property_default_scalar_coercion_bails() {
        // PHP stores `float(5.0)` for `public float $x = 5;`; assertSame(5.0,…)
        // PASSES. The reducer evaluates `5` → `Int(5)` and would model
        // `Float(5.0) === Int(5)` → FAIL = divergence. Fail-closed: BAIL.
        let src = r#"<?php
class D { public float $x = 5; }
class T {
    public function testDefault(): void {
        $d = new D();
        $this->assertSame(5.0, $d->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testDefault", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn property_default_matching_type_does_not_bail() {
        // A matching `int` default needs no coercion → must still PASS.
        let src = r#"<?php
class D { public int $x = 5; }
class T {
    public function testDefaultOk(): void {
        $d = new D();
        $this->assertSame(5, $d->x);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testDefaultOk", vec![]),
            Outcome::Pass
        );
    }

    // Sink: CLOSURE / ARROW typed PARAM.
    #[test]
    fn closure_param_scalar_coercion_bails() {
        // `fn(int $x) => $x` coerces `"5"` to `int(5)` at the param boundary; PHP
        // returns `int(5)`, assertSame(5,…) PASSES. The reducer binds `Str("5")`
        // verbatim and would model `Int(5) === Str("5")` → FAIL. Fail-closed: BAIL.
        let src = r#"<?php
class T {
    public function testClosureParam(): void {
        $f = fn(int $x) => $x;
        $this->assertSame(5, $f("5"));
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testClosureParam", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn closure_param_matching_type_does_not_bail() {
        // A matching `int` argument needs no coercion → must still PASS.
        let src = r#"<?php
class T {
    public function testClosureParamOk(): void {
        $f = fn(int $x) => $x;
        $this->assertSame(5, $f(5));
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testClosureParamOk", vec![]),
            Outcome::Pass
        );
    }

    // Sink: CLOSURE / ARROW typed RETURN.
    #[test]
    fn closure_return_scalar_coercion_bails() {
        // A `function (): string { return true; }` closure coerces the `true`
        // return to `"1"` in PHP; assertSame("1",…) PASSES. The reducer returns
        // `Bool(true)` verbatim → models `Str("1") === Bool(true)` → FAIL.
        // Fail-closed: BAIL.
        let src = r#"<?php
class T {
    public function testClosureReturn(): void {
        $f = function (): string { return true; };
        $this->assertSame("1", $f());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testClosureReturn", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn closure_return_matching_type_does_not_bail() {
        // A matching `string` return needs no coercion → must still PASS.
        let src = r#"<?php
class T {
    public function testClosureReturnOk(): void {
        $f = function (): string { return "x"; };
        $this->assertSame("x", $f());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "T", "testClosureReturnOk", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn closure_default_param_scalar_coercion_bails() {
        // The default branch of the closure param bind is also a typed boundary:
        // `fn(int $x = 5)` called with no args binds the default. A *coercing*
        // default value (string literal "5") would coerce to int → BAIL.
        let src = r#"<?php
class T {
    public function testClosureDefault(): void {
        $f = fn(int $x = "5") => $x;
        $this->assertSame(5, $f());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "T", "testClosureDefault", vec![]),
            Outcome::Bailed(_)
        ));
    }
}
