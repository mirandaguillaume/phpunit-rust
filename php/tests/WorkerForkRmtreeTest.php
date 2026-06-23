<?php

declare(strict_types=1);

namespace Proust\Tests;

use PHPUnit\Framework\TestCase;

/**
 * Finding (1): the per-child TMPDIR shutdown cleanup must use lstat semantics.
 *
 * The recursive remove closure historically decided recursion with
 * is_dir($sub), which FOLLOWS symlinks. A test that writes a symlink into its
 * TMPDIR pointing at a directory OUTSIDE the worker temp (PHPUnit's own
 * fixtures legitimately do this) would have the link target's CONTENTS deleted
 * at every worker exit — silent data loss outside the sandbox.
 *
 * The fix lifts the recursion logic into a standalone, side-effect-free helper
 * `proust_rmtree()` defined in worker_fork.php (guarded so that requiring
 * the file for the function definition does NOT run the fork-pool master), and
 * gives it lstat semantics: a symlink is @unlink'd as a link (never followed),
 * a real directory is recursed into, everything else is @unlink'd.
 */
final class WorkerForkRmtreeTest extends TestCase
{
    private string $root;

    protected function setUp(): void
    {
        // Loading worker_fork.php must NOT run the master. The main-body guard
        // keys on the entry script being worker_fork.php itself; under PHPUnit
        // it is the phpunit binary, so requiring it only defines the helpers.
        require_once __DIR__ . '/../worker_fork.php';

        self::assertTrue(
            function_exists('proust_rmtree'),
            'worker_fork.php must expose proust_rmtree() for cleanup'
        );

        $this->root = sys_get_temp_dir() . '/proust-rmtree-test-' . getmypid() . '-' . uniqid();
        @mkdir($this->root, 0700, true);
    }

    protected function tearDown(): void
    {
        // Defensive: remove anything the assertions left behind. Uses the same
        // lstat-safe helper so this teardown can't itself follow a stray link.
        if (is_dir($this->root)) {
            proust_rmtree($this->root);
        }
    }

    /**
     * The core regression: a symlink inside the tree pointing at an EXTERNAL
     * directory must be removed as a link, leaving the target and its contents
     * untouched.
     */
    public function testRmtreeDoesNotFollowSymlinkToExternalDir(): void
    {
        // The "precious" external directory that a fixture's symlink points at.
        $external = $this->root . '/external_target';
        @mkdir($external, 0700, true);
        $precious = $external . '/precious.txt';
        file_put_contents($precious, 'must survive');

        // The worker temp dir, with a symlink that escapes to $external.
        $workerTmp = $this->root . '/worker_tmp';
        @mkdir($workerTmp, 0700, true);
        file_put_contents($workerTmp . '/scratch.txt', 'disposable');
        $link = $workerTmp . '/escape_link';
        self::assertTrue(symlink($external, $link), 'failed to create test symlink');

        // Sanity: is_dir() follows the link (this is exactly the trap).
        self::assertTrue(is_dir($link), 'precondition: is_dir follows the symlink');
        self::assertTrue(is_link($link), 'precondition: the entry is a symlink');

        proust_rmtree($workerTmp);

        // The worker temp dir is gone…
        self::assertDirectoryDoesNotExist($workerTmp, 'worker tmp must be removed');
        // …but the external target and its file are untouched.
        self::assertDirectoryExists($external, 'external symlink target must survive');
        self::assertFileExists($precious, 'contents behind the symlink must survive');
        self::assertSame('must survive', file_get_contents($precious));
    }

    /**
     * A symlink to an external FILE must be unlinked as a link, never deleting
     * the pointed-at file.
     */
    public function testRmtreeDoesNotFollowSymlinkToExternalFile(): void
    {
        $externalFile = $this->root . '/external_file.txt';
        file_put_contents($externalFile, 'keep me');

        $workerTmp = $this->root . '/worker_tmp_file';
        @mkdir($workerTmp, 0700, true);
        $link = $workerTmp . '/file_link';
        self::assertTrue(symlink($externalFile, $link), 'failed to create file symlink');

        proust_rmtree($workerTmp);

        self::assertDirectoryDoesNotExist($workerTmp);
        self::assertFileExists($externalFile, 'symlinked external file must survive');
        self::assertSame('keep me', file_get_contents($externalFile));
    }

    /**
     * A symlink AT THE TOP of the path being removed must also be treated as a
     * link (guard the top path the same way for completeness): removing a tree
     * whose root is itself a symlink must unlink the link, not nuke the target.
     */
    public function testRmtreeHandlesSymlinkAtTopPath(): void
    {
        $external = $this->root . '/top_external';
        @mkdir($external, 0700, true);
        file_put_contents($external . '/keep.txt', 'top survive');

        $topLink = $this->root . '/top_link';
        self::assertTrue(symlink($external, $topLink), 'failed to create top symlink');

        proust_rmtree($topLink);

        self::assertFalse(is_link($topLink), 'the top symlink must be removed');
        self::assertDirectoryExists($external, 'top symlink target dir must survive');
        self::assertFileExists($external . '/keep.txt', 'contents behind top symlink must survive');
    }

    /**
     * The ordinary case is unchanged: a nested real-directory tree with files
     * is fully removed.
     */
    public function testRmtreeRemovesNestedRealTree(): void
    {
        $tmp = $this->root . '/real_tree';
        @mkdir($tmp . '/a/b/c', 0700, true);
        file_put_contents($tmp . '/top.txt', '1');
        file_put_contents($tmp . '/a/mid.txt', '2');
        file_put_contents($tmp . '/a/b/c/deep.txt', '3');

        proust_rmtree($tmp);

        self::assertDirectoryDoesNotExist($tmp, 'the whole real tree must be removed');
    }
}
