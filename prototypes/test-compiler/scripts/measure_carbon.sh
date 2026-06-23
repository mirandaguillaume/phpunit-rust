export TOKEN_WARNING_DISABLED=1
set -e
CARBON=/tmp/proust-smoke/carbon
RUNS=9
docker run --rm -v "$CARBON":/p -w /p proust-bench:php84 sh -c '
PHPUNIT_BIN=vendor/bin/phpunit
RUNS='"$RUNS"'
# Warm caches first (one untimed run each).
php -d memory_limit=-1 "$PHPUNIT_BIN" --no-coverage tests/Carbon/DiffTest.php >/tmp/w1 2>&1 || true
php -d memory_limit=-1 "$PHPUNIT_BIN" --no-coverage tests/Carbon/DiffCompiledTest.php >/tmp/w2 2>&1 || true

# Extract PHPUnit-internal "Time: 00:00.xxx" too, plus full-process wall-clock.
bench () {
  FILE=$1; LABEL=$2
  i=0
  echo "--- full-process wall-clock ms ($LABEL) ---"
  while [ "$i" -lt "$RUNS" ]; do
    START=$(php -r "echo (int)(microtime(true)*1000);")
    php -d memory_limit=-1 "$PHPUNIT_BIN" --no-coverage "$FILE" >/tmp/out.txt 2>&1
    END=$(php -r "echo (int)(microtime(true)*1000);")
    INT=$(grep -o "Time: [0-9:.]*" /tmp/out.txt | head -1)
    echo "$((END-START))  ($INT)"
    i=$((i+1))
  done
}
bench tests/Carbon/DiffTest.php          ORIGINAL
bench tests/Carbon/DiffCompiledTest.php  COMPILED
'
