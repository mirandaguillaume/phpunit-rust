export TOKEN_WARNING_DISABLED=1
set -e
CARBON=/tmp/phpunit-rust-smoke/carbon
docker run --rm -v "$CARBON":/p -w /p phpunit-rust-bench:php84 \
  sh -c "php -d memory_limit=-1 vendor/bin/phpunit --no-coverage tests/Carbon/DiffTest.php 2>&1 | tail -25"
