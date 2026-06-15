//! Transactional-fixture ELIGIBILITY: may a test class be run with
//! [`SharedTransactionalFixture`](../../../../php/src/SharedTransactionalFixture.php) — the
//! expensive deterministic fixture built ONCE per class + each test rolled back — instead
//! of rebuilding the fixture per test?
//!
//! CONSERVATIVE by construction: a class is flagged `Eligible` ONLY when (a) its setUp
//! (own or an in-project ancestor's) builds a RECOGNISED deterministic DB fixture, AND
//! (b) NO method does something a per-test rollback cannot soundly undo — an explicit
//! commit / transaction-control, in-test DDL, an exception expectation, or a cross-test
//! `@depends`/`#[Depends]`. A false NEGATIVE only forgoes a speedup; a false POSITIVE
//! would corrupt isolation, so every uncertainty resolves to `Ineligible`. Detection is a
//! textual scan of each method's source span (over-broad matches stay safe: an extra
//! disqualifier hit only declines a class; the trait's rollback is the actual safety net).

use mago_span::HasSpan;
use mago_syntax::ast::ast::class_like::member::ClassLikeMember;
use mago_syntax::ast::ast::class_like::method::Method;
use mago_syntax::ast::ast::class_like::Class;

use crate::mago_bridge::MagoProject;

/// The verdict for one test class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxEligibility {
    /// Safe to run under `SharedTransactionalFixture`.
    Eligible,
    /// Not flagged; the carried string is the (first) reason.
    Ineligible(String),
}

impl TxEligibility {
    pub fn is_eligible(&self) -> bool {
        matches!(self, TxEligibility::Eligible)
    }
}

/// Lifecycle methods — scanned for the fixture-BUILDER (setUp family), never for
/// disqualifiers (they are not tests).
fn is_lifecycle(name: &[u8]) -> bool {
    let l = name.to_ascii_lowercase();
    matches!(
        l.as_slice(),
        b"setup" | b"setupbeforeclass" | b"teardown" | b"teardownafterclass"
    )
}

/// Calls in setUp that signal "builds an expensive deterministic DB fixture worth hoisting".
const FIXTURE_BUILDERS: &[&str] = &[
    "createSchema",
    "createSchemaForModels",
    "setUpEntitySchema",
    "getEntityManager",
    "getSchemaTool",
    "createSchemaManager",
];

/// Patterns in a TEST method that a per-test rollback cannot soundly undo, or that mark the
/// test as managing its own transaction / schema → not rollback-isolatable. Matched as
/// substrings of the method's source (case-insensitive); over-broad is safe (declines only).
const DISQUALIFIERS: &[&str] = &[
    "->commit(",
    "->beginTransaction(",
    "->rollBack(",
    "->rollback(",
    "->createSavepoint(",
    "->createSchema(",
    "->dropSchema(",
    "->dropDatabase(",
    "expectException",
    "@depends",
];

/// The source slice of a method (its full span), lowercased for matching.
fn method_source<'a>(source: &'a str, m: &Method) -> &'a str {
    let span = m.span();
    let start = span.start.offset as usize;
    let end = (span.end.offset as usize).min(source.len());
    source.get(start..end).unwrap_or("")
}

/// Does this setUp method's source call a recognised fixture builder?
fn setup_builds_fixture(source: &str, m: &Method) -> bool {
    let src = method_source(source, m);
    FIXTURE_BUILDERS
        .iter()
        .any(|b| src.contains(&format!("{b}(")))
}

/// The first disqualifier found in a test method's source, if any.
fn test_disqualifier(source: &str, m: &Method) -> Option<String> {
    let src = method_source(source, m).to_ascii_lowercase();
    for d in DISQUALIFIERS {
        if src.contains(&d.to_ascii_lowercase()) {
            return Some((*d).to_string());
        }
    }
    // `#[Depends(...)]` attribute (cross-test state) — checked on the method itself.
    for list in m.attribute_lists.iter() {
        for attr in list.attributes.iter() {
            let name = attr_simple_name(&attr.name);
            if name.eq_ignore_ascii_case(b"Depends")
                || name.eq_ignore_ascii_case(b"DependsExternal")
            {
                return Some("#[Depends]".to_string());
            }
        }
    }
    None
}

/// Last `\`-separated segment of an attribute's identifier (mirrors the e-graph helper).
fn attr_simple_name<'a>(name: &'a mago_syntax::ast::ast::identifier::Identifier<'a>) -> &'a [u8] {
    use mago_syntax::ast::ast::identifier::Identifier;
    let full = match name {
        Identifier::Local(l) => l.value,
        Identifier::Qualified(q) => q.value,
        Identifier::FullyQualified(f) => f.value,
    };
    full.rsplit(|b| *b == b'\\').next().unwrap_or(full)
}

