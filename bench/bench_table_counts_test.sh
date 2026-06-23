#!/usr/bin/env bash
#
# bench/bench_table_counts_test.sh — self-test for bench_table_counts.
#
# bench-daily.yml's auto-PR must distinguish a PARITY-relevant refresh (a
# suite's vanilla/rust test COUNT changed) from cosmetic timing jitter
# (wall-times drift ±5-10% every run while counts are identical). This test
# drives the count-extraction in isolation and asserts it is stable under
# timing-only jitter and changes only when a count changes.
#
# Run: bash bench/bench_table_counts_test.sh   (exit 0 = all asserts pass)

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Source bench_table_counts.sh for its function only — the sourcing guard
# (BENCH_TABLE_COUNTS_SOURCED) prevents the CLI body from executing.
export BENCH_TABLE_COUNTS_SOURCED=1
# shellcheck source=/dev/null
source "${SCRIPT_DIR}/bench_table_counts.sh"

fails=0
assert() {
    # assert <desc> <expected> <actual>
    local desc="$1" expected="$2" actual="$3"
    if [[ "${expected}" == "${actual}" ]]; then
        echo "  ok: ${desc}"
    else
        echo "  FAIL: ${desc} — expected '${expected}', got '${actual}'" >&2
        fails=$((fails + 1))
    fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

# --- Baseline table (the committed shape). -----------------------------------
cat > "${tmp}/base.md" <<'EOF'
# Benchmarks
<!-- BENCH:TABLE:START (auto-generated) -->
| Project | Tests (vanilla / rust) | vanilla | proust | speedup |
|---|---|---|---|---|
| brick-math | 20392 / 20392 | 3022 ms | 1042 ms | 2.9× |
| carbon | 6145 / 6145 | 34646 ms | 20021 ms | 1.7× |
<!-- BENCH:TABLE:END -->
trailing prose that must be ignored
EOF

# --- Same counts, DIFFERENT timings (the jitter case). -----------------------
cat > "${tmp}/jitter.md" <<'EOF'
# Benchmarks
<!-- BENCH:TABLE:START (auto-generated) -->
| Project | Tests (vanilla / rust) | vanilla | proust | speedup |
|---|---|---|---|---|
| brick-math | 20392 / 20392 | 2596 ms | 562 ms | 4.6× |
| carbon | 6145 / 6145 | 28871 ms | 17952 ms | 1.6× |
<!-- BENCH:TABLE:END -->
trailing prose that must be ignored
EOF

# --- A real COUNT change on one suite (parity-relevant). ---------------------
cat > "${tmp}/countchange.md" <<'EOF'
# Benchmarks
<!-- BENCH:TABLE:START (auto-generated) -->
| Project | Tests (vanilla / rust) | vanilla | proust | speedup |
|---|---|---|---|---|
| brick-math | 20392 / 20392 | 3022 ms | 1042 ms | 2.9× |
| carbon | 6145 / 6148 | 34646 ms | 20021 ms | 1.7× |
<!-- BENCH:TABLE:END -->
trailing prose that must be ignored
EOF

base="$(bench_table_counts "${tmp}/base.md")"
jitter="$(bench_table_counts "${tmp}/jitter.md")"
countchange="$(bench_table_counts "${tmp}/countchange.md")"

assert "two data rows are extracted" "2" "$(printf '%s\n' "${base}" | grep -c '|')"
assert "timing-only jitter yields identical counts" "${base}" "${jitter}"

# A vanilla/rust count change MUST alter the extraction.
if [[ "${base}" != "${countchange}" ]]; then
    echo "  ok: a vanilla/rust count change is detected"
else
    echo "  FAIL: count change not detected" >&2
    fails=$((fails + 1))
fi

# Header ("Project") and separator ("---") rows must never leak into output.
assert "no header/separator rows leak" "0" "$(printf '%s\n' "${base}" | grep -cE 'Project|---')"

if [[ "${fails}" -eq 0 ]]; then
    echo "ALL PASS"
    exit 0
else
    echo "${fails} assertion(s) failed" >&2
    exit 1
fi
