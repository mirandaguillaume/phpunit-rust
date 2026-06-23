#!/usr/bin/env bash
# Run --bake-mocks tests inside a Docker container with the given PHP version.
# Usage: ./scripts/docker-test-bake.sh <php-version> <project-path> [extra-args...]
#
# Example:
#   ./scripts/docker-test-bake.sh 8.2-cli /home/gumiranda/service-order
#   ./scripts/docker-test-bake.sh 8.3-cli /home/gumiranda/service-product --filter FooTest

set -euo pipefail

PHP_IMAGE="${1:-php82}"
PROJECT="${2:-$(pwd)}"
shift 2 || true
EXTRA_ARGS=("$@")

BIN=/home/gumiranda/PHPUnit_rust/target/release/proust
PHP_SCRIPTS=/home/gumiranda/PHPUnit_rust/php

if [[ ! -f "$BIN" ]]; then
    echo "Binary not found: $BIN — run 'cargo build --release' first" >&2
    exit 1
fi

echo "=== PHP $PHP_IMAGE | $PROJECT ==="

docker run --rm \
    --tmpfs "/tmp:rw,exec,nosuid,size=${TMPFS_SIZE:-4g}" \
    -v "${BIN}:/proust/target/release/proust:ro" \
    -v "${PHP_SCRIPTS}:/proust/php:ro" \
    -v "${PROJECT}:/app" \
    -w /app \
    "proust-test:${PHP_IMAGE}" \
    /proust/target/release/proust --bake-mocks "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"
