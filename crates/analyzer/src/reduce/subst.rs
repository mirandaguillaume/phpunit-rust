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

use mago_syntax::ast::ast::function_like::function::Function;
use mago_syntax::ast::ast::statement::Statement;
use mago_syntax::ast::Program;

use super::eval::{run_body_returning, BailReason, CallResolver, NoResolver, Scope};
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
}
