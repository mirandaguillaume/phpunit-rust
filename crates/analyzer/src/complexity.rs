//! Cyclomatic complexity extraction from mago AST method bodies.
//!
//! Counts decision points (if/elseif, loops, switch cases, catch, match arms,
//! logical `&&`/`||`/`??`/`?:`). Base complexity per method is 1.

use std::path::PathBuf;

use mago_interner::ThreadedInterner;
use mago_syntax::ast::binary::{Binary, BinaryOperator};
use mago_syntax::ast::control_flow::r#if::{If, IfBody};
use mago_syntax::ast::{
    Block, ClassLikeMember, Expression, Match, MatchArm, MethodBody, Statement, Switch, SwitchCase,
    Try, While,
};

use crate::boundary::{Boundary, BoundaryResolver};
use crate::mago_bridge::MagoProject;

/// Cyclomatic complexity and location data for one class method.
#[derive(Debug, Clone)]
pub struct MethodComplexity {
    pub class: String,
    pub method: String,
    pub file: PathBuf,
    pub start_line: u32,
    pub end_line: u32,
    /// McCabe cyclomatic complexity: 1 (base) + decision points in the body.
    pub cyclomatic: u32,
}

/// Compute cyclomatic complexity for all methods of project-boundary classes.
pub fn compute_all(project: &MagoProject, boundary: &BoundaryResolver) -> Vec<MethodComplexity> {
    let interner = project.interner();
    let mut result = Vec::new();

    for (name, refl) in project.class_likes() {
        let source_id = refl.span.start.source;
        let Some(src) = project.source_by_id(source_id) else {
            continue;
        };
        let file = PathBuf::from(interner.lookup(&src.identifier.0).to_string());

        if boundary.classify(&file) != Boundary::Project {
            continue;
        }

        let class_name = project.class_name_str(name);

        let program = project.get_or_parse(src);

        for (method_key, method_refl) in &refl.methods.members {
            let method_name = interner.lookup(method_key).to_string();
            let start_line = src.line_number(method_refl.span.start.offset) as u32 + 1;
            let end_line = src.line_number(method_refl.span.end.offset) as u32 + 1;

            let cc = navigate(
                program.statements.iter(),
                &class_name,
                &method_name,
                interner,
            )
            .unwrap_or(0);

            result.push(MethodComplexity {
                class: class_name.clone(),
                method: method_name,
                file: file.clone(),
                start_line,
                end_line,
                cyclomatic: 1 + cc,
            });
        }
    }

    result
}

// ── AST navigation ────────────────────────────────────────────────────────────

fn simple_name(class: &str) -> &str {
    class.rsplit('\\').next().unwrap_or(class)
}

