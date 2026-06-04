#!/usr/bin/env bash
#
# bench/run_in_container.sh — run a bench command INSIDE the CI-faithful PHP
# container (bench/Dockerfile.ci), so counts and timings come from a
# reproducible environment where phpunit-rust is at parity on every suite,
# rather than from the bare runner's PHP build (which produces a count quirk).
#
# The runner-built release binary, php/ (with its installed vendor/), and bench/
# are mounted in; the binary finds its PHP scripts at <bindir>/../../php = /opt/php.
# A writable /out lets the command hand a results file back to the host.
#
# Usage:
#   IMAGE=prust-ci:php84 WORKERS=4 bench/run_in_container.sh bash /work/bench/parity_one.sh carbon
#   OUT_DIR=/tmp RUNS=3 WORKERS=4 bench/run_in_container.sh bash /work/bench/run_oss_bench.sh /out/bench-table.md
#
# Env: IMAGE, RUNS, WORKERS, OUT_DIR (host dir mounted at /out), TMPFS_SIZE.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-prust-ci:php84}"
OUT_DIR="${OUT_DIR:-/tmp}"
mkdir -p "$OUT_DIR"

# --init: tini as PID 1 so signals reach the PHP fork children (no orphan
#         workers if the run is killed).
# --tmpfs /tmp: per-test scratch + cloned suites in RAM, off the overlay fs.
exec docker run --rm --init \
    --tmpfs "/tmp:rw,exec,nosuid,size=${TMPFS_SIZE:-6g}" \
    -v "${REPO}/bench":/work/bench:ro \
    -v "${REPO}/target/release/phpunit-rust":/opt/phpunit-rust/bin/phpunit-rust:ro \
    -v "${REPO}/php":/opt/php:ro \
    -v "${OUT_DIR}":/out \
    -e BINARY=/opt/phpunit-rust/bin/phpunit-rust \
    -e SMOKE=/tmp/smoke \
    -e RUNS="${RUNS:-1}" \
    -e WORKERS="${WORKERS:-4}" \
    "${IMAGE}" "$@"
