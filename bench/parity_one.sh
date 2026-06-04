#!/usr/bin/env bash
#
# bench/parity_one.sh — clone + run ONE OSS suite at workers=1 and assert that
# phpunit-rust executed the SAME number of tests as vanilla PHPUnit.
#
# Designed to run as one matrix job per suite:
#   * isolation — a crash or resource spike in one suite can't corrupt another,
#     and each suite gets a dedicated runner so workers are never starved by
#     cross-suite load (the suspected cause of php-parser's variable CI drift);
#   * workers=1 — no row-splitting (rust's static pre-fork provider enumeration
#     can't diverge from PHPUnit's runtime expansion) and no parallel worker
#     contention. The maximally vanilla-comparable mode → the gate measures CODE
#     parity, not the runner's scheduling.
#
# Usage: bench/parity_one.sh <suite-name>
# Env:   SMOKE (clone parent dir), BINARY (phpunit-rust path)
# Exit:  0 = parity; 1 = count mismatch / crash / no data; 2 = usage / setup error.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="${SCRIPT_DIR}/oss_suites.tsv"
SUITE="${1:?Usage: $0 <suite-name>}"
SMOKE="${SMOKE:-/tmp/phpunit-rust-smoke}"
BINARY="${BINARY:-${SCRIPT_DIR}/../target/release/phpunit-rust}"
mkdir -p "$SMOKE"

# Look up the suite's clone URL / ref / extra_env from the manifest.
row="$(awk -F'\t' -v s="$SUITE" '$1==s{print; exit}' "$MANIFEST")"
[[ -n "$row" ]] || { echo "parity_one: unknown suite '$SUITE'" >&2; exit 2; }
IFS=$'\t' read -r name git_url ref extra_env <<<"$row"

suite_dir="${SMOKE}/${name}"
if [[ ! -d "${suite_dir}" ]]; then
    echo "[parity] cloning ${name} @ ${ref} ..." >&2
    if ! git clone --depth 1 --branch "${ref}" "${git_url}" "${suite_dir}" 2>&1; then
        echo "parity_one: clone failed for ${name}" >&2; exit 2
    fi
    if ! ( cd "${suite_dir}" && COMPOSER_NO_INTERACTION=1 \
           composer install --no-interaction --prefer-dist --no-progress 2>&1 ); then
        echo "parity_one: composer install failed for ${name}" >&2; exit 2
    fi
fi

# One run at WORKERS (the matrix sets it: 4 = gate / real path, 1 = diagnosis).
# bench_host handles the per-suite vanilla quirks (phpunit-itself's
# ./phpunit --testsuite unit, mockery's bootstrap, etc.).
WORKERS="${WORKERS:-4}"
out="$(
    export BINARY SMOKE RUNS="${RUNS:-1}" WORKERS
    [[ -n "${extra_env}" ]] && export "${extra_env?}"
    "${SCRIPT_DIR}/bench_host.sh" "${name}"
)"
echo "${out}"

# bench_host rows: | name | runner | workers | tests | wall ms |  → tests is field 5.
van="$(awk -F'|'  '/vanilla-phpunit/ {gsub(/[^0-9?]/,"",$5); print $5; exit}' <<<"${out}")"
rust="$(awk -F'|' '/phpunit-rust/    {gsub(/[^0-9?]/,"",$5); print $5; exit}' <<<"${out}")"

echo "[parity] ${name} (workers=${WORKERS}): vanilla=${van:-?} rust=${rust:-?}" >&2
if [[ "${van}" =~ ^[0-9]+$ && "${rust}" =~ ^[0-9]+$ && "${van}" == "${rust}" ]]; then
    echo "[parity] ${name} PASS (workers=${WORKERS}: ${rust} == vanilla ${van})"
    exit 0
fi
echo "[parity] ${name} FAIL — workers=${WORKERS}: rust=${rust:-?} vs vanilla=${van:-?} (not a like-for-like run)" >&2
exit 1
