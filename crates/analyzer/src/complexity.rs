//! Cyclomatic complexity extraction from mago AST method bodies.
//!
//! Counts decision points (if/elseif, loops, switch cases, catch, match arms,
//! logical `&&`/`||`/`??`/`?:`). Base complexity per method is 1.

use std::path::PathBuf;

use mago_syntax::ast::binary::{Binary, BinaryOperator};
use mago_syntax::ast::control_flow::r#if::{If, IfBody};
use mago_syntax::ast::{
    Block, ClassLikeMember, Expression, Match, MatchArm, MethodBody, Statement, Switch, SwitchCase,
    Try, While,
};

use crate::boundary::{Boundary, BoundaryResolver};
use crate::mago_bridge::{MagoProject, word_to_string};

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
    let codebase = project.codebase();
    let mut result = Vec::new();

    for refl in project.class_likes() {
        let Some(src) = project.file_of_span(&refl.span) else {
            continue;
        };
        let file = match &src.path {
            Some(p) => p.clone(),
            None => PathBuf::from(String::from_utf8_lossy(&src.name).into_owned()),
        };

        if boundary.classify(&file) != Boundary::Project {
            continue;
        }

        let class_name = word_to_string(&refl.name);
        let logical_name = String::from_utf8_lossy(&src.name).into_owned();

        for method_word in refl.methods.iter() {
            let Some(method_refl) = codebase.get_method(refl.name.as_bytes(), method_word.as_bytes())
            else {
                continue;
            };
            let method_name = word_to_string(method_word);
            let start_line = src.line_number(method_refl.span.start.offset) + 1;
            let end_line = src.line_number(method_refl.span.end.offset) + 1;

            let cc = project
                .with_program(&logical_name, |program, _file, _names| {
                    navigate(program.statements.iter(), &class_name, &method_name).unwrap_or(0)
                })
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

/// Case-insensitive compare an AST identifier's raw bytes against a `&str`.
fn name_eq_ignore_case(bytes: &[u8], s: &str) -> bool {
    String::from_utf8_lossy(bytes).eq_ignore_ascii_case(s)
}

/// Lowercase an AST identifier's raw bytes into an owned `String`.
fn name_to_lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_lowercase()
}

/// Walk `stmts` to find the class + method and return its decision-point count.
fn navigate<'a, 's>(
    stmts: impl Iterator<Item = &'s Statement<'a>>,
    class: &str,
    method: &str,
) -> Option<u32>
where
    'a: 's,
{
    let simple = simple_name(class);
    let method_lc = method.to_lowercase();

    for stmt in stmts {
        match stmt {
            Statement::Class(c) if name_eq_ignore_case(c.name.value, simple) => {
                for member in c.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if name_to_lower(m.name.value) == method_lc {
                            if let MethodBody::Concrete(block) = &m.body {
                                return Some(count_block(block));
                            }
                            return Some(0);
                        }
                    }
                }
                return None;
            }
            Statement::Trait(t) if name_eq_ignore_case(t.name.value, simple) => {
                for member in t.members.iter() {
                    if let ClassLikeMember::Method(m) = member {
                        if name_to_lower(m.name.value) == method_lc {
                            if let MethodBody::Concrete(block) = &m.body {
                                return Some(count_block(block));
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
                    NamespaceBody::Implicit(b) => navigate(b.statements.iter(), class, method),
                    NamespaceBody::BraceDelimited(b) => navigate(b.statements.iter(), class, method),
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

fn count_block(block: &Block) -> u32 {
    block.statements.iter().map(count_stmt).sum()
}

fn count_stmts<'a, 's>(stmts: impl Iterator<Item = &'s Statement<'a>>) -> u32
where
    'a: 's,
{
    stmts.map(count_stmt).sum()
}

fn count_stmt(stmt: &Statement) -> u32 {
    match stmt {
        Statement::If(if_stmt) => count_if(if_stmt),
        Statement::While(w) => 1 + count_while_body(w),
        Statement::DoWhile(dw) => 1 + count_stmt(dw.statement),
        Statement::For(f) => 1 + count_stmts(f.body.statements().iter()),
        Statement::Foreach(fe) => 1 + count_stmts(fe.body.statements().iter()),
        Statement::Switch(sw) => count_switch(sw),
        Statement::Try(t) => count_try(t),
        Statement::Block(b) => count_block(b),
        Statement::Expression(e) => count_expr(e.expression),
        Statement::Return(r) => r.value.map_or(0, count_expr),
        _ => 0,
    }
}

fn count_if(if_stmt: &If) -> u32 {
    let mut cc = 1 + count_expr(if_stmt.condition);
    match &if_stmt.body {
        IfBody::Statement(sb) => {
            cc += count_stmt(sb.statement);
            for ei in sb.else_if_clauses.iter() {
                cc += 1 + count_expr(ei.condition) + count_stmt(ei.statement);
            }
            if let Some(el) = &sb.else_clause {
                cc += count_stmt(el.statement);
            }
        }
        IfBody::ColonDelimited(cb) => {
            cc += count_stmts(cb.statements.iter());
            for ei in cb.else_if_clauses.iter() {
                cc += 1 + count_expr(ei.condition) + count_stmts(ei.statements.iter());
            }
            if let Some(el) = &cb.else_clause {
                cc += count_stmts(el.statements.iter());
            }
        }
    }
    cc
}

fn count_while_body(w: &While) -> u32 {
    use mago_syntax::ast::WhileBody;
    match &w.body {
        WhileBody::Statement(s) => count_stmt(s),
        WhileBody::ColonDelimited(cb) => count_stmts(cb.statements.iter()),
    }
}

fn count_switch(sw: &Switch) -> u32 {
    sw.body
        .cases()
        .iter()
        .map(|case| match case {
            SwitchCase::Expression(ec) => 1 + count_stmts(ec.statements.iter()),
            SwitchCase::Default(dc) => count_stmts(dc.statements.iter()),
        })
        .sum()
}

fn count_try(t: &Try) -> u32 {
    let mut cc = count_block(&t.block);
    for catch in t.catch_clauses.iter() {
        cc += 1 + count_block(&catch.block);
    }
    if let Some(finally) = &t.finally_clause {
        cc += count_block(&finally.block);
    }
    cc
}

fn count_expr(expr: &Expression) -> u32 {
    match expr {
        Expression::Binary(b) => count_binary(b),
        Expression::Conditional(c) => {
            1 + count_expr(c.condition)
                + c.then.map_or(0, count_expr)
                + count_expr(c.r#else)
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
        | BinaryOperator::NullCoalesce(_) => 1,
        _ => 0,
    };
    self_cc + count_expr(b.lhs) + count_expr(b.rhs)
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
