#!/usr/bin/env bash
#
# bench/bench_table_counts.sh — extract the per-suite test-count column from a
# BENCHMARKS.md reference table.
#
# The daily bench auto-PR (bench-daily.yml) must distinguish a PARITY-relevant
# refresh (a suite's vanilla/rust test COUNT changed) from cosmetic timing
# jitter (wall-times drift ±5-10% every run while the counts are identical).
# Only the former should open a PR. This helper emits one "suite|vanilla/rust"
# line per data row (sorted), so two table versions can be compared on counts
# alone — ignoring the timing and speedup columns entirely.
#
# Usage (CLI):  bench/bench_table_counts.sh <BENCHMARKS.md>
# Usage (lib):  BENCH_TABLE_COUNTS_SOURCED=1 source bench/bench_table_counts.sh
#               bench_table_counts <BENCHMARKS.md>
#
# Self-test:    bash bench/bench_table_counts_test.sh

set -uo pipefail

# Emit "suite|vanilla/rust" for every data row between the BENCH:TABLE markers,
# sorted. The header row ("Tests (vanilla / rust)") and the "---" separator are
# skipped because their counts cell isn't shaped like "<int> / <int>".
bench_table_counts() {
    local file="${1:?usage: bench_table_counts <BENCHMARKS.md>}"
    awk -F'|' '
        /BENCH:TABLE:START/ { intbl = 1; next }
        /BENCH:TABLE:END/   { intbl = 0 }
        intbl {
            suite = $2; counts = $3
            gsub(/^[ \t]+|[ \t]+$/, "", suite)
            gsub(/^[ \t]+|[ \t]+$/, "", counts)
            if (counts ~ /^[0-9]+ \/ [0-9]+$/) { print suite "|" counts }
        }
    ' "$file" | sort
}

# CLI entry — runs only when executed directly, not when sourced (mirrors
# parity_one.sh's PARITY_ONE_SOURCED guard so the self-test can source us).
if [[ "${BENCH_TABLE_COUNTS_SOURCED:-0}" != "1" ]]; then
    bench_table_counts "${1:?Usage: $0 <BENCHMARKS.md>}"
fi
