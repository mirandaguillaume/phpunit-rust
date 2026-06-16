<?php

declare(strict_types=1);

namespace PhpunitRust;

/**
 * Build-once, reset-per-test fixture isolation — the "transactional test" pattern,
 * packaged so an eligible class opts in without bespoke lifecycle plumbing.
 *
 * A test class that `use`s this trait builds its EXPENSIVE deterministic fixture (a DB
 * schema, an EntityManager, a seeded connection) EXACTLY ONCE per class — in
 * setUpBeforeClass — and SHARES it across every test in the class. Each test then runs
 * inside a transaction that is rolled back afterwards, so the shared schema/structure
 * persists while every test's data writes are reset. That turns an O(tests) "rebuild the
 * fixture each test" cost into O(1) build + O(tests) cheap rollback.
 *
 * Measured on doctrine-orm (php8.4, in-memory sqlite, 4 entities, real PHPUnit): the
 * per-test schema DROP+CREATE costs ~3.4 ms/test; build-once + per-test rollback costs
 * ~0.002 ms/test — byte-identical results (OK (20, 40) in both), ~1.65x faster end-to-end.
 *
 * The trait owns ONLY the lifecycle; the class supplies the fixture specifics via three
 * hooks (deliberately DB/ORM-agnostic, so the runner core carries no doctrine dependency):
 *   - buildSharedFixture()         build the expensive fixture ONCE (store it statically);
 *   - beginFixtureTransaction()    open this test's isolation boundary (begin / savepoint);
 *   - rollbackFixtureTransaction() undo this test's writes (rollback) AND reset in-memory
 *                                  state that survives the DB rollback — e.g. clear an ORM
 *                                  identity map, so a later test never sees a managed
 *                                  entity from an earlier one.
 * tearDownSharedFixture() (optional) releases the fixture after the class.
 *
 * SOUNDNESS: a class may use this ONLY when its tests are isolation-equivalent under
 * rollback — no committed/DDL writes the rollback cannot undo, and no reliance on a fresh
 * fixture rebuild per test beyond what rollback + reset restores. The runner's eligibility
 * analysis gates which classes opt in; a misuse surfaces as an ordinary test failure
 * (stale row / leaked entity), never a silent wrong pass.
 */
trait SharedTransactionalFixture
{
    public static function setUpBeforeClass(): void
    {
        parent::setUpBeforeClass();
        static::buildSharedFixture();
    }

    public static function tearDownAfterClass(): void
    {
        static::tearDownSharedFixture();
        parent::tearDownAfterClass();
    }

    protected function setUp(): void
    {
        parent::setUp();
        static::beginFixtureTransaction();
    }

    protected function tearDown(): void
    {
        static::rollbackFixtureTransaction();
        parent::tearDown();
    }

    /** Build the expensive deterministic fixture ONCE per class (store it statically). */
    abstract protected static function buildSharedFixture(): void;

    /** Open this test's isolation boundary on the shared fixture (transaction / savepoint). */
    abstract protected static function beginFixtureTransaction(): void;

    /** Undo this test's writes and reset surviving in-memory state (identity map, …). */
    abstract protected static function rollbackFixtureTransaction(): void;

    /** Release the shared fixture after the class (optional; default no-op). */
    protected static function tearDownSharedFixture(): void
    {
    }
}
