//! Oracle / anti-drift guard: the tree-sitter SharedTransactionalFixture eligibility verdict
//! (in the `discovery` crate) MUST agree with PR #41's reference Mago verdict
//! (`analyzer::reduce::eligibility::transactional_eligibility`) on self-contained, single-class
//! cases. This pins the tree-sitter port to #41's semantics so the two never silently diverge.
//!
//! NOTE: only SINGLE-CLASS cases are compared. The discovery walk additionally folds in-project
//! ANCESTOR setUps across files (fixing #41's same-file-only limitation, eligibility.rs:175-177),
//! so on a cross-file abstract-base fixture discovery is intentionally MORE permissive than Mago.
//! Those cases are out of scope for the agreement oracle by construction.

use std::io::Write;

use analyzer::mago_bridge::MagoProject;
use analyzer::reduce::eligibility::transactional_eligibility;
use discovery::shared_fixture_report_in_dir;

#[test]
fn tx_eligibility_matches_mago_oracle() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("composer.json"), "{}").unwrap();

    // Four single-class cases mirroring eligibility.rs's own unit fixtures:
    // eligible (builder + read-only), no-builder, committing, expectException.
    // Top-level (non-namespaced) classes: the reference `transactional_eligibility` resolves the
    // class AST by scanning top-level `Statement::Class` only (eligibility.rs find_class_ast does
    // not descend into a namespace block), matching #41's own unit-test style. discovery's
    // tree-sitter port handles namespaces too, but that extra coverage is out of oracle scope.
    let src = r#"<?php

class EligibleTest extends \PHPUnit\Framework\TestCase {
    protected function setUp(): void { $this->createSchema(); }
    public function testReads() { $this->assertTrue(true); }
}

class NoBuilderTest extends \PHPUnit\Framework\TestCase {
    protected function setUp(): void { $this->seed(); }
    public function testReads() { $this->assertTrue(true); }
}

class CommittingTest extends \PHPUnit\Framework\TestCase {
    protected function setUp(): void { $this->createSchema(); }
    public function testWrites() { $this->conn->commit(); }
}

class ExpectExceptionTest extends \PHPUnit\Framework\TestCase {
    protected function setUp(): void { $this->createSchema(); }
    public function testThrows() { $this->expectException(\Exception::class); }
}
"#;
    let mut f = std::fs::File::create(dir.path().join("OracleCasesTest.php")).unwrap();
    f.write_all(src.as_bytes()).unwrap();

    let report = shared_fixture_report_in_dir(dir.path()).expect("discovery report");
    let project = MagoProject::load(dir.path()).expect("mago load");

    let expected: &[(&str, bool)] = &[
        ("EligibleTest", true),
        ("NoBuilderTest", false),
        ("CommittingTest", false),
        ("ExpectExceptionTest", false),
    ];

    for (short, want) in expected {
        let entry = report
            .iter()
            .find(|c| c.fqcn.ends_with(short))
            .unwrap_or_else(|| panic!("{short} missing from discovery report"));
        let mago = transactional_eligibility(&project, &entry.fqcn).is_eligible();
        assert_eq!(
            entry.tx_eligible, mago,
            "tree-sitter vs Mago disagree on {short} (fqcn={})",
            entry.fqcn
        );
        assert_eq!(entry.tx_eligible, *want, "wrong verdict on {short}");
    }
}