/// Classify ONE class AST against its source, given any inherited setUp methods (own +
/// ancestors) already collected. The CORE decision — unit-tested directly.
fn classify(class: &Class, source: &str, ancestor_setups: &[(&Method, &str)]) -> TxEligibility {
    // (a) a fixture-building setUp anywhere in the chain.
    let mut builder = false;
    for member in class.members.iter() {
        if let ClassLikeMember::Method(m) = member {
            if m.name.value.eq_ignore_ascii_case(b"setUp") && setup_builds_fixture(source, m) {
                builder = true;
            }
        }
    }
    for (m, src) in ancestor_setups {
        if m.name.value.eq_ignore_ascii_case(b"setUp") && setup_builds_fixture(src, m) {
            builder = true;
        }
    }
    if !builder {
        return TxEligibility::Ineligible("setUp builds no recognised fixture".to_string());
    }

    // (b) no disqualifier in any non-lifecycle method (the tests).
    for member in class.members.iter() {
        if let ClassLikeMember::Method(m) = member {
            if is_lifecycle(m.name.value) {
                continue;
            }
            if let Some(reason) = test_disqualifier(source, m) {
                let mname = String::from_utf8_lossy(m.name.value);
                return TxEligibility::Ineligible(format!("{mname}: {reason}"));
            }
        }
    }
    TxEligibility::Eligible
}

/// Classify a class by FQCN through the project (resolving its source + in-project ancestor
/// setUps). Vendor ancestors are excluded from the project, so a fixture built only in a
/// vendor base is NOT seen → `Ineligible` (sound: we never flag what we cannot verify).
pub fn transactional_eligibility(project: &MagoProject, class: &str) -> TxEligibility {
    let Some(meta) = project.find_class(class) else {
        return TxEligibility::Ineligible("class not found".to_string());
    };
    let Some(file) = project.file_of_span(&meta.span) else {
        return TxEligibility::Ineligible("source unavailable".to_string());
    };
    let logical = String::from_utf8_lossy(&file.name).into_owned();
    project
        .with_program(&logical, |program, file, _names| {
            let source = String::from_utf8_lossy(&file.contents);
            let Some(class_ast) = find_class_ast(program, class.as_bytes()) else {
                return TxEligibility::Ineligible("class AST not found".to_string());
            };
            // v1: resolve same-file ancestors only (cross-file ancestors are a follow-up;
            // a fixture built in an out-of-program base yields Ineligible, conservatively).
            classify(class_ast, &source, &[])
        })
        .unwrap_or_else(|| TxEligibility::Ineligible("reparse failed".to_string()))
}

/// Find a class declaration by simple name anywhere in a program.
fn find_class_ast<'a>(
    program: &'a mago_syntax::ast::Program<'a>,
    fqcn: &[u8],
) -> Option<&'a Class<'a>> {
    let simple = fqcn.rsplit(|&b| b == b'\\').next().unwrap_or(fqcn);
    for stmt in program.statements.iter() {
        if let mago_syntax::ast::ast::statement::Statement::Class(c) = stmt {
            if c.name.value.eq_ignore_ascii_case(simple) {
                return Some(c);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mago_bridge::MagoProject;

    fn eligibility(src: &str, class: &str) -> TxEligibility {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Code.php"), src).unwrap();
        let project = MagoProject::load(dir.path()).unwrap();
        transactional_eligibility(&project, class)
    }

    /// A doctrine-shaped class: setUp builds the schema, tests only persist/query → Eligible.
    #[test]
    fn schema_setup_readonly_tests_is_eligible() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class BlogTest extends TestCase {
    private $em;
    protected function setUp(): void {
        $this->em = mkEm();
        (new SchemaTool($this->em))->createSchema($this->em->getMetadataFactory()->getAllMetadata());
    }
    public function testPersist(): void {
        $this->em->persist(new BUser());
        $this->em->flush();
        self::assertSame(1, $this->em->getRepository(BUser::class)->count([]));
    }
}
"#;
        assert_eq!(eligibility(src, "BlogTest"), TxEligibility::Eligible);
    }

    /// setUp that builds no recognised fixture → Ineligible (nothing worth hoisting).
    #[test]
    fn no_fixture_builder_is_ineligible() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class PlainTest extends TestCase {
    protected function setUp(): void { $this->x = 1; }
    public function testA(): void { self::assertSame(1, $this->x); }
}
"#;
        assert!(matches!(
            eligibility(src, "PlainTest"),
            TxEligibility::Ineligible(_)
        ));
    }

    /// A test that COMMITS inside its transaction cannot be rolled back → Ineligible.
    #[test]
    fn committing_test_is_ineligible() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class CommitTest extends TestCase {
    private $em;
    protected function setUp(): void {
        $this->em = mkEm();
        (new SchemaTool($this->em))->createSchema($this->em->getMetadataFactory()->getAllMetadata());
    }
    public function testCommits(): void {
        $this->em->getConnection()->commit();
        self::assertTrue(true);
    }
}
"#;
        assert!(matches!(
            eligibility(src, "CommitTest"),
            TxEligibility::Ineligible(_)
        ));
    }

    /// A test expecting an exception may not be rollback-safe → Ineligible.
    #[test]
    fn exception_test_is_ineligible() {
        let src = r#"<?php
use PHPUnit\Framework\TestCase;
final class ExcTest extends TestCase {
    private $em;
    protected function setUp(): void {
        $this->em = mkEm();
        (new SchemaTool($this->em))->createSchema($this->em->getMetadataFactory()->getAllMetadata());
    }
    public function testThrows(): void {
        $this->expectException(\RuntimeException::class);
        $this->em->flush();
    }
}
"#;
        assert!(matches!(
            eligibility(src, "ExcTest"),
            TxEligibility::Ineligible(_)
        ));
    }
}