/// Walk `stmts` to find the class + method and return its decision-point count.
fn navigate<'s>(
    stmts: impl Iterator<Item = &'s Statement>,
    class: &str,
    method: &str,
    interner: &ThreadedInterner,
) -> Option<u32> {
    let simple = simple_name(class);
    let method_lc = method.to_lowercase();

    for stmt in stmts {
        match stmt {
            Statement::Class(c) if interner.lookup(&c.name.value).eq_ignore_ascii_case(simple) => {
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if interner.lookup(&m.name.value).to_lowercase() == method_lc {
                            if let MethodBody::Concrete(block) = &m.body {
                                return Some(count_block(block, interner));
                            }
                            return Some(0);
                        }
                    }
                }
                return None;
            }
            Statement::Trait(t) if interner.lookup(&t.name.value).eq_ignore_ascii_case(simple) => {
                for member in t.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if interner.lookup(&m.name.value).to_lowercase() == method_lc {
                            if let MethodBody::Concrete(block) = &m.body {
                                return Some(count_block(block, interner));
                            }
                            return Some(0);
                        }
                    }
                }
                return None;
            }
            Statement::Namespace(ns) => {
                use mago_syntax::ast::namespace::NamespaceBody;
                let found = match &ns.body {
                    NamespaceBody::Implicit(b) => {
                        navigate(b.statements.iter(), class, method, interner)
                    }
                    NamespaceBody::BraceDelimited(b) => {
                        navigate(b.statements.iter(), class, method, interner)
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

// ── Decision-point counters ───────────────────────────────────────────────────

fn count_block(block: &Block, interner: &ThreadedInterner) -> u32 {
    block
        .statements
        .iter()
        .map(|s| count_stmt(s, interner))
        .sum()
}

fn count_stmts<'s>(stmts: impl Iterator<Item = &'s Statement>, interner: &ThreadedInterner) -> u32 {
    stmts.map(|s| count_stmt(s, interner)).sum()
}

fn count_stmt(stmt: &Statement, interner: &ThreadedInterner) -> u32 {
    match stmt {
        Statement::If(if_stmt) => count_if(if_stmt, interner),
        Statement::While(w) => 1 + count_while_body(w, interner),
        Statement::DoWhile(dw) => 1 + count_stmt(&dw.statement, interner),
        Statement::For(f) => 1 + count_stmts(f.body.statements().iter(), interner),
        Statement::Foreach(fe) => 1 + count_stmts(fe.body.statements().iter(), interner),
        Statement::Switch(sw) => count_switch(sw, interner),
        Statement::Try(t) => count_try(t, interner),
        Statement::Block(b) => count_block(b, interner),
        Statement::Expression(e) => count_expr(&e.expression),
        Statement::Return(r) => r.value.as_ref().map_or(0, count_expr),
        _ => 0,
    }
}

fn count_if(if_stmt: &If, interner: &ThreadedInterner) -> u32 {
    let mut cc = 1 + count_expr(&if_stmt.condition);
    match &if_stmt.body {
        IfBody::Statement(sb) => {
            cc += count_stmt(&sb.statement, interner);
            for ei in sb.else_if_clauses.iter() {
                cc += 1 + count_expr(&ei.condition) + count_stmt(&ei.statement, interner);
            }
            if let Some(el) = &sb.else_clause {
                cc += count_stmt(&el.statement, interner);
            }
        }
        IfBody::ColonDelimited(cb) => {
            cc += count_stmts(cb.statements.iter(), interner);
            for ei in cb.else_if_clauses.iter() {
                cc += 1 + count_expr(&ei.condition) + count_stmts(ei.statements.iter(), interner);
            }
            if let Some(el) = &cb.else_clause {
                cc += count_stmts(el.statements.iter(), interner);
            }
        }
    }
    cc
}

fn count_while_body(w: &While, interner: &ThreadedInterner) -> u32 {
    use mago_syntax::ast::WhileBody;
    match &w.body {
        WhileBody::Statement(s) => count_stmt(s, interner),
        WhileBody::ColonDelimited(cb) => count_stmts(cb.statements.iter(), interner),
    }
}

fn count_switch(sw: &Switch, interner: &ThreadedInterner) -> u32 {
    sw.body
        .cases()
        .iter()
        .map(|case| match case {
            SwitchCase::Expression(ec) => 1 + count_stmts(ec.statements.iter(), interner),
            SwitchCase::Default(dc) => count_stmts(dc.statements.iter(), interner),
        })
        .sum()
}

fn count_try(t: &Try, interner: &ThreadedInterner) -> u32 {
    let mut cc = count_block(&t.block, interner);
    for catch in t.catch_clauses.iter() {
        cc += 1 + count_block(&catch.block, interner);
    }
    if let Some(finally) = &t.finally_clause {
        cc += count_block(&finally.block, interner);
    }
    cc
}

fn count_expr(expr: &Expression) -> u32 {
    match expr {
        Expression::Binary(b) => count_binary(b),
        Expression::Conditional(c) => {
            1 + count_expr(&c.condition)
                + c.then.as_ref().map_or(0, |e| count_expr(e))
                + count_expr(&c.r#else)
        }
        Expression::Match(m) => count_match(m),
        _ => 0,
    }
}

fn count_binary(b: &Binary) -> u32 {
    let self_cc = match b.operator {
        BinaryOperator::And(_)
        | BinaryOperator::Or(_)
        | BinaryOperator::LowAnd(_)
        | BinaryOperator::LowOr(_)
        | BinaryOperator::Elvis(_)
        | BinaryOperator::NullCoalesce(_) => 1,
        _ => 0,
    };
    self_cc + count_expr(&b.lhs) + count_expr(&b.rhs)
}

fn count_match(m: &Match) -> u32 {
    m.arms
        .iter()
        .map(|arm| match arm {
            MatchArm::Expression(_) => 1,
            MatchArm::Default(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;

    fn make_project_and_boundary(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, MagoProject, BoundaryResolver) {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let target = dir.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).ok();
            std::fs::write(target, content).unwrap();
        }
        let project = MagoProject::load(dir.path()).unwrap();
        let cfg = ProjectConfig {
            root: dir.path().to_path_buf(),
            source_includes: vec![dir.path().join("src")],
            test_suites: vec![dir.path().join("tests")],
            source_excludes: vec![],
        };
        let boundary = BoundaryResolver::from_config(&cfg);
        (dir, project, boundary)
    }

    fn cc(project: &MagoProject, boundary: &BoundaryResolver, class: &str, method: &str) -> u32 {
        compute_all(project, boundary)
            .into_iter()
            .find(|m| m.class.eq_ignore_ascii_case(class) && m.method.eq_ignore_ascii_case(method))
            .map(|m| m.cyclomatic)
            .expect("method not found")
    }

    #[test]
    fn simple_method_is_one() {
        let (_dir, proj, bnd) = make_project_and_boundary(&[(
            "src/A.php",
            "<?php\nclass A {\n  public function foo(): void {}\n}",
        )]);
        assert_eq!(cc(&proj, &bnd, "A", "foo"), 1);
    }

    #[test]
    fn single_if_is_two() {
        let (_dir, proj, bnd) = make_project_and_boundary(&[(
            "src/A.php",
            "<?php\nclass A {\n  public function foo(int $x): int {\n    if ($x > 0) { return 1; }\n    return 0;\n  }\n}",
        )]);
        assert_eq!(cc(&proj, &bnd, "A", "foo"), 2);
    }

    #[test]
    fn if_with_elseif_counts_each_branch() {
        let (_dir, proj, bnd) = make_project_and_boundary(&[(
            "src/A.php",
            "<?php\nclass A {\n  public function foo(int $x): int {\n    if ($x > 0) { return 1; } elseif ($x < 0) { return -1; } else { return 0; }\n  }\n}",
        )]);
        // if=1 + elseif=1 = 2 decision points → CC=3
        assert_eq!(cc(&proj, &bnd, "A", "foo"), 3);
    }

    #[test]
    fn logical_and_adds_one() {
        let (_dir, proj, bnd) = make_project_and_boundary(&[(
            "src/A.php",
            "<?php\nclass A {\n  public function foo(bool $a, bool $b): bool {\n    if ($a && $b) { return true; }\n    return false;\n  }\n}",
        )]);
        // if=1 + &&=1 = 2 → CC=3
        assert_eq!(cc(&proj, &bnd, "A", "foo"), 3);
    }

    #[test]
    fn foreach_adds_one() {
        let (_dir, proj, bnd) = make_project_and_boundary(&[(
            "src/A.php",
            "<?php\nclass A {\n  public function foo(array $xs): void {\n    foreach ($xs as $x) { echo $x; }\n  }\n}",
        )]);
        assert_eq!(cc(&proj, &bnd, "A", "foo"), 2);
    }
}
