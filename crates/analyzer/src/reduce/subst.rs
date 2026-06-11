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
use std::collections::{HashMap, HashSet};

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

    /// PHP property-READ visibility (round 18). The record model is scope-blind:
    /// it stored ONE slot per name and returned it to any reader, while PHP
    /// denies an out-of-scope read (Error → PHPUnit test Error for an external
    /// read; undefined-property Warning + NULL for a parent-private read from a
    /// child method) — both were definitive false verdicts.
    ///
    /// Resolution walks the receiver's chain MOST-DERIVED FIRST and stops at the
    /// first class-like whose codex metadata declares the prop (own + flattened
    /// trait props; the slot tracker already bailed shadowed-private chains, so
    /// the first declaration found IS the single modelled slot):
    /// * `public` → allow (the overwhelmingly common case);
    /// * `private` → allow only when the reading body's declaring class IS the
    ///   declaring class;
    /// * `protected` → allow only when the reading class is somewhere in the
    ///   receiver's own chain (PHP allows the declaring class, descendants AND
    ///   ancestors — all linearly related inside one chain).
    ///
    /// A prop declared NOWHERE in the chain is dynamic (PHP reads it publicly) →
    /// allow; an unknown receiver class is a defensive allow (every
    /// record-producing path verified its class resolves — and an unseeded slot
    /// still bails at the read). `reading_class = None` (free function / closure
    /// scope) has no class context → any non-public read bails (PHP denies it
    /// from global scope too).
    fn bail_inaccessible_prop_read(
        &self,
        receiver_class: &[u8],
        prop: &[u8],
        reading_class: Option<&[u8]>,
    ) -> Result<(), BailReason> {
        use mago_codex::visibility::Visibility;
        let codebase = self.project.codebase();
        let leaf_key = normalize_fqcn(receiver_class).to_ascii_lowercase();
        let Some(class_meta) = codebase.get_class_like(&leaf_key) else {
            return Ok(());
        };
        let mut chain: Vec<Vec<u8>> = vec![class_meta.name.as_bytes().to_vec()];
        chain.extend(
            class_meta
                .all_parent_classes
                .iter()
                .map(|w| w.as_bytes().to_vec()),
        );
        let reading_key = reading_class.map(|c| normalize_fqcn(c).to_ascii_lowercase());
        // codex keys properties WITH the leading `$`.
        let mut prop_key = Vec::with_capacity(prop.len() + 1);
        prop_key.push(b'$');
        prop_key.extend_from_slice(prop);
        for hop in &chain {
            let Some(pm) = codebase.get_property(hop, &prop_key) else {
                continue;
            };
            // A `static` property is never SERVED through `$obj->x` (PHP reads an
            // undefined instance property → NULL); the record returned the slot
            // value → a definitive false verdict (round 19).
            if pm.flags.is_static() {
                return Err(BailReason::UnsupportedConstruct(format!(
                    "read of static property ${} through an instance \
                     (PHP undefined-property NULL divergence unmodelled)",
                    String::from_utf8_lossy(prop),
                )));
            }
            let declaring = normalize_fqcn(hop).to_ascii_lowercase();
            match pm.read_visibility {
                Visibility::Public => return Ok(()),
                Visibility::Private => {
                    if reading_key.as_deref() == Some(declaring.as_slice()) {
                        return Ok(());
                    }
                    return Err(BailReason::UnsupportedConstruct(format!(
                        "read of private property ${} outside its declaring class `{}` \
                         (PHP visibility Error / undefined-property divergence unmodelled)",
                        String::from_utf8_lossy(prop),
                        String::from_utf8_lossy(&declaring),
                    )));
                }
                Visibility::Protected => {
                    if let Some(rk) = &reading_key {
                        if chain
                            .iter()
                            .any(|h| normalize_fqcn(h).to_ascii_lowercase() == *rk)
                        {
                            return Ok(());
                        }
                    }
                    return Err(BailReason::UnsupportedConstruct(format!(
                        "read of protected property ${} from outside the receiver's \
                         class chain (PHP visibility Error unmodelled)",
                        String::from_utf8_lossy(prop),
                    )));
                }
            }
        }
        // Declared nowhere in the chain → a dynamic prop; PHP reads it publicly.
        Ok(())
    }

    /// PHP property-WRITE visibility (round 19) — the write-side twin of
    /// [`Self::bail_inaccessible_prop_read`]. Walks the receiver chain
    /// most-derived first to the declaring hop and consults `write_visibility`
    /// (equal to `read_visibility` under a classic declaration; the narrowed
    /// set-scope under `private(set)` / `protected(set)`):
    /// * a `static` declaration → BAIL (PHP never writes a static via `$obj->`,
    ///   it forks a dynamic instance property);
    /// * `public` → allow;
    /// * `private` → allow only when the writing body's class IS the declaring
    ///   class (else PHP forks a separate dynamic property OR Errors — the record
    ///   overwrite diverges);
    /// * `protected` → allow only when the writing class is in the receiver chain.
    ///
    /// A prop declared NOWHERE is a dynamic write (allow, as today). No class
    /// context (`writing_class = None`, a free function / closure) → any
    /// non-public write bails.
    fn bail_inaccessible_prop_write(
        &self,
        receiver_class: &[u8],
        prop: &[u8],
        writing_class: Option<&[u8]>,
    ) -> Result<(), BailReason> {
        use mago_codex::visibility::Visibility;
        let codebase = self.project.codebase();
        let leaf_key = normalize_fqcn(receiver_class).to_ascii_lowercase();
        let Some(class_meta) = codebase.get_class_like(&leaf_key) else {
            return Ok(());
        };
        let mut chain: Vec<Vec<u8>> = vec![class_meta.name.as_bytes().to_vec()];
        chain.extend(
            class_meta
                .all_parent_classes
                .iter()
                .map(|w| w.as_bytes().to_vec()),
        );
        let writing_key = writing_class.map(|c| normalize_fqcn(c).to_ascii_lowercase());
        let mut prop_key = Vec::with_capacity(prop.len() + 1);
        prop_key.push(b'$');
        prop_key.extend_from_slice(prop);
        for hop in &chain {
            let Some(pm) = codebase.get_property(hop, &prop_key) else {
                continue;
            };
            if pm.flags.is_static() {
                return Err(BailReason::UnsupportedConstruct(format!(
                    "write to static property ${} through an instance \
                     (PHP dynamic-property divergence unmodelled)",
                    String::from_utf8_lossy(prop),
                )));
            }
            let declaring = normalize_fqcn(hop).to_ascii_lowercase();
            match pm.write_visibility {
                Visibility::Public => return Ok(()),
                Visibility::Private => {
                    if writing_key.as_deref() == Some(declaring.as_slice()) {
                        return Ok(());
                    }
                    return Err(BailReason::UnsupportedConstruct(format!(
                        "write to private / private(set) property ${} outside its \
                         declaring class `{}` (PHP forks a dynamic property or Errors; \
                         the record overwrite diverges)",
                        String::from_utf8_lossy(prop),
                        String::from_utf8_lossy(&declaring),
                    )));
                }
                Visibility::Protected => {
                    if let Some(wk) = &writing_key {
                        if chain
                            .iter()
                            .any(|h| normalize_fqcn(h).to_ascii_lowercase() == *wk)
                        {
                            return Ok(());
                        }
                    }
                    return Err(BailReason::UnsupportedConstruct(format!(
                        "write to protected / protected(set) property ${} from outside \
                         the receiver's class chain (PHP visibility Error unmodelled)",
                        String::from_utf8_lossy(prop),
                    )));
                }
            }
        }
        // Declared nowhere → a dynamic property write; allowed (as today).
        Ok(())
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
                // current_class = None: a free function has no class scope, so
                // any non-public property read in its body bails (PHP denies
                // those from global scope too — round 18).
                let ret = run_body_returning_with_names(
                    &func.body,
                    bindings,
                    self,
                    names,
                    &file.contents,
                    None,
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
                // current_class = the DECLARING class (round 18) so property reads
                // in the body enforce PHP visibility from the right scope.
                let ret = run_body_returning_with_names(
                    block,
                    bindings,
                    self,
                    names,
                    &file.contents,
                    Some(&declaring_fqcn),
                )?;
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
        // re-checks scalar coercion (round 3 Task A); symmetric with construct —
        // plus the chain's readonly names (round 18) so a method-body write to a
        // readonly prop bails instead of silently mutating the record.
        let (prop_hints, readonly_props) = self.collect_instance_prop_hints(class);

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
                    readonly_props.clone(),
                    &declaring_fqcn,
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
    /// bare property name, AND the chain's `readonly` property names (round 18) —
    /// plain declarations via the shared collectors plus PROMOTED readonly ctor
    /// params (structurally invisible to the plain-only walkers). Mirrors the
    /// hint-collection in [`BridgeResolver::construct_object`] so a typed
    /// `$this->prop = …` write in an instance method re-checks coercion, and an
    /// instance-method write to a readonly prop bails at the write site (a
    /// dispatch-path mutation of one is PHP's "Cannot modify readonly property"
    /// Error — the record silently mutated, a definitive false Pass).
    fn collect_instance_prop_hints(&self, class: &[u8]) -> (PropHints, HashSet<Vec<u8>>) {
        let codebase = self.project.codebase();
        let Some(class_meta) = codebase.get_class_like(class) else {
            return (PropHints::new(), HashSet::new());
        };
        let mut hint_chain: Vec<Vec<u8>> = class_meta
            .all_parent_classes
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        hint_chain.reverse();
        hint_chain.push(class_meta.name.as_bytes().to_vec());
        let mut prop_hints = PropHints::new();
        let mut readonly_props: HashSet<Vec<u8>> = HashSet::new();
        for fqcn in &hint_chain {
            // A `readonly class` (PHP 8.2) makes EVERY declared property
            // implicitly readonly (round 19). This is the DISPATCH path, so a
            // post-construction mutator write to any of its props must bail —
            // mark them all readonly. (Construction/setUp pass `false`: the ctor
            // legally writes each readonly prop once.)
            let class_is_readonly = codebase
                .get_class_like(&normalize_fqcn(fqcn).to_ascii_lowercase())
                .is_some_and(|m| m.flags.is_readonly());
            let mut seen: Vec<Vec<u8>> = Vec::new();
            self.collect_trait_scalar_property_hints(
                fqcn,
                &mut prop_hints,
                &mut seen,
                &mut readonly_props,
                class_is_readonly,
            );
            self.collect_class_scalar_property_hints(
                fqcn,
                &mut prop_hints,
                &mut readonly_props,
                class_is_readonly,
            );
            // Promoted readonly ctor params are real readonly properties of the
            // hop (round 18) — the plain-only collectors above missed them. A
            // readonly CLASS makes promoted params readonly too (round 19).
            for p in self.promoted_ctor_params_of(fqcn) {
                if p.is_readonly || class_is_readonly {
                    readonly_props.insert(p.name);
                }
            }
        }
        (prop_hints, readonly_props)
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

    /// Construct `new class(args)` (Task B): seed props from the FULL resolvable
    /// ancestor chain's plain literal property defaults (each hop's used traits
    /// flattened in first) + promoted params, then run the constructor body
    /// (property writes permitted) and return the populated record. The
    /// constructor may be inherited (resolved via its own declaring class), so
    /// it is run in its declaring file.
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
        // PHP refuses `new` on an abstract class (Error: "Cannot instantiate
        // abstract class X") — the test ERRORS in PHPUnit. Building a record
        // anyway let such a test reduce to a definitive Pass (false green).
        if class_meta.flags.is_abstract() {
            return Err(BailReason::UnsupportedConstruct(
                "cannot instantiate abstract class (PHP Error)".into(),
            ));
        }
        // PHP refuses `new` on a non-class class-like — "Cannot instantiate
        // trait/interface/enum X" → the test ERRORS in PHPUnit. Building a record
        // for one (the trait branch of the seeder would gladly seed it) reduces
        // to a definitive false green (round 19).
        if !class_meta.kind.is_class() {
            return Err(BailReason::UnsupportedConstruct(format!(
                "cannot instantiate a {} (PHP Error)",
                class_meta.kind.as_str()
            )));
        }
        // Fail-closed: a declared ancestor outside the scanned codebase carries
        // ext-internal state the record cannot model — never construct an empty
        // shell (it would compare as `{}` and diverge from PHPUnit, false green).
        self.bail_unresolvable_declared_ancestry(class, false)?;
        // The trait sibling: a trait used anywhere in the chain (leaf, ancestor,
        // nested trait-of-trait) that is absent from the scan carries an
        // unknowable property set — bail before seeding (fail-closed).
        self.bail_unresolvable_used_traits(class)?;
        // A `__set` anywhere in the chain routes ctor-body writes to UNDECLARED
        // props through arbitrary user code — the record model would store the
        // write verbatim (false green). Bail on its PRESENCE (round 17 fix 2).
        // `__get` is the read-side twin (round 18): an inaccessible/undeclared
        // prop read routes through it legally in PHP, while the record returned
        // the raw slot (false red) — bail on its PRESENCE too.
        self.bail_magic_method_in_chain(class, b"__set", "magic property-write routing")?;
        self.bail_magic_method_in_chain(class, b"__get", "magic property-read routing")?;
        let record_class = class_meta.original_name.as_bytes().to_vec();
        let class_fqcn = class_meta.name.as_bytes().to_vec();

        // 1) Seed plain (non-promoted) literal property defaults across the FULL
        //    resolvable ancestor chain, parents-first (a child redeclaration
        //    overrides a parent's), each hop's used traits BEFORE the hop's own
        //    AST (PHP flattens a trait's properties into the using class; a
        //    legal redeclare must be an IDENTICAL declaration — incompatible is
        //    a PHP fatal — so class-after-trait observes PHP's flattening).
        //    The two bails above guarantee every chain member resolves, so the
        //    walk is total. Leaf-only seeding recorded `{z:7}` vs `{}` for an
        //    inherited default — a structural prop-count mismatch the equality
        //    path compares WITHOUT any property read (false red on assertEquals
        //    AND false green on assertNotEquals).
        let resolver = self;
        let mut chain: Vec<Vec<u8>> = class_meta
            .all_parent_classes
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        chain.reverse();
        chain.push(class_meta.name.as_bytes().to_vec());
        let mut props: Vec<(Vec<u8>, Value)> = Vec::new();
        let mut slots = PropSlotTracker::default();
        for fqcn in &chain {
            let mut trait_seen: Vec<Vec<u8>> = Vec::new();
            self.seed_used_trait_property_defaults(
                fqcn,
                &mut props,
                false,
                &mut trait_seen,
                &mut slots,
            )?;
            self.seed_class_like_property_defaults(fqcn, &mut props, false, &mut slots)?;
            // Promotion declares a REAL property on the hop (round 18): the
            // plain-only seeders above never see it, so a promoted param
            // redeclaring an ancestor's private (or shadowed by a later hop's
            // redeclare) escaped the slot tracker — declare it here. Never
            // seed() it: the value only exists once the ctor actually runs.
            for p in self.promoted_ctor_params_of(fqcn) {
                slots.declare_promoted(&p.name, p.vis)?;
            }
            slots.end_hop();
        }

        // Collect the declared SCALAR property hints (round 3 Task A) across the
        // same chain — symmetric with build_test_case_this — so a typed
        // `$this->prop = …` write in the ctor body re-checks coercion even when the
        // property is DECLARED IN A PARENT. (Leaf-only collection missed inherited
        // hints, letting an un-coerced value through → a divergent Pass/Fail.)
        // Walk parents-first so a child override wins over a parent on a dup name.
        // The chain's `readonly` property names ride along (round 17 fix 4) so a
        // ctor-body write to one bails at the write site.
        let mut prop_hints = PropHints::new();
        let mut readonly_props: HashSet<Vec<u8>> = HashSet::new();
        for fqcn in &chain {
            // A class's used traits BEFORE its own hints (round 5: trait-declared
            // typed scalar props are a SEPARATE set, never in all_parent_classes),
            // so a class-level redeclare wins over a trait's hint.
            let mut seen: Vec<Vec<u8>> = Vec::new();
            self.collect_trait_scalar_property_hints(
                fqcn,
                &mut prop_hints,
                &mut seen,
                &mut readonly_props,
                false,
            );
            self.collect_class_scalar_property_hints(
                fqcn,
                &mut prop_hints,
                &mut readonly_props,
                false,
            );
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
                // PHP refuses `new C()` from a test body when `C::__construct`
                // is private/protected ("Call to private C::__construct() from
                // scope …") → the test ERRORS. Construction happens in
                // out-of-scope test bodies, so a non-public ctor always Errors
                // here; modelling the in-scope `new self()` case is unnecessary —
                // a blanket bail only costs coverage (round 19).
                let ctor_is_public = meta.method_metadata.as_ref().is_none_or(|m| {
                    matches!(m.visibility, mago_codex::visibility::Visibility::Public)
                });
                if !ctor_is_public {
                    return Err(BailReason::UnsupportedConstruct(
                        "new on a class with a non-public constructor \
                         (PHP visibility Error unmodelled)"
                            .into(),
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
                            readonly_props.clone(),
                            &ctor_class,
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
        // Fail-closed on a declared ancestor missing from the scan, EXCEPT the
        // PHPUnit `TestCase` base (never scanned project-only; its internal
        // state is never part of a compared record).
        self.bail_unresolvable_declared_ancestry(class_key.as_bytes(), true)?;
        // The trait sibling: a trait used anywhere in the RESOLVABLE chain that
        // is absent from the scan carries an unknowable property set — bail.
        // (An exempt unresolved `TestCase` is not in `all_parent_classes`, so
        // its traits are out of reach — and out of the compared record.)
        self.bail_unresolvable_used_traits(class_key.as_bytes())?;
        // A `__set` anywhere in the resolvable chain routes setUp writes to
        // UNDECLARED fixture props through arbitrary user code — bail on its
        // PRESENCE (round 17 fix 2; symmetric with `construct_object`), and
        // `__get` likewise on the read side (round 18).
        self.bail_magic_method_in_chain(
            class_key.as_bytes(),
            b"__set",
            "magic property-write routing",
        )?;
        self.bail_magic_method_in_chain(
            class_key.as_bytes(),
            b"__get",
            "magic property-read routing",
        )?;
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
        // so a typed `$this->prop = …` write inside setUp re-checks coercion, and
        // the chain's `readonly` property names (round 17 fix 4) so a setUp write
        // to one bails at the write site.
        let mut prop_hints = PropHints::new();
        let mut readonly_props: HashSet<Vec<u8>> = HashSet::new();
        let mut slots = PropSlotTracker::default();
        for fqcn in &chain {
            // Each hop's used traits BEFORE the hop's own AST (PHP flattens a
            // trait's properties into the using class; a legal class-level
            // redeclare must be identical, so class-after-trait matches PHP).
            let mut trait_seen: Vec<Vec<u8>> = Vec::new();
            self.seed_used_trait_property_defaults(
                fqcn,
                &mut props,
                true,
                &mut trait_seen,
                &mut slots,
            )?;
            self.seed_class_like_property_defaults(fqcn, &mut props, true, &mut slots)?;
            // Promoted ctor params declare real properties on the hop (round
            // 18) — declare (never seed) them so the slot tracker sees the
            // same shadowed-private pairs as on the `new` path.
            for p in self.promoted_ctor_params_of(fqcn) {
                slots.declare_promoted(&p.name, p.vis)?;
            }
            slots.end_hop();
            // A class's used traits BEFORE its own hints (round 5: trait-declared
            // typed scalar props are a SEPARATE set, never in all_parent_classes),
            // so a class-level redeclare wins over a trait's hint.
            let mut seen: Vec<Vec<u8>> = Vec::new();
            self.collect_trait_scalar_property_hints(
                fqcn,
                &mut prop_hints,
                &mut seen,
                &mut readonly_props,
                false,
            );
            self.collect_class_scalar_property_hints(
                fqcn,
                &mut prop_hints,
                &mut readonly_props,
                false,
            );
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
                            readonly_props.clone(),
                            &setup_class,
                        )
                    })
                    .unwrap_or_else(|| {
                        Err(BailReason::Other("could not re-parse setUp file".into()))
                    })?;
        }
        Ok(Some(this))
    }

    /// Seed one class-like's OWN plain literal property defaults into `props`
    /// (class OR trait AST — a trait's properties are flattened into the using
    /// class, so the same member walk applies). `tolerant` selects the
    /// test-case-chain seeder (skip static/hooked/non-literal members; an
    /// unresolvable class-like is silently skipped — its props stay unseeded →
    /// a read bails later) vs the `new`-path seeder (those members BAIL,
    /// frontier §4, and an unresolvable class-like BAILS — the construction
    /// path pre-verified the whole chain resolves, so a miss is unexpected).
    fn seed_class_like_property_defaults(
        &self,
        fqcn: &[u8],
        props: &mut Vec<(Vec<u8>, Value)>,
        tolerant: bool,
        slots: &mut PropSlotTracker,
    ) -> Result<(), BailReason> {
        fn miss(tolerant: bool, what: &str) -> Result<(), BailReason> {
            if tolerant {
                Ok(())
            } else {
                Err(BailReason::Other(format!(
                    "class-like {what} not found after re-parse"
                )))
            }
        }
        let codebase = self.project.codebase();
        let key = normalize_fqcn(fqcn);
        let Some(class_meta) = codebase.get_class_like(&key.to_ascii_lowercase()) else {
            return miss(tolerant, "metadata");
        };
        let Some(file) = self.project.file_of_span(&class_meta.span) else {
            return miss(tolerant, "file");
        };
        let logical = String::from_utf8_lossy(&file.name).into_owned();
        let class_fqcn = class_meta.name.as_bytes().to_vec();
        self.project
            .with_program(&logical, |program, _file, _names| {
                if let Some(class_node) = find_class(program, &class_fqcn) {
                    if tolerant {
                        seed_plain_property_defaults_tolerant(
                            class_node.members.iter(),
                            props,
                            slots,
                        )
                    } else {
                        seed_plain_property_defaults(class_node.members.iter(), props, slots)
                    }
                } else if let Some(trait_node) = find_trait(program, &class_fqcn) {
                    if tolerant {
                        seed_plain_property_defaults_tolerant(
                            trait_node.members.iter(),
                            props,
                            slots,
                        )
                    } else {
                        seed_plain_property_defaults(trait_node.members.iter(), props, slots)
                    }
                } else {
                    miss(tolerant, "AST")
                }
            })
            .unwrap_or_else(|| miss(tolerant, "program"))
    }

    /// Seed the plain literal property defaults of every trait `fqcn` USES into
    /// `props` (recursing into traits a trait itself uses, nested-first so a
    /// using trait's redeclaration wins), exactly like a class AST — PHP
    /// flattens trait properties (defaults AND the untyped-defaultless NULL)
    /// into the using class. Called BEFORE the class's own seeding so a
    /// class-level redeclaration wins (PHP only allows an IDENTICAL one, so
    /// either order observes the same value). `seen` guards a (malformed)
    /// trait-use cycle and dedups mago's populator-flattened `used_traits`.
    /// The callers bail on any absent trait first
    /// ([`Self::bail_unresolvable_used_traits`]), so a lookup miss here is a
    /// defensive dead-path.
    fn seed_used_trait_property_defaults(
        &self,
        fqcn: &[u8],
        props: &mut Vec<(Vec<u8>, Value)>,
        tolerant: bool,
        seen: &mut Vec<Vec<u8>>,
        slots: &mut PropSlotTracker,
    ) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let lower = normalize_fqcn(fqcn).to_ascii_lowercase();
        if seen.contains(&lower) {
            return Ok(());
        }
        seen.push(lower.clone());
        let Some(class_meta) = codebase.get_class_like(&lower) else {
            return Ok(());
        };
        // Snapshot the used-trait FQCNs (the borrow of `class_meta` ends here so
        // the recursive call can re-borrow the codebase).
        let traits: Vec<Vec<u8>> = class_meta
            .used_traits
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        for trait_fqcn in &traits {
            // Skip an already-seeded trait ENTIRELY (mago's populator flattens
            // nested traits into the user's `used_traits`; re-seeding one after
            // its using trait would invert the using-trait-wins order).
            if seen.contains(&normalize_fqcn(trait_fqcn).to_ascii_lowercase()) {
                continue;
            }
            self.seed_used_trait_property_defaults(trait_fqcn, props, tolerant, seen, slots)?;
            self.seed_class_like_property_defaults(trait_fqcn, props, tolerant, slots)?;
        }
        Ok(())
    }

    /// Collect one class's OWN declared scalar property hints into `hints` (round 3
    /// Task A; used by the test-case `$this` builder to walk the ancestor chain so a
    /// typed `$this->prop = …` write in setUp re-checks coercion) and its `readonly`
    /// property names into `readonly` (round 17 fix 4). A child class's hint
    /// overrides a parent's on a duplicate name (the chain is walked
    /// parents-first). A class not on disk is silently skipped.
    fn collect_class_scalar_property_hints(
        &self,
        fqcn: &[u8],
        hints: &mut PropHints,
        readonly: &mut HashSet<Vec<u8>>,
        mark_all_readonly: bool,
    ) {
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
                    hints.extend(collect_scalar_property_hints(
                        class_node.members.iter(),
                        readonly,
                        mark_all_readonly,
                    ));
                } else if let Some(trait_node) = find_trait(program, &class_fqcn) {
                    hints.extend(collect_scalar_property_hints(
                        trait_node.members.iter(),
                        readonly,
                        mark_all_readonly,
                    ));
                }
            });
    }

    /// Walk the DECLARED `extends` chain of `start_key` and BAIL (fail-closed)
    /// when any ancestor class is absent from the scanned codebase.
    ///
    /// Each hop is the AST extends name the scanner recorded
    /// (`direct_parent_class`, present even when the parent never resolved —
    /// mago's `all_parent_classes` may OMIT unknown parents, so it cannot be
    /// trusted for this check). An absent ancestor means ext-internal state the
    /// record cannot carry (\DateTime's wall-clock instant, \Exception's
    /// file/line/trace): constructing it as an empty shell made
    /// `assertEquals(new MyDate(), new MyDate())` compare `{} == {}` → a FALSE
    /// GREEN against PHPUnit's comparator chain. Interfaces carry no state, so
    /// only the extends chain matters.
    ///
    /// `exempt_phpunit_test_case`: the test-case `$this` builder runs with the
    /// PHPUnit `TestCase` base exempt — a project-only (vendor-excluded) scan
    /// never carries it, and the `$this` record's TestCase-internal state is
    /// never compared (assertions on `$this` itself bail elsewhere). The walk
    /// stops there: nothing above an absent class is reachable anyway.
    fn bail_unresolvable_declared_ancestry(
        &self,
        start_key: &[u8],
        exempt_phpunit_test_case: bool,
    ) -> Result<(), BailReason> {
        const TESTCASE_FQCN_LOWER: &[u8] = b"phpunit\\framework\\testcase";
        let codebase = self.project.codebase();
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut current = normalize_fqcn(start_key).to_ascii_lowercase();
        loop {
            if seen.contains(&current) {
                // Declared-extends cycle (malformed source): every hop so far
                // resolved, nothing new to verify.
                return Ok(());
            }
            let Some(meta) = codebase.get_class_like(&current) else {
                // The caller verified `start_key` resolves; defensive only.
                return Ok(());
            };
            let Some(parent) = &meta.direct_parent_class else {
                return Ok(()); // reached a root class: the whole chain resolved.
            };
            let parent_key = normalize_fqcn(parent.as_bytes()).to_ascii_lowercase();
            if codebase.get_class_like(&parent_key).is_none() {
                if exempt_phpunit_test_case && parent_key == TESTCASE_FQCN_LOWER {
                    return Ok(());
                }
                return Err(BailReason::UnsupportedConstruct(format!(
                    "class extends `{}`, a class not in the codebase (ext-internal state unmodelled)",
                    String::from_utf8_lossy(parent.as_bytes()),
                )));
            }
            seen.push(current);
            current = parent_key;
        }
    }

    /// The trait sibling of [`Self::bail_unresolvable_declared_ancestry`]: walk
    /// every trait used anywhere in `start_key`'s RESOLVABLE chain (the leaf,
    /// each ancestor, recursing into traits a trait itself uses, cycle-guarded)
    /// and BAIL (fail-closed) when any is absent from the scanned codebase.
    ///
    /// An absent trait's property set is unknowable: seeding would silently
    /// omit its defaults (the same structural prop-count divergence as the
    /// ancestry hole) and its typed props would take verbatim (un-coerced)
    /// writes — the round-5 documented divergence. PHP itself fatals ("Trait
    /// not found") when the class is declared, so no real verdict is lost.
    ///
    /// `all_parent_classes` only carries RESOLVED ancestors; the ancestry bail
    /// runs first, so the only possibly-unresolved ancestor is an exempt
    /// PHPUnit `TestCase` — out of the compared record by design.
    fn bail_unresolvable_used_traits(&self, start_key: &[u8]) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let leaf_key = normalize_fqcn(start_key).to_ascii_lowercase();
        let Some(class_meta) = codebase.get_class_like(&leaf_key) else {
            // The caller verified `start_key` resolves; defensive only.
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

    /// One hop of [`Self::bail_unresolvable_used_traits`]: check `fqcn`'s own
    /// `used_traits` (recorded at scan time from the AST `use` list, present
    /// even when the trait never resolved) and recurse into each.
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
            return Ok(()); // chain members resolve; defensive only.
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

    /// BAIL (fail-closed) when ANY class-like in the resolvable chain (the leaf,
    /// each ancestor, every used trait at any depth) declares the named magic
    /// property method (round 17 fix 2 for `__set`; round 18 adds `__get`).
    ///
    /// PHP routes a write to an UNDECLARED (or inaccessible) property through
    /// `__set` — arbitrary user code that may drop, rename, or validate the
    /// write — while the record model stores `$this->foo = …` verbatim
    /// (`assertSame(5, $c->foo)` was a FALSE GREEN against a `__set` that drops
    /// the write). `__get` is the read twin: an inaccessible/undeclared read
    /// returns `__get`'s result legally, while the record returned the raw slot
    /// (a false red). The PRESENCE of the method anywhere in the chain bails;
    /// plain dynamic-prop access WITHOUT it matches PHP (gold-verified) and
    /// stays modelled. The ancestry/trait resolvability bails run first, so the
    /// chain walk here is total (an exempt unresolved PHPUnit `TestCase`
    /// declares neither).
    fn bail_magic_method_in_chain(
        &self,
        start_key: &[u8],
        magic: &[u8],
        what: &str,
    ) -> Result<(), BailReason> {
        let codebase = self.project.codebase();
        let leaf_key = normalize_fqcn(start_key).to_ascii_lowercase();
        let Some(class_meta) = codebase.get_class_like(&leaf_key) else {
            return Ok(()); // the caller verified the leaf resolves; defensive only.
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

    /// One hop of [`Self::bail_magic_method_in_chain`]: check `fqcn`'s own declared
    /// magic method, then recurse into each used trait (a trait's magic method is
    /// flattened into the using class). `seen` guards a (malformed) trait-use cycle.
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
            return Ok(()); // chain members resolve; defensive only.
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
    /// `seen` guards against a (malformed) trait-use cycle. A trait absent from
    /// the codebase is a defensive DEAD PATH here: every record-producing path
    /// (`construct_object`, `build_test_case_this`, and instance dispatch on a
    /// record they built) bails on an absent used trait first
    /// ([`Self::bail_unresolvable_used_traits`]), so a hint walk only ever sees
    /// resolved traits — verbatim (un-coerced) writes can no longer slip
    /// through an absent vendor trait's typed props.
    fn collect_trait_scalar_property_hints(
        &self,
        fqcn: &[u8],
        hints: &mut PropHints,
        seen: &mut Vec<Vec<u8>>,
        readonly: &mut HashSet<Vec<u8>>,
        mark_all_readonly: bool,
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
            // trait that redeclares a nested-trait prop wins. `mark_all_readonly`
            // rides along so a trait flattened into a `readonly class` has its
            // props marked readonly too (round 19).
            self.collect_trait_scalar_property_hints(
                trait_fqcn,
                hints,
                seen,
                readonly,
                mark_all_readonly,
            );
            self.collect_class_scalar_property_hints(
                trait_fqcn,
                hints,
                readonly,
                mark_all_readonly,
            );
        }
    }

    /// The PROMOTED constructor params declared by `fqcn`'s OWN `__construct`
    /// AST (round 18). Promotion declares a REAL property on the declaring
    /// class — exactly what the round-17 slot tracker models — but the
    /// plain-declaration walkers never see it: the chain walks use this to
    /// `declare()` the slot (no `seed()` — a promoted prop has no value until
    /// the ctor runs) and the dispatch-path collector to pick up promoted
    /// `readonly` names. A class without its own `__construct` (inherited or
    /// none, checked cheaply on the codex metadata first) or one whose AST
    /// cannot be re-located returns the empty set — the seeding/dispatch paths
    /// already bail on a chain member that genuinely fails to resolve.
    fn promoted_ctor_params_of(&self, fqcn: &[u8]) -> Vec<PromotedCtorParam> {
        let codebase = self.project.codebase();
        let key = normalize_fqcn(fqcn).to_ascii_lowercase();
        // Cheap metadata pre-check: only re-parse a hop that declares its OWN
        // __construct (an inherited ctor's promoted props belong to the
        // declaring ancestor — itself a hop in every chain walk).
        if !codebase.method_is_declared_in_class(&key, b"__construct") {
            return Vec::new();
        }
        let Some(class_meta) = codebase.get_class_like(&key) else {
            return Vec::new();
        };
        let Some(file) = self.project.file_of_span(&class_meta.span) else {
            return Vec::new();
        };
        let logical = String::from_utf8_lossy(&file.name).into_owned();
        let class_fqcn = class_meta.name.as_bytes().to_vec();
        self.project
            .with_program(&logical, |program, _file, _names| {
                let Some(ctor) = find_class_method(program, &class_fqcn, b"__construct") else {
                    return Vec::new();
                };
                ctor.parameter_list
                    .parameters
                    .iter()
                    .filter(|p| p.is_promoted_property())
                    .map(|p| PromotedCtorParam {
                        name: strip_dollar(p.variable.name),
                        vis: param_visibility(p),
                        is_readonly: has_readonly_modifier(p),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// One promoted constructor parameter of a class's own `__construct` (round 18):
/// the bare property name it declares, its primary (read) visibility (a `private`
/// promotion is a separate PHP slot across hops — the slot tracker's concern) and
/// whether it is `readonly` (a body write to it bails).
struct PromotedCtorParam {
    name: Vec<u8>,
    vis: PropVis,
    is_readonly: bool,
}

/// The primary (read) visibility of a promoted constructor parameter (round 19;
/// symmetric with [`plain_property_visibility`]). Asymmetric `private(set)` is a
/// SEPARATE modifier variant and does not lower the read visibility.
fn param_visibility(param: &FunctionLikeParameter) -> PropVis {
    use mago_syntax::ast::ast::modifier::Modifier;
    for m in param.modifiers.iter() {
        match m {
            Modifier::Private(_) => return PropVis::Private,
            Modifier::Protected(_) => return PropVis::Protected,
            Modifier::Public(_) => return PropVis::Public,
            _ => {}
        }
    }
    PropVis::Public
}

/// PHP property visibility, RANKED for the link-time "a redeclaration must not
/// reduce visibility" fatal (round 19): `public` > `protected` > `private`.
/// Asymmetric `private(set)` / `protected(set)` is NOT modelled here — only the
/// primary (read) visibility governs the slot/redeclare rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PropVis {
    Public,
    Protected,
    Private,
}

impl PropVis {
    fn rank(self) -> u8 {
        match self {
            PropVis::Public => 2,
            PropVis::Protected => 1,
            PropVis::Private => 0,
        }
    }
}

/// Per-chain-walk property-SLOT bookkeeping (round 17, fixes 1+3; round 19 adds
/// the visibility-lowering + plain/promoted-duplicate fatals). One hop = one
/// chain member's flattened used traits + its own AST. The name-keyed record
/// model breaks down in places the seeding walk can SEE:
///
/// * **Cross-hop private shadowing** (fix 1): a prop name declared `private`
///   by an earlier hop and REDECLARED by a later hop keeps TWO slots in PHP
///   (`"x":"P":private` AND `"x":"C":private` — a parent-scoped method reads
///   the parent slot). Last-wins seeding collapsed them → a false red on
///   `assertSame` (and a mirrored false green). Bail on the later declaration.
///   Cross-hop public/protected redeclares stay legal single-slot (seeded).
/// * **Within-hop composition conflict** (fix 3): two declarations of the same
///   name flattened into ONE class (trait-vs-trait or trait-vs-class) are
///   PHP's property-composition FATAL unless strictly compatible — a class
///   that never loads must never produce a verdict. The duplicate-SEED check
///   fires REGARDLESS of value: over-bailing the rare legal identical
///   redeclare is accepted (a bail only costs coverage, never truth).
#[derive(Default)]
struct PropSlotTracker {
    /// Prop names DECLARED by a COMPLETED earlier hop, with the (read)
    /// visibility of that declaration (declaration — not just a seeded default —
    /// creates the PHP slot, so a typed defaultless `private int $x;` shadows
    /// too). Round 17 tracked only private names; round 19 keeps the visibility
    /// so a visibility-LOWERING redeclare can be caught.
    earlier: Vec<(Vec<u8>, PropVis)>,
    /// Declarations of the CURRENT hop (promoted into `earlier` at `end_hop`).
    hop: Vec<(Vec<u8>, PropVis)>,
    /// Prop names SEEDED (given a value) by the current hop.
    hop_seeded: Vec<Vec<u8>>,
}

impl PropSlotTracker {
    /// The current hop DECLARES `name` with visibility `vis`. Bails when an
    /// EARLIER hop declared the same name:
    /// * `private` → two distinct PHP slots, unmodelled (round 17 fix 1); or
    /// * with a HIGHER visibility than `vis` → PHP's "Access level to … must be
    ///   public/protected" link-time fatal on a visibility-lowering redeclare
    ///   (round 19) — the class never loads, so a verdict would be false.
    fn declare(&mut self, name: &[u8], vis: PropVis) -> Result<(), BailReason> {
        if let Some((_, prev)) = self.earlier.iter().find(|(n, _)| n == name) {
            if *prev == PropVis::Private {
                return Err(BailReason::UnsupportedConstruct(format!(
                    "property ${} redeclares an ancestor's private property \
                     (shadowed private property slots unmodelled)",
                    String::from_utf8_lossy(name)
                )));
            }
            if vis.rank() < prev.rank() {
                return Err(BailReason::UnsupportedConstruct(format!(
                    "property ${} redeclares an inherited property with lower \
                     visibility (PHP link-time access-level fatal unmodelled)",
                    String::from_utf8_lossy(name)
                )));
            }
        }
        self.hop.push((name.to_vec(), vis));
        Ok(())
    }

    /// A PROMOTED constructor parameter declares `name`: the same cross-hop
    /// checks as [`Self::declare`], PLUS a SAME-HOP duplicate bail — a plain
    /// property AND a promoted param of the same name in ONE class is PHP's
    /// "Cannot redeclare" fatal (round 19; the cross-hop redeclare is legal and
    /// handled by `declare`). The plain/trait seeders run before the promoted
    /// loop, so `self.hop` already holds this hop's plain declarations.
    fn declare_promoted(&mut self, name: &[u8], vis: PropVis) -> Result<(), BailReason> {
        if self.hop.iter().any(|(n, _)| n == name) {
            return Err(BailReason::UnsupportedConstruct(format!(
                "property ${} declared twice in one class (plain property + \
                 promoted constructor parameter — PHP \"Cannot redeclare\" fatal \
                 unmodelled)",
                String::from_utf8_lossy(name)
            )));
        }
        self.declare(name, vis)
    }

    /// The current hop SEEDS `name`: bail on a within-hop duplicate (PHP's
    /// property-composition fatal), else record it.
    fn seed(&mut self, name: &[u8]) -> Result<(), BailReason> {
        if self.hop_seeded.iter().any(|n| n == name) {
            return Err(BailReason::UnsupportedConstruct(format!(
                "property ${} declared twice in one composition \
                 (property composition conflict (PHP fatal) unmodelled)",
                String::from_utf8_lossy(name)
            )));
        }
        self.hop_seeded.push(name.to_vec());
        Ok(())
    }

    /// Close the current hop: its declarations become "earlier" for the next
    /// hop; the within-hop seed set resets (cross-hop redeclares are legal).
    fn end_hop(&mut self) {
        self.earlier.append(&mut self.hop);
        self.hop_seeded.clear();
    }
}

/// The primary (read) visibility of a plain property declaration (round 19;
/// round 17 fix 1 used a `private`-only bool). Asymmetric `private(set)` /
/// `protected(set)` is a SEPARATE modifier variant (`PrivateSet`/`ProtectedSet`)
/// and does NOT lower the read visibility — the loop returns on the FIRST
/// symmetric visibility keyword; no keyword → `public` (PHP's default).
fn plain_property_visibility(
    plain: &mago_syntax::ast::ast::class_like::property::PlainProperty,
) -> PropVis {
    use mago_syntax::ast::ast::modifier::Modifier;
    for m in plain.modifiers.iter() {
        match m {
            Modifier::Private(_) => return PropVis::Private,
            Modifier::Protected(_) => return PropVis::Protected,
            Modifier::Public(_) => return PropVis::Public,
            _ => {}
        }
    }
    PropVis::Public
}

/// Seed plain literal property defaults for a TEST-CASE chain member (class or
/// trait members), TOLERATING (skipping the seeding of) static / non-literal-default
/// properties instead of bailing — but a property HOOK bails (round 19) and a
/// static property is still DECLARED in the slot tracker (so a shadowed slot bails).
///
/// Rationale (frontier, fail-closed-preserving): a base PHPUnit `TestCase` carries
/// static props that are not part of the modelled INSTANCE record — not seeding
/// them is sound (a test that READS an unseeded instance property bails at the read
/// site). But a static name still occupies a PHP slot, so it is declared; and a
/// property hook changes read/write semantics the record cannot model, so it bails.
/// Only a plain, non-static property carrying a LITERAL default is seeded.
fn seed_plain_property_defaults_tolerant<'a>(
    members: impl Iterator<Item = &'a ClassLikeMember<'a>>,
    props: &mut Vec<(Vec<u8>, Value)>,
    slots: &mut PropSlotTracker,
) -> Result<(), BailReason> {
    use mago_syntax::ast::ast::class_like::property::{Property, PropertyItem};
    use mago_syntax::ast::ast::modifier::Modifier;

    for member in members {
        let ClassLikeMember::Property(property) = member else {
            continue; // non-property member: skip.
        };
        let plain = match property {
            Property::Plain(plain) => plain,
            // A property hook (get/set) changes read/write semantics: the record
            // model reads/writes the backing slot RAW and diverges (a virtual
            // hooked prop shadowing an ancestor private returns the parent slot
            // instead of routing through `get`; a setUp write drops the `set`
            // hook). BAIL the test-case build, symmetric with the __get/__set
            // chain bail (round 19; the non-tolerant `new`-path seeder already
            // bails hooks).
            Property::Hooked(_) => {
                return Err(BailReason::UnsupportedConstruct(
                    "property hooks in the test-case chain (read/write routing unmodelled)".into(),
                ));
            }
        };
        let is_static = plain
            .modifiers
            .iter()
            .any(|m| matches!(m, Modifier::Static(_)));
        let vis = plain_property_visibility(plain);
        for item in plain.items.iter() {
            let name = strip_dollar(item.variable().name);
            // Slot bookkeeping is NOT tolerant (round 17 fixes 1+3): a shadowed
            // private slot / composition conflict diverges structurally, so the
            // bail propagates even on the test-case chain. A STATIC declaration
            // still occupies a PHP slot, so declare it too (round 19): a child
            // `static $x` shadowing an ancestor `private $x` is the shadowed-slot
            // divergence (the instance read `$this->x` is undefined-property NULL
            // in PHP, but the record returned the parent's private slot).
            slots.declare(&name, vis)?;
            // Static props are NOT part of an instance record → never seeded (an
            // instance access of one bails: the static check in the read/write
            // visibility guard, or an unseeded read).
            if is_static {
                continue;
            }
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
                slots.seed(&name)?;
                set_prop(props, name, v);
            }
        }
    }
    Ok(())
}

/// Run a constructor AST over a fresh `$this` record: seed promoted params, bind
/// plain params, then run the body with property writes enabled. Returns the
/// populated record.
#[allow(clippy::too_many_arguments)]
fn run_constructor(
    resolver: &BridgeResolver,
    ctor: &Method,
    this: Value,
    args: &[Value],
    names: &mago_names::ResolvedNames,
    source: &[u8],
    mut prop_hints: PropHints,
    mut readonly_props: HashSet<Vec<u8>>,
    declaring_class: &[u8],
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
                // The `set_prop` below IS the readonly prop's legal write-once
                // (PHP seeds the promoted prop from the param). Record the name
                // so any LATER ctor-BODY write to it bails at the write site
                // (round 17 fix 4: PHP raises "Cannot modify readonly property";
                // the record model silently overwrote — a false green). Mutator
                // methods outside the ctor bail independently of this.
                readonly_props.insert(bare.clone());
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
    run_ctor_body_with_names(
        block,
        bindings,
        resolver,
        names,
        source,
        prop_hints,
        readonly_props,
        declaring_class,
    )
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
    readonly: &mut HashSet<Vec<u8>>,
    mark_all_readonly: bool,
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
        // Record `readonly` names BEFORE the scalar-hint filter (round 17 fix 4):
        // readonly props are typically object-typed and would never reach the
        // hint map, but a ctor/setUp body write to one must still bail.
        // `mark_all_readonly` (round 19) is set on the DISPATCH path for a hop
        // declared `readonly class` (PHP 8.2): EVERY property of a readonly class
        // is implicitly readonly, so a post-construction mutator write bails. It
        // is FALSE on the construction/setUp paths (the ctor must legally write
        // each readonly prop once).
        let class_readonly_all = mark_all_readonly
            || plain
                .modifiers
                .iter()
                .any(|m| matches!(m, Modifier::Readonly(_)));
        if class_readonly_all {
            for item in plain.items.iter() {
                readonly.insert(strip_dollar(item.variable().name));
            }
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
/// e.g. `public int $x = 5;`, from class OR trait members (a trait's properties
/// are flattened into the using class). Static / readonly / hooked /
/// non-literal-default properties BAIL (frontier §4). Properties with no
/// default are left unset (a later read of one bails).
fn seed_plain_property_defaults<'a>(
    members: impl Iterator<Item = &'a ClassLikeMember<'a>>,
    props: &mut Vec<(Vec<u8>, Value)>,
    slots: &mut PropSlotTracker,
) -> Result<(), BailReason> {
    use mago_syntax::ast::ast::class_like::property::{Property, PropertyItem};
    use mago_syntax::ast::ast::modifier::Modifier;

    for member in members {
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
                let vis = plain_property_visibility(plain);
                for item in plain.items.iter() {
                    match item {
                        // No default: an UNTYPED `public $x;` is initialized to
                        // NULL by PHP (exact semantics) — leaving it unset made
                        // `{x:null}` vs `{}` a prop-count mismatch (false red).
                        // A TYPED defaultless prop stays UNSET: it is
                        // uninitialized in PHP and absent from both `==` and the
                        // comparator chain (gold-verified) — seeding it NULL
                        // would diverge; a read of it bails later, fail-closed.
                        PropertyItem::Abstract(a) => {
                            let name = strip_dollar(a.variable.name);
                            slots.declare(&name, vis)?;
                            if plain.hint.is_none() {
                                slots.seed(&name)?;
                                set_prop(props, name, Value::Null);
                            }
                        }
                        PropertyItem::Concrete(c) => {
                            let name = strip_dollar(c.variable.name);
                            slots.declare(&name, vis)?;
                            // A non-literal default (function call, new, etc.) bails.
                            let mut scope = Scope::new(HashMap::new(), &NoResolver);
                            let v = super::eval::eval_default(c.value, &mut scope)?;
                            // A typed scalar property coerces its default in PHP
                            // (`public float $x = 5;` stores `float(5.0)`); the
                            // reducer keeps `Int(5)` → fail-closed BAIL on mismatch.
                            if let Some(hint) = &plain.hint {
                                bail_if_scalar_hint_coerces(hint, &v, "property default")?;
                            }
                            slots.seed(&name)?;
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
pub(super) fn strip_dollar(name: &[u8]) -> Vec<u8> {
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
pub(super) fn find_class_method<'a>(
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
pub(super) fn normalize_fqcn(fqcn: &[u8]) -> Vec<u8> {
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
                    // The test-method body's declaring class (round 18) —
                    // mirrors the driver threading `body_class`.
                    Some(class_name.as_bytes()),
                )
            })
            .expect("with_program")
    }

    /// Defense-in-depth for the `$this` builder: a (would-be) test class whose
    /// DECLARED ancestry leaves the scanned codebase through a NON-TestCase
    /// ancestor must bail — its parent's state is unmodelled.
    #[test]
    fn build_this_bails_on_unresolvable_non_testcase_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Code.php"),
            "<?php class WeirdBase extends \\Some\\Vendor\\Base {}\nclass MyTest extends WeirdBase {}",
        )
        .unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        let resolver = BridgeResolver::new(&project);
        let res = resolver.build_test_case_this("MyTest");
        assert!(
            matches!(res, Err(BailReason::UnsupportedConstruct(_))),
            "an unresolvable non-TestCase ancestor must bail the $this build; got {res:?}"
        );
    }

    /// The PHPUnit `TestCase` base is EXEMPT from the unresolvable-ancestry
    /// bail in the `$this` builder: a project-only scan never carries it, and
    /// the `$this` record's TestCase-internal state is never compared
    /// (assertions on `$this` itself bail elsewhere).
    #[test]
    fn build_this_exempts_absent_phpunit_testcase_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Code.php"),
            "<?php class MyTest extends \\PHPUnit\\Framework\\TestCase { public $n = 1; }",
        )
        .unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        let resolver = BridgeResolver::new(&project);
        let res = resolver.build_test_case_this("MyTest");
        assert!(
            matches!(res, Ok(Some(_))),
            "the absent PHPUnit TestCase ancestor is exempt — $this must build; got {res:?}"
        );
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

    // ── Round 17: construction-layer slot-model residuals (4 symmetry-audit fixes) ──

    #[test]
    fn shadowed_private_property_construction_bails() {
        // PHP keeps TWO slots for a private prop redeclared in a subclass
        // ("x":"P":private=1 AND "x":"C":private=2); a parent-scoped method reads
        // the PARENT slot → px() returns 1. The name-keyed record collapses them
        // last-wins to {x:2} → px() returns 2 → assertSame(1, …) FAILS where
        // PHPUnit PASSES (false red; the mirrored assertion is a false green).
        // Fail-closed: shadowed private property slots are unmodelled → BAIL.
        let src = r#"<?php
class ShadowParent {
    private $x = 1;
    public function px(): int { return $this->x; }
}
class ShadowChild extends ShadowParent {
    private $x = 2;
}
class ShadowTest {
    public function testPx(): void {
        $this->assertSame(1, (new ShadowChild())->px());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "ShadowTest", "testPx", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn subclass_public_prop_redeclare_stays_single_slot() {
        // NO-OVER-BAIL control: a subclass redeclaring a parent's PUBLIC prop is
        // legal single-slot PHP (the child's default wins) — must still PASS.
        let src = r#"<?php
class PubParent {
    public $x = 1;
    public function px(): int { return $this->x; }
}
class PubChild extends PubParent {
    public $x = 2;
}
class PubTest {
    public function testPx(): void {
        $this->assertSame(2, (new PubChild())->px());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "PubTest", "testPx", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn magic_set_in_chain_construction_bails() {
        // `__set` routes a write to an UNDECLARED prop through arbitrary user
        // code (here: dropped) — `$c->foo` stays unset in PHP, the test ERRORS;
        // the record model stores {foo:5} verbatim → assertSame(5, …) was a
        // FALSE GREEN. Presence of `__set` anywhere in the chain → BAIL.
        let src = r#"<?php
class DropAll {
    public function __set($name, $value) { }
    public function __construct() { $this->foo = 5; }
}
class DropAllTest {
    public function testFoo(): void {
        $this->assertSame(5, (new DropAll())->foo);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "DropAllTest", "testFoo", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn magic_set_on_ancestor_construction_bails() {
        // The `__set` bail must see the WHOLE resolvable chain, not just the leaf.
        let src = r#"<?php
class SetterBase {
    public function __set($name, $value) { }
}
class SetterChild extends SetterBase {
    public function __construct() { $this->foo = 5; }
}
class SetterChildTest {
    public function testFoo(): void {
        $this->assertSame(5, (new SetterChild())->foo);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "SetterChildTest", "testFoo", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn dynamic_prop_write_without_magic_set_still_reduces() {
        // NO-OVER-BAIL control: a plain dynamic-prop ctor write WITHOUT `__set`
        // in the chain matches PHP (gold-verified) — must still PASS.
        let src = r#"<?php
class Bag {
    public function __construct() { $this->foo = 5; }
}
class BagTest {
    public function testFoo(): void {
        $this->assertSame(5, (new Bag())->foo);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "BagTest", "testFoo", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn build_this_bails_when_chain_declares_magic_set() {
        // The test-case `$this` builder shares the `__set` bail: a fixture write
        // in setUp could be routed through an ancestor's `__set` the same way.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Code.php"),
            "<?php class MagicBase { public function __set($n, $v) {} }\nclass MagicTest extends MagicBase {}",
        )
        .unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        let resolver = BridgeResolver::new(&project);
        let res = resolver.build_test_case_this("MagicTest");
        assert!(
            matches!(res, Err(BailReason::UnsupportedConstruct(_))),
            "a `__set` anywhere in the test-case chain must bail the $this build; got {res:?}"
        );
    }

    #[test]
    fn trait_class_property_composition_conflict_bails() {
        // `trait T { public $x = 1; } class C { use T; public $x = 2; }` is a PHP
        // FATAL at composition time ("…define the same property … incompatible") —
        // the class can never load, so NO verdict can be green. The last-wins seed
        // ({x:2}) made this test PASS. Within-hop duplicate seed → BAIL.
        let src = r#"<?php
trait TX {
    public $x = 1;
}
class CX {
    use TX;
    public $x = 2;
}
class CXTest {
    public function testX(): void {
        $this->assertSame(2, (new CX())->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "CXTest", "testX", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn trait_trait_property_composition_conflict_bails() {
        // Two traits with differing defaults for the same prop in one class:
        // the same composition fatal — BAIL, never a verdict.
        let src = r#"<?php
trait TA {
    public $x = 1;
}
trait TB {
    public $x = 2;
}
class CAB {
    use TA;
    use TB;
}
class CABTest {
    public function testX(): void {
        $this->assertSame(2, (new CAB())->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "CABTest", "testX", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn same_default_trait_composition_over_bails() {
        // DOCUMENTED OVER-BAIL: an IDENTICAL class-level redeclare of a trait
        // prop is legal PHP (compatible composition) and would Pass in PHPUnit.
        // The within-hop duplicate-seed bail fires REGARDLESS of value — the
        // compatibility predicate (visibility+type+default equality) is not
        // modelled, and a bail only costs coverage, never truth. Accepted.
        let src = r#"<?php
trait TS {
    public $x = 1;
}
class CS {
    use TS;
    public $x = 1;
}
class CSTest {
    public function testX(): void {
        $this->assertSame(1, (new CS())->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "CSTest", "testX", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn readonly_promoted_ctor_body_rewrite_bails() {
        // The promoted-param seeding is the readonly prop's legal write-once; the
        // ctor BODY re-write is PHP's "Cannot modify readonly property" Error —
        // the test ERRORS in PHPUnit. The record model overwrote to {x:2} and
        // PASSED (false green). Body write to a readonly name → BAIL.
        let src = r#"<?php
class RoBox {
    public function __construct(public readonly int $x) { $this->x = $x + 1; }
}
class RoBoxTest {
    public function testX(): void {
        $this->assertSame(2, (new RoBox(1))->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "RoBoxTest", "testX", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn readonly_promoted_without_body_rewrite_still_constructs() {
        // NO-OVER-BAIL control: a promoted readonly param WITHOUT a body
        // re-write is the legal write-once — must still construct and PASS.
        let src = r#"<?php
class RoOk {
    public function __construct(public readonly int $x) {}
}
class RoOkTest {
    public function testX(): void {
        $this->assertSame(1, (new RoOk(1))->x);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "RoOkTest", "testX", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn plain_readonly_ctor_body_write_bails() {
        // DOCUMENTED OVER-BAIL: the FIRST ctor-body write to a plain (declared,
        // non-promoted) readonly prop is PHP's legal write-once init and would
        // Pass in PHPUnit. The bail fires on ANY body write to a readonly name —
        // modelling write-once state per prop is not worth the divergence risk
        // of getting it wrong; a bail only costs coverage, never truth. Accepted.
        let src = r#"<?php
class RoPlain {
    public readonly int $x;
    public function __construct() { $this->x = 5; }
}
class RoPlainTest {
    public function testX(): void {
        $this->assertSame(5, (new RoPlain())->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "RoPlainTest", "testX", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn build_this_bails_on_setup_write_to_readonly_prop() {
        // setUp shares the ctor-body write path: a write to a readonly-declared
        // fixture prop bails the $this build (same fail-closed rule as the ctor).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Code.php"),
            "<?php class RoSetupTest {\n    private readonly int $n;\n    protected function setUp(): void { $this->n = 1; }\n}",
        )
        .unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        let resolver = BridgeResolver::new(&project);
        let res = resolver.build_test_case_this("RoSetupTest");
        assert!(
            matches!(res, Err(BailReason::UnsupportedConstruct(_))),
            "a setUp write to a readonly prop must bail the $this build; got {res:?}"
        );
    }

    #[test]
    fn build_this_bails_on_shadowed_private_fixture_prop() {
        // The test-case chain walk shares the shadowed-private bail (a fixture
        // prop redeclared over a parent's private keeps two slots in PHP).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Code.php"),
            "<?php class ShadowBase { private $x = 1; }\nclass ShadowCaseTest extends ShadowBase { private $x = 2; }",
        )
        .unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        let resolver = BridgeResolver::new(&project);
        let res = resolver.build_test_case_this("ShadowCaseTest");
        assert!(
            matches!(res, Err(BailReason::UnsupportedConstruct(_))),
            "a shadowed private fixture prop must bail the $this build; got {res:?}"
        );
    }

    // ── Round 18: promoted slots + readonly on dispatch + read visibility ──

    #[test]
    fn promoted_param_redeclaring_ancestor_private_bails() {
        // Gold (php8.4, runs clean): P keeps its own private $x slot — px()
        // reads P's slot (= 1) even on a C instance whose promoted $x is 2.
        // The name-keyed record collapsed both into {x:2} → px() = 2 → a FALSE
        // GREEN on assertSame(2, ...). Promotion declares a REAL property on C,
        // so the slot tracker must see it and bail the shadowed-private pair.
        let src = r#"<?php
class P { private $x = 1; public function px() { return $this->x; } }
class C extends P { public function __construct(private $x = 2) {} }
class PromShadowTest {
    public function testPx(): void {
        $c = new C();
        $this->assertSame(2, $c->px());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "PromShadowTest", "testPx", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn child_plain_redeclare_over_ancestor_promoted_private_bails() {
        // Mirror: the ANCESTOR's ctor promotes a private $x; the child's plain
        // public $x = 9 is a second PHP slot. The record kept one slot (the
        // inherited ctor's set_prop wrote 5 last) → the external read of the
        // PUBLIC slot returned 5 instead of PHP's 9 (a false red).
        let src = r#"<?php
class PBase { public function __construct(private $x = 5) {} public function px() { return $this->x; } }
class PChild extends PBase { public $x = 9; }
class PromMirrorTest {
    public function testX(): void {
        $c = new PChild();
        $this->assertSame(9, $c->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "PromMirrorTest", "testX", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn promoted_public_over_public_redeclare_still_reduces() {
        // NO-OVER-BAIL control: a promoted PUBLIC param redeclaring a parent's
        // PUBLIC prop is PHP's legal single-slot override — must keep reducing.
        let src = r#"<?php
class PubBase { public $x = 1; }
class PubChild extends PubBase { public function __construct(public $x = 2) {} }
class PromPublicTest {
    public function testX(): void {
        $this->assertSame(2, (new PubChild())->x);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "PromPublicTest", "testX", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn readonly_promoted_write_in_instance_method_bails() {
        // Gold (php8.4): $c->setX(5) is PHP's "Cannot modify readonly property"
        // Error → the test ERRORS in PHPUnit. The dispatch path never threaded
        // the readonly set (and the plain-only collector missed promoted names
        // structurally), so the record mutated → a definitive FALSE Pass.
        let src = r#"<?php
class RoDisp {
    public function __construct(public readonly int $x = 1) {}
    public function setX(int $v): void { $this->x = $v; }
}
class RoDispatchTest {
    public function testSetX(): void {
        $c = new RoDisp();
        $c->setX(5);
        $this->assertSame(5, $c->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "RoDispatchTest", "testSetX", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn readonly_class_non_mutating_method_still_inlines() {
        // NO-OVER-BAIL control: carrying the readonly set into the dispatch
        // scope must not affect a method that only READS the readonly prop.
        let src = r#"<?php
class RoRead {
    public function __construct(public readonly int $x = 3) {}
    public function double(): int { return $this->x * 2; }
}
class RoReadTest {
    public function testDouble(): void {
        $c = new RoRead();
        $this->assertSame(6, $c->double());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "RoReadTest", "testDouble", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn child_method_reading_parent_private_bails() {
        // Gold (php8.4, runs clean): $this->x inside VC is UNDEFINED (VP's slot
        // is private to VP) → Warning + null → the assert FAILS in PHPUnit. The
        // scope-blind record read returned the parent's 1 → a false green.
        let src = r#"<?php
class VP { private $x = 1; }
class VC extends VP { public function cx() { return $this->x; } }
class VisChildTest {
    public function testCx(): void {
        $c = new VC();
        $this->assertSame(1, $c->cx());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "VisChildTest", "testCx", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn external_read_of_private_prop_bails() {
        // Gold (php8.4): a test-body read of a private prop is PHP's "Cannot
        // access private property" Error → the test ERRORS in PHPUnit. The
        // record read returned 5 → a definitive verdict where PHPUnit errors.
        let src = r#"<?php
class VS { private $secret = 5; }
class VisExternalTest {
    public function testRead(): void {
        $s = new VS();
        $this->assertSame(5, $s->secret);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "VisExternalTest", "testRead", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn external_read_of_protected_prop_bails() {
        // Same as the private flavor: an external protected read is a PHP
        // Error (PHPUnit test Error) — never a value.
        let src = r#"<?php
class VPr { protected $p = 7; }
class VisProtExternalTest {
    public function testRead(): void {
        $o = new VPr();
        $this->assertSame(7, $o->p);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "VisProtExternalTest", "testRead", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn private_prop_behind_magic_get_bails() {
        // Gold (php8.4): the inaccessible read routes through __get → 42 → the
        // test PASSES legally. The record returned the raw slot (1) → a false
        // red. __get's PRESENCE anywhere in the chain bails the construction
        // (symmetric with the round-17 __set bail).
        let src = r#"<?php
class VG {
    private $data = 1;
    public function __get($name) { return 42; }
}
class VisMagicGetTest {
    public function testRead(): void {
        $g = new VG();
        $this->assertSame(42, $g->data);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "VisMagicGetTest", "testRead", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn external_public_prop_read_still_reduces() {
        // NO-OVER-BAIL control: public props are the overwhelmingly common
        // case (commonmark HtmlElement) — external reads must keep reducing.
        let src = r#"<?php
class VPub { public $v = 3; }
class VisPublicTest {
    public function testRead(): void {
        $p = new VPub();
        $this->assertSame(3, $p->v);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "VisPublicTest", "testRead", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn self_read_of_own_private_prop_still_reduces() {
        // NO-OVER-BAIL control: a method reading ITS OWN class's private prop
        // is PHP-legal (the doctrine fixture pattern) — must keep reducing.
        let src = r#"<?php
class VOwn {
    private $w = 4;
    public function getW() { return $this->w; }
}
class VisOwnTest {
    public function testRead(): void {
        $o = new VOwn();
        $this->assertSame(4, $o->getW());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "VisOwnTest", "testRead", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn child_method_reading_parent_protected_still_reduces() {
        // NO-OVER-BAIL control: protected reads from anywhere in the
        // receiver's own chain are PHP-legal — must keep reducing.
        let src = r#"<?php
class ProtBase { protected $p = 6; }
class ProtChild extends ProtBase { public function getP() { return $this->p; } }
class VisProtChainTest {
    public function testRead(): void {
        $c = new ProtChild();
        $this->assertSame(6, $c->getP());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "VisProtChainTest", "testRead", vec![]),
            Outcome::Pass
        );
    }

    // ─── Round 19: targeted bails (gate take-9) ──────────────────────────────

    #[test]
    fn child_write_to_parent_private_prop_bails() {
        // Gold (php8.4, runs clean): VWC::setx writes $this->x, but x is private
        // to VWP, so PHP forks a SEPARATE dynamic property — VWP's slot stays 1.
        // px() (in VWP) reads VWP's private x = 1 → assertSame(1, 1) PASSES. The
        // by-value record overwrote the single x slot to 5 → px() read 5 → a
        // definitive false FAIL. The write-visibility guard bails.
        let src = r#"<?php
class VWP { private $x = 1; public function px() { return $this->x; } }
class VWC extends VWP { public function setx($v) { $this->x = $v; } }
class WriteVisTest {
    public function testW(): void {
        $c = new VWC();
        $c->setx(5);
        $this->assertSame(1, $c->px());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "WriteVisTest", "testW", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn private_set_write_from_subclass_bails() {
        // Gold (php8.4): writing a public private(set) prop from a SUBCLASS is
        // "Cannot modify private(set) property P4::$x from scope C4" → ERROR. The
        // record silently overwrote the slot → a definitive false Pass. The
        // write guard consults write_visibility (private for private(set)).
        let src = r#"<?php
class P4 { public private(set) int $x = 1; }
class C4 extends P4 { public function setX(int $v): void { $this->x = $v; } }
class PrivSetWriteTest {
    public function testW(): void {
        $c = new C4();
        $c->setX(5);
        $this->assertSame(5, $c->x);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "PrivSetWriteTest", "testW", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn own_class_write_to_own_private_prop_still_reduces() {
        // NO-OVER-BAIL control: a method writing ITS OWN class's private prop is
        // PHP-legal (the doctrine setUp pattern) — must keep reducing. The write
        // guard allows it (writing class == declaring class).
        let src = r#"<?php
class OwnW {
    private $x = 0;
    public function setIt() { $this->x = 7; }
    public function getIt() { return $this->x; }
}
class OwnWriteTest {
    public function testW(): void {
        $c = new OwnW();
        $c->setIt();
        $this->assertSame(7, $c->getIt());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "OwnWriteTest", "testW", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn new_with_non_public_constructor_bails() {
        // Gold (php8.4): `new SingleC()` from a test body is "Call to private
        // SingleC::__construct() from scope …" → ERROR. The record built a
        // passing object → a definitive false Pass.
        let src = r#"<?php
class SingleC { private function __construct() {} }
class PrivateCtorTest {
    public function testNew(): void {
        $s = new SingleC();
        $this->assertTrue(true);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "PrivateCtorTest", "testNew", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn new_with_public_constructor_still_reduces() {
        // NO-OVER-BAIL control: a public constructor must keep constructing.
        let src = r#"<?php
class PubC { public $v; public function __construct() { $this->v = 9; } }
class PubCtorTest {
    public function testNew(): void {
        $c = new PubC();
        $this->assertSame(9, $c->v);
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "PubCtorTest", "testNew", vec![]),
            Outcome::Pass
        );
    }

    #[test]
    fn new_on_a_trait_bails() {
        // Gold (php8.4): `new BagT()` is "Cannot instantiate trait BagT" → ERROR.
        // construct_object would seed the trait as an instance → a false Pass.
        let src = r#"<?php
trait BagT { public $v = 3; }
class NewTraitTest {
    public function testNew(): void {
        $b = new BagT();
        $this->assertTrue(true);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "NewTraitTest", "testNew", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn visibility_lowering_redeclare_bails() {
        // Gold (php8.4): LvC lowering an inherited public $x to private is a
        // link-time fatal "Access level to LvC::$x must be public" → the class
        // never loads, PHPUnit ERRORS. The record built it → a false Pass.
        let src = r#"<?php
class LvP { public $x = 1; }
class LvC extends LvP { private $x = 2; public function gx() { return $this->x; } }
class LowerVisTest {
    public function testNew(): void {
        $c = new LvC();
        $this->assertSame(2, $c->gx());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "LowerVisTest", "testNew", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn same_hop_plain_and_promoted_duplicate_bails() {
        // Gold (php8.4): a plain `$x` AND a promoted `$x` in one class is "Cannot
        // redeclare WhpC::$x" → fatal, the class never loads, PHPUnit ERRORS. The
        // record built it → a false Pass.
        let src = r#"<?php
class WhpC {
    private $x = 1;
    public function __construct(private $x = 2) {}
    public function gx() { return $this->x; }
}
class DupPropTest {
    public function testNew(): void {
        $c = new WhpC();
        $this->assertSame(2, $c->gx());
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "DupPropTest", "testNew", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn readonly_class_mutator_bails() {
        // Gold (php8.4): a setter on a `readonly class` (PHP 8.2) is "Cannot
        // modify readonly property Ro2::$y" → ERROR. Every prop of a readonly
        // class is implicitly readonly; the record silently overwrote → a false
        // Pass. The dispatch path marks all props readonly.
        let src = r#"<?php
readonly class Ro2 {
    public int $y;
    public function __construct() { $this->y = 1; }
    public function setY(int $v): void { $this->y = $v; }
}
class RoClassTest {
    public function testSet(): void {
        $c = new Ro2();
        $c->setY(5);
        $this->assertSame(5, $c->y);
    }
}
"#;
        assert!(matches!(
            reduce_with_subst(src, "RoClassTest", "testSet", vec![]),
            Outcome::Bailed(_)
        ));
    }

    #[test]
    fn readonly_class_read_only_method_still_reduces() {
        // NO-OVER-BAIL control: a readonly class whose method only READS must
        // keep reducing — construction writes each prop once (legally), and the
        // dispatch-path readonly mark only bails WRITES.
        let src = r#"<?php
readonly class Ro3 {
    public int $y;
    public function __construct() { $this->y = 4; }
    public function doubled(): int { return $this->y * 2; }
}
class RoReadClassTest {
    public function testRead(): void {
        $c = new Ro3();
        $this->assertSame(8, $c->doubled());
    }
}
"#;
        assert_eq!(
            reduce_with_subst(src, "RoReadClassTest", "testRead", vec![]),
            Outcome::Pass
        );
    }
}
