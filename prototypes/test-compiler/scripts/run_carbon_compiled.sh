export TOKEN_WARNING_DISABLED=1
set -e
CARBON=/tmp/proust-smoke/carbon
docker run --rm -v "$CARBON":/p -w /p proust-bench:php84 \
  sh -c "php -d memory_limit=-1 vendor/bin/phpunit --no-coverage tests/Carbon/DiffCompiledTest.php 2>&1 | tail -28"
