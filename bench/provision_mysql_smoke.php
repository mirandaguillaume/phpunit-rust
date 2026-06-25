<?php

declare(strict_types=1);

/**
 * MySQL provisioning gate — exercises the MysqlProvisioner adapter + the
 * dbHandle credential extraction end-to-end against a real MySQL, the one
 * provisioning path not covered by the Postgres parity job. Run inside the CI
 * image (which carries pdo_mysql) against a mysql service; exits non-zero on
 * any divergence.
 *
 * Connection comes from the MY_* env vars (CI-friendly defaults).
 */

require dirname(__DIR__) . '/php/vendor/autoload.php';

use Proust\Provisioning\ProvisionerFactory;
use Proust\TestExecutor;

$host = getenv('MY_HOST') ?: '127.0.0.1';
$port = getenv('MY_PORT') ?: '3306';
$user = getenv('MY_USER') ?: 'root';
$pass = getenv('MY_PASS') ?: 'root';
$db = getenv('MY_DB') ?: 'app_test';
$base = "mysql://$user:$pass@$host:$port/$db";

function check(bool $ok, string $what): void
{
    if (! $ok) {
        fwrite(STDERR, "PROVISION-MYSQL FAIL: $what\n");
        exit(1);
    }
    echo "  ok: $what\n";
}

$opts = [\PDO::ATTR_ERRMODE => \PDO::ERRMODE_EXCEPTION];

// Seed the template: a parent + child (FK) with data — exercises the
// FK_CHECKS=0 copy where table order would otherwise violate the constraint.
echo "== seed template ($db) ==\n";
$admin = new \PDO("mysql:host=$host;port=$port", $user, $pass, $opts);
$admin->exec("DROP DATABASE IF EXISTS `$db`");
$admin->exec("CREATE DATABASE `$db`");
$tpl = new \PDO("mysql:host=$host;port=$port;dbname=$db", $user, $pass, $opts);
$tpl->exec('CREATE TABLE parent(id INT PRIMARY KEY)');
$tpl->exec('CREATE TABLE child(id INT PRIMARY KEY, pid INT, FOREIGN KEY(pid) REFERENCES parent(id))');
$tpl->exec('INSERT INTO parent VALUES (1),(2)');
$tpl->exec('INSERT INTO child VALUES (10,1),(11,2)');

echo "== provision 2 clones ==\n";
$prov = ProvisionerFactory::fromBaseDsn($base);
$dsn0 = $prov->cloneOne('app_test_pr1_w0');
$prov->cloneOne('app_test_pr1_w1');

echo "== assert clone, isolation, dbHandle creds, gc ==\n";
$c0 = new \PDO("mysql:host=$host;port=$port;dbname=app_test_pr1_w0", $user, $pass, $opts);
check((int) $c0->query('SELECT count(*) FROM child')->fetchColumn() === 2, 'clone copied schema+data through the FK table');

$c0->exec('INSERT INTO parent VALUES (99)');
check((int) $tpl->query('SELECT count(*) FROM parent')->fetchColumn() === 2, 'clone write stays isolated from the template');

// dbHandle must extract user=/password= from the DSN (PDO MySQL ignores them in
// the DSN) and connect.
putenv("PROUST_DB_DSN=$dsn0");
$handle = TestExecutor::connection();
check($handle !== null && (int) $handle->query('SELECT count(*) FROM child')->fetchColumn() === 2, 'dbHandle connects via credentials extracted from the DSN');

// Release every connection to the clones before gc — gc deliberately SKIPS a
// clone that still has an active backend (the safety rule), so leaving one open
// is the test's bug, not gc's. Drop $c0 and reset dbHandle's memoized handle.
$c0 = null;
$handle = null;
putenv('PROUST_DB_DSN');
TestExecutor::connection(); // inert path drops the static PDO -> closes it

// MySQL can lag a beat before a closed connection leaves processlist, so
// accumulate gcSweep over a short retry window.
$dropped = [];
for ($i = 0; $i < 25 && count($dropped) < 2; $i++) {
    $dropped = array_values(array_unique([...$dropped, ...$prov->gcSweep()]));
    if (count($dropped) < 2) {
        usleep(200_000);
    }
}
sort($dropped);
check($dropped === ['app_test_pr1_w0', 'app_test_pr1_w1'], 'gc reclaimed both clones (zero-backend)');

echo "PROVISION-MYSQL OK\n";
