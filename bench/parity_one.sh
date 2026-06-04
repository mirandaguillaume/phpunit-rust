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
#
# PHPUNIT_RUST_DUMP_TESTS makes the runner write its exact EXPANDED test list
# (one line per data row: Class::method|Status) — the forensic input that
# --list-tests can't provide. Costs one file write; only read on failure.
WORKERS="${WORKERS:-4}"
DUMP="${TMPDIR:-/tmp}/rust-tests-${name}.txt"
out="$(
    export BINARY SMOKE RUNS="${RUNS:-1}" WORKERS
    export PHPUNIT_RUST_DUMP_TESTS="${DUMP}"
    [[ -n "${extra_env}" ]] && export "${extra_env?}"
    "${SCRIPT_DIR}/bench_host.sh" "${name}"
)"
echo "${out}"

# bench_host rows: | name | runner | workers | tests | wall ms |  → tests is field 5.
van="$(awk -F'|'  '/vanilla-phpunit/ {gsub(/[^0-9?]/,"",$5); print $5; exit}' <<<"${out}")"
rust="$(awk -F'|' '/phpunit-rust/    {gsub(/[^0-9?]/,"",$5); print $5; exit}' <<<"${out}")"

# Forensics: collapse BOTH sides to per-method row counts and print only the
# divergent methods, so a red gate's CI log pinpoints exactly which tests
# diverge — on the machine where they diverge. (The drift never reproduces on
# dev boxes, so this is the only direct observation available.)
# NOTE: vanilla `--list-tests` does NOT apply `<groups><exclude>`, so methods
# from excluded groups show up as vanilla-only noise (van>0 rust=0) — expected.
forensics() {
    if [[ ! -s "${DUMP}" ]]; then
        echo "[forensics] no rust dump at ${DUMP} — skipping" >&2
        return
    fi
    local phpunit_bin="vendor/bin/phpunit" extra=""
    if [[ "${name}" == "phpunit-itself" ]]; then
        phpunit_bin="phpunit"; extra="--testsuite unit"
    fi
    local vlist="${TMPDIR:-/tmp}/van-tests-${name}.txt"
    ( cd "${suite_dir}" && php -d memory_limit=-1 "${phpunit_bin}" ${extra} --list-tests 2>/dev/null ) > "${vlist}" || true

    local vcounts="${vlist}.counts" rcounts="${DUMP}.counts"
    # vanilla lines: " - Class::method#0" / " - Class::method\"named set\"" /
    # " - Class::method" / " - PHPUnit\Framework\SkippedTestCase::Class::method".
    # EXTRACT the leading Class::method instead of stripping suffixes: dataset
    # names can contain quotes/newlines (wrapped entries leave continuation
    # fragments), and extraction drops those fragments instead of mangling them.
    grep -F '::' "${vlist}" \
      | sed -E 's/^[[:space:]]*-[[:space:]]*//; s/^PHPUnit\\Framework\\SkippedTestCase:://' \
      | grep -oE '^[A-Za-z0-9_\\]+::[A-Za-z_][A-Za-z0-9_]*' \
      | sort | uniq -c | awk '{print $2 "\t" $1}' | sort > "${vcounts}"
    sed 's/|[^|]*$//' "${DUMP}" | sort | uniq -c | awk '{print $2 "\t" $1}' | sort > "${rcounts}"

    echo "[forensics] divergent methods (method | vanilla-rows | rust-rows):"
    join -t$'\t' -a1 -a2 -e 0 -o 0,1.2,2.2 "${vcounts}" "${rcounts}" \
      | awk -F'\t' '$2 != $3 {printf "  %s | %s | %s\n", $1, $2, $3}' | head -120
    echo "[forensics] totals: vanilla=$(awk -F'\t' '{s+=$2} END{print s+0}' "${vcounts}")  rust=$(awk -F'\t' '{s+=$2} END{print s+0}' "${rcounts}")"

    echo "[forensics] rust status breakdown for the divergent methods (top 12):"
    join -t$'\t' -a1 -a2 -e 0 -o 0,1.2,2.2 "${vcounts}" "${rcounts}" \
      | awk -F'\t' '$2 != $3 {print $1}' | head -12 \
      | while IFS= read -r m; do
            printf '  %s:' "${m}"
            grep -F "${m}|" "${DUMP}" | cut -d'|' -f2 | sort | uniq -c | awk '{printf "  %s=%s", $2, $1}'
            echo
        done
}

echo "[parity] ${name} (workers=${WORKERS}): vanilla=${van:-?} rust=${rust:-?}" >&2
if [[ "${van}" =~ ^[0-9]+$ && "${rust}" =~ ^[0-9]+$ && "${van}" == "${rust}" ]]; then
    [[ "${PARITY_FORCE_FORENSICS:-0}" == "1" ]] && forensics
    echo "[parity] ${name} PASS (workers=${WORKERS}: ${rust} == vanilla ${van})"
    exit 0
fi
forensics
echo "[parity] ${name} FAIL — workers=${WORKERS}: rust=${rust:-?} vs vanilla=${van:-?} (not a like-for-like run)" >&2
exit 1
