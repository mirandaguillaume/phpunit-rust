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

set -euo pipefail

TABLE="${1:?Usage: $0 <bench-table.md>}"
[[ -f "$TABLE" ]] || { echo "check_parity: table not found: $TABLE" >&2; exit 2; }

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
if [[ "$checked" -eq 0 ]]; then
    echo "check_parity: FAIL — the table contained no suite rows (the bench produced no data)." >&2
    exit 1
fi
if [[ "$fail" -ne 0 ]]; then
    echo "check_parity: FAIL — at least one suite ran a different number of tests under vanilla vs phpunit-rust." >&2
    echo "check_parity: refusing to publish — these numbers are not a like-for-like comparison." >&2
    exit 1
fi
echo "check_parity: PASS — every suite ran the same number of tests under both runners."
