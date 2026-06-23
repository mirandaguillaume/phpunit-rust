export TOKEN_WARNING_DISABLED=1
set -e
# Controlled fixture: correctness gate + wall-clock median, original vs compiled.
# Runs the fixtures from INSIDE the carbon project so its native vendor/autoload (with
# phpunit 10) resolves; the fixtures themselves only need TestCase + their own Heavy.php.
# We --no-configuration so carbon's bootstrap/AbstractTestCase is not pulled in.
HERE="$(cd "$(dirname "$0")" && pwd)"
DIR="${1:-$HERE/../controlled}"
CARBON="${CARBON_DIR:-/tmp/proust-smoke/carbon}"
RUNS="${RUNS:-7}"

# Stage the fixtures into the carbon project root, run, then remove them.
cp "$DIR/Heavy.php" "$DIR/HeavyTest.php" "$DIR/HeavyCompiledTest.php" "$CARBON/"
cleanup() { rm -f "$CARBON/Heavy.php" "$CARBON/HeavyTest.php" "$CARBON/HeavyCompiledTest.php"; }
trap cleanup EXIT

docker run --rm -v "$CARBON":/p -w /p proust-bench:php84 sh -c '
PHPUNIT_BIN=vendor/bin/phpunit
RUNS='"$RUNS"'
echo "=== CORRECTNESS: ORIGINAL ===";  php -d memory_limit=-1 "$PHPUNIT_BIN" --no-configuration --no-coverage HeavyTest.php 2>&1 | tail -4
echo "=== CORRECTNESS: COMPILED ===";  php -d memory_limit=-1 "$PHPUNIT_BIN" --no-configuration --no-coverage HeavyCompiledTest.php 2>&1 | tail -4
bench () { i=0; echo "--- timings ms ($2) ---"; while [ "$i" -lt "$RUNS" ]; do
  S=$(php -r "echo (int)(microtime(true)*1000);"); php -d memory_limit=-1 "$PHPUNIT_BIN" --no-configuration --no-coverage "$1" >/tmp/o 2>&1; E=$(php -r "echo (int)(microtime(true)*1000);"); echo $((E-S)); i=$((i+1)); done; }
echo "=== WALLCLOCK ORIGINAL ==="; bench HeavyTest.php ORIGINAL
echo "=== WALLCLOCK COMPILED ==="; bench HeavyCompiledTest.php COMPILED
'
