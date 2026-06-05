#!/usr/bin/env bash
#
# bench/check_parity.sh — strict parity gate for the daily OSS benchmark.
#
# An unattended bench must NEVER publish numbers from a run where phpunit-rust
# and vanilla PHPUnit executed a DIFFERENT number of tests. A worker crash that
# silently drops outcomes is indistinguishable from a blazing-fast run — the
# wall-time cannot detect its own corruption, only the test COUNT can. (See the
# PR that introduced this gate: brick-math once reported 566/20392 tests and a
# bogus "38.6×" speedup because the fork-pool workers fatal-ed at startup.)
#
# Policy (chosen deliberately): STRICT equality, KEEP all suites. Suites with a
# known parity gap — faker / php-parser (data-provider row enumeration) and
# phpunit-itself (`.phpt` tests phpunit-rust does not run) — WILL fail this gate
# until their gap is closed. That is by design: a parity gap is a loud, blocking
# failure rather than silently dropped coverage.
#
# Usage:
#   bench/check_parity.sh /tmp/bench-table.md
#
# Exit codes: 0 = every suite ran the same count; 1 = at least one diverged (or
# the table had no data rows); 2 = usage / missing-file error.
#
# WORKER-DEATH BLIND SPOT (documented, intentional): this gate sees ONLY the
# rendered counts table. A worker death on single-row methods synthesises
# EXACTLY one Error per lost method, so the COUNT is preserved and a run where
# tests never executed still reads as parity. parity_one.sh closes that hole by
# scanning the runner's expanded dump (PHPUNIT_RUST_DUMP_TESTS) — but the daily
# bench path (bench_host.sh → run_oss_bench.sh) never enables that dump and
# hands this script the formatted table alone, with no per-test rows. There is
# therefore nothing here to scan by default; the count-equality verdict is the
# strongest signal the table exposes. To let this gate ALSO refuse death runs,
# set PHPUNIT_RUST_DEATH_DUMPS to a glob of dump files (e.g. the per-suite
# /tmp/rust-tests-*.txt parity_one.sh writes); when present and non-empty, any
# worker-death marker found across them fails the gate regardless of counts.

set -euo pipefail

TABLE="${1:?Usage: $0 <bench-table.md>}"
[[ -f "$TABLE" ]] || { echo "check_parity: table not found: $TABLE" >&2; exit 2; }

# Same markers the runner/worker write into a dump's message field; see
# parity_one.sh for the per-marker source. "worker process " (trailing space)
# matches died/crashed/terminated without tripping on benign "worker" text.
WORKER_DEATH_RE='worker process '

fail=0
checked=0
printf '%-16s %10s %10s   %s\n' "SUITE" "VANILLA" "RUST" "VERDICT"
printf '%-16s %10s %10s   %s\n' "----------------" "----------" "----------" "-------"

while IFS= read -r line; do
    # Only data rows (markdown table rows start with '|').
    [[ "$line" == \|* ]] || continue

    # Column 1 = suite name, column 2 = "vanilla / rust" test counts.
    name=$(awk -F'|' '{gsub(/^[[:space:]]+|[[:space:]]+$/, "", $2); print $2}' <<<"$line")
    counts=$(awk -F'|' '{gsub(/^[[:space:]]+|[[:space:]]+$/, "", $3); print $3}' <<<"$line")

    # Skip the header row and the |---| separator.
    [[ "$name" == "Project" || "$name" == ---* || -z "$name" ]] && continue

    van=$(awk -F'/'  '{gsub(/[[:space:]]/, "", $1); print $1}' <<<"$counts")
    rust=$(awk -F'/' '{gsub(/[[:space:]]/, "", $2); print $2}' <<<"$counts")

    checked=$((checked + 1))
    if [[ "$van" =~ ^[0-9]+$ && "$rust" =~ ^[0-9]+$ && "$van" == "$rust" ]]; then
        verdict="PASS"
    else
        verdict="FAIL"
        fail=1
    fi
    printf '%-16s %10s %10s   %s\n' "$name" "${van:-?}" "${rust:-?}" "$verdict"
done < "$TABLE"

echo

# Opportunistic worker-death scan. The table can't expose deaths (counts are
# preserved by the one-Error-per-lost-method synthesis), so this only fires
# when the caller hands us the runner's expanded dumps via the env glob. When
# it does, a single death marker fails the gate even if every count matched —
# the same death-despite-matching-counts guarantee parity_one.sh enforces.
if [[ -n "${PHPUNIT_RUST_DEATH_DUMPS:-}" ]]; then
    death_rows=0
    # Word-split the glob deliberately; each token may itself be a glob.
    # nullglob keeps an unmatched pattern from being scanned as a literal name.
    shopt -s nullglob
    # shellcheck disable=SC2086
    for dump in ${PHPUNIT_RUST_DEATH_DUMPS}; do
        [[ -s "$dump" ]] || continue
        n=$(awk -F'|' -v re="${WORKER_DEATH_RE}" \
            'index($3, re) > 0 { c++ } END { print c+0 }' "$dump")
        if [[ "$n" -gt 0 ]]; then
            echo "check_parity: ${n} worker-death row(s) in ${dump}" >&2
            death_rows=$((death_rows + n))
        fi
    done
    shopt -u nullglob
    if [[ "$death_rows" -gt 0 ]]; then
        echo "check_parity: FAIL — ${death_rows} worker-death row(s) found despite the counts table; tests never ran (a lost single-row method synthesises one Error and hides behind an equal count)." >&2
        fail=1
    fi
fi

if [[ "$checked" -eq 0 ]]; then
    echo "check_parity: FAIL — the table contained no suite rows (the bench produced no data)." >&2
    exit 1
fi
if [[ "$fail" -ne 0 ]]; then
    echo "check_parity: FAIL — at least one suite ran a different number of tests under vanilla vs phpunit-rust (or worker deaths were detected in the dumps)." >&2
    echo "check_parity: refusing to publish — these numbers are not a like-for-like comparison." >&2
    exit 1
fi
echo "check_parity: PASS — every suite ran the same number of tests under both runners."
