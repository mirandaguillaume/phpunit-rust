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
    make_object, run_body_returning, run_ctor_body, BailReason, CallResolver, NoResolver, Scope,
};
use super::value::Value;
use crate::mago_bridge::MagoProject;

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
    ) -> Result<Option<Value>, BailReason> {
        // The class comes from the RUNTIME receiver record (never a static type).
        let Value::Object { class, .. } = this else {
            return Ok(None);
        };
        self.with_depth(|s| s.inline_method(class, method, Some(this.clone()), args))
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
            .with_program(&logical_name, |program, _file, _names| {
                let func = find_function(program, name)
                    .ok_or_else(|| BailReason::UnknownCall(String::from_utf8_lossy(name).into()))?;
                let bindings = bind_params(func, args)?;
                // Recurse through THIS resolver so nested user calls inline too.
                run_body_returning(&func.body, bindings, self).map(Some)
            });

        match outcome {
            Some(r) => r,
            // `with_program` returned None → the file index lookup missed.
            None => Err(BailReason::Other("could not re-parse callee file".into())),
        }
    }

    /// Run `f` under the recursion-depth guard (shared by every inlining entry).
    fn with_depth(
        &self,
        f: impl FnOnce(&Self) -> Result<Option<Value>, BailReason>,
    ) -> Result<Option<Value>, BailReason> {
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
        // Resolve the concrete declaring method (follows inheritance/traits).
        let Some(meta) = codebase.get_declaring_method(class, method) else {
            return Ok(None);
        };
        // Abstract dispatch → bail (frontier §5).
        if meta
            .method_metadata
            .as_ref()
            .is_some_and(|m| m.is_abstract)
        {
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
            .with_program(&logical_name, |program, _file, _names| {
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
                run_body_returning(block, bindings, self).map(Some)
            });
        outcome.unwrap_or_else(|| Err(BailReason::Other("could not re-parse method file".into())))
    }

    /// Construct `new class(args)` (Task B): seed props from plain literal property
    /// defaults + promoted params, then run the constructor body (property writes
    /// permitted) and return the populated record. The constructor may be inherited
    /// (resolved via its own declaring class), so it is run in its declaring file.
    fn construct_object(
        &self,
        class: &[u8],
        args: &[Value],
    ) -> Result<Option<Value>, BailReason> {
        let codebase = self.project.codebase();
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

        // 1) Seed plain (non-promoted) property declarations with literal defaults.
        //    (Read off THIS class's own AST. Inherited-property defaults are not
        //    modelled in v2 — a read of an unseeded prop bails, fail-closed.)
        let resolver = self;
        let mut props: Vec<(Vec<u8>, Value)> = self
            .project
            .with_program(&class_logical, |program, _file, _names| {
                let Some(class_node) = find_class(program, &class_fqcn) else {
                    return Err(BailReason::Other("class AST not found after re-parse".into()));
                };
                let mut p: Vec<(Vec<u8>, Value)> = Vec::new();
                seed_plain_property_defaults(class_node, &mut p, resolver)?;
                Ok(p)
            })
            .unwrap_or_else(|| Err(BailReason::Other("could not re-parse class file".into())))?;

        // 2) Run the constructor (promoted-param seeding + body). Resolved through
        //    the DECLARING class so an inherited constructor inlines correctly.
        let ctor_meta = codebase.get_declaring_method(class, b"__construct");
        match ctor_meta {
            Some(meta) => {
                if meta
                    .method_metadata
                    .as_ref()
                    .is_some_and(|m| m.is_abstract)
                {
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
                    .with_program(&ctor_logical, |program, _file, _names| {
                        let ctor = find_class_method(program, &ctor_class, b"__construct")
                            .ok_or_else(|| {
                                BailReason::Other("constructor AST not found after re-parse".into())
                            })?;
                        run_constructor(resolver, ctor, this.clone(), args)
                    })
                    .unwrap_or_else(|| {
                        Err(BailReason::Other("could not re-parse constructor file".into()))
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
}

/// Run a constructor AST over a fresh `$this` record: seed promoted params, bind
/// plain params, then run the body with property writes enabled. Returns the
/// populated record.
fn run_constructor(
    resolver: &BridgeResolver,
    ctor: &Method,
    this: Value,
    args: &[Value],
) -> Result<Value, BailReason> {
    let Value::Object {
        class,
        mut props,
    } = this
    else {
        return Err(BailReason::Other("constructor $this is not an object".into()));
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
        // A promoted property seeds `$this->name` directly (PHP semantics).
        if param.is_promoted_property() {
            if has_readonly_modifier(param) {
                // readonly is fine for a write-once seed at construction time, so
                // we allow it here; any LATER mutation of the prop bails because
                // mutator methods bail. (No special handling needed.)
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
    run_ctor_body(block, bindings, resolver)
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
                        return Err(BailReason::UnsupportedConstruct(
                            "static property".into(),
                        ));
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
                            let name = strip_dollar(c.variable.name);
                            set_prop(props, name, v);
                        }
                    }
                }
            }
            // Property hooks (get/set) change read/write semantics → bail.
            Property::Hooked(_) => {
                return Err(BailReason::UnsupportedConstruct(
                    "property hooks".into(),
                ))
            }
        }
    }
    Ok(())
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
        bindings.insert(key, value);
    }

    Ok(bindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reduce::eval::{run_method_body, Outcome};
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
            .with_program(&logical, |program, _file, _names| {
                let block = find_method_block(program, method).expect("method block");
                run_method_body(block, given_map, &resolver)
            })
            .expect("with_program")
    }

    fn find_method_block<'a>(
        program: &'a mago_syntax::ast::Program<'a>,
        method: &str,
    ) -> Option<&'a mago_syntax::ast::ast::block::Block<'a>> {
        use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
        use mago_syntax::ast::ast::class_like::method::MethodBody;
        for stmt in program.statements.iter() {
            if let Statement::Class(class) = stmt {
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
        // A method that writes $this->prop is a mutator — the by-value model gets
        // aliasing wrong, so inlining it must BAIL (frontier §2).
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
}
