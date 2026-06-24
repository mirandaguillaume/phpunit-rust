#!/usr/bin/env bash
#
# bench/parity_death_scan_test.sh — self-test for the worker-death gate guard.
#
# The parity gate's verdict is count equality, but a worker death on
# single-row methods synthesises EXACTLY one Error per lost method: the count
# is preserved and the gate would PASS while those tests never ran. This test
# drives the death-scan logic in isolation against synthetic dumps and asserts
# that a matching-counts scenario which contains death rows FAILS the gate.
#
# Run: bash bench/parity_death_scan_test.sh   (exit 0 = all asserts pass)

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Source parity_one.sh for its functions only — the sourcing guard
# (PARITY_ONE_SOURCED) prevents the clone/run/gate body from executing.
export PARITY_ONE_SOURCED=1
# shellcheck source=/dev/null
source "${SCRIPT_DIR}/parity_one.sh"

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

# --- Fixture 1: a clean dump (no deaths) → death count must be 0. -------------
clean="${tmp}/clean.txt"
cat > "${clean}" <<'EOF'
App\FooTest::testOne|Passed|
App\FooTest::testTwo|Failed|some assertion failed
App\BarTest::testThree|Passed|
EOF
n="$(count_worker_deaths "${clean}")"
assert "clean dump has zero death rows" "0" "${n}"

# --- Fixture 2: 'worker process died' (signal/exit) synthesised rows. ---------
died="${tmp}/died.txt"
cat > "${died}" <<'EOF'
App\FooTest::testOne|Passed|
App\BarTest::testHeavy|Error|worker process died: signal 9
App\BarTest::testHeavy2|Error|worker process died: exit code 255
EOF
n="$(count_worker_deaths "${died}")"
assert "'worker process died' rows are counted" "2" "${n}"

# --- Fixture 3: the long-lived-mode crash marker. ----------------------------
crashed="${tmp}/crashed.txt"
cat > "${crashed}" <<'EOF'
App\FooTest::testOne|Error|worker process crashed before reporting this test
EOF
n="$(count_worker_deaths "${crashed}")"
assert "'worker process crashed' row is counted" "1" "${n}"

# --- Fixture 4: the PHP-side class-level termination marker. ------------------
terminated="${tmp}/terminated.txt"
cat > "${terminated}" <<'EOF'
App\BazTest::<class>|Error|worker process terminated before this class could run (prewarm)
EOF
n="$(count_worker_deaths "${terminated}")"
assert "'worker process terminated' row is counted" "1" "${n}"

# --- Fixture 5: a benign user message must NOT false-positive. ----------------
benign="${tmp}/benign.txt"
cat > "${benign}" <<'EOF'
App\FooTest::testProcess|Failed|expected the worker pool size to be 4
EOF
n="$(count_worker_deaths "${benign}")"
assert "benign 'worker' message is not a death row" "0" "${n}"

# --- Fixture 6: the integration assertion — gate FAILS despite matching counts.
# gate_verdict() returns the exit code the gate would use: 0 = parity,
# 1 = mismatch OR death-despite-matching-counts. Here van==rust==3 but the
# dump carries death rows, so the verdict MUST be 1.
matching_with_deaths="${tmp}/match-deaths.txt"
cat > "${matching_with_deaths}" <<'EOF'
App\FooTest::testOne|Passed|
App\BarTest::testHeavy|Error|worker process died: signal 9
App\BazTest::testThree|Passed|
EOF
rc=0
gate_verdict "match-suite" 3 3 "${matching_with_deaths}" >/dev/null 2>&1 || rc=$?
assert "matching counts + death rows => exit 1 (gate fails)" "1" "${rc}"

# --- Fixture 7: matching counts, NO deaths => exit 0 (true parity). -----------
rc=0
gate_verdict "match-suite" 3 3 "${clean}" >/dev/null 2>&1 || rc=$?
assert "matching counts + clean dump => exit 0 (gate passes)" "0" "${rc}"

# --- Fixture 8: real count mismatch still => exit 1. --------------------------
rc=0
gate_verdict "match-suite" 4 3 "${clean}" >/dev/null 2>&1 || rc=$?
assert "count mismatch => exit 1 (gate fails)" "1" "${rc}"

# --- check_parity.sh opportunistic dump scan ---------------------------------
# A counts table where every suite matches MUST still FAIL when the caller
# supplies death dumps via PROUST_DEATH_DUMPS, and PASS without them.
table="${tmp}/bench-table.md"
cat > "${table}" <<'EOF'
| Project | vanilla / rust | speedup |
| ------- | -------------- | ------- |
| brick-math | 1024 / 1024 | 8.1x |
| carbon | 4096 / 4096 | 6.4x |
EOF

# Baseline: matching counts, no dumps env => PASS (exit 0).
rc=0
( "${SCRIPT_DIR}/check_parity.sh" "${table}" ) >/dev/null 2>&1 || rc=$?
assert "check_parity matching table, no dumps => exit 0" "0" "${rc}"

# Clean dumps supplied => still PASS (no death markers present).
rc=0
( PROUST_DEATH_DUMPS="${clean}" "${SCRIPT_DIR}/check_parity.sh" "${table}" ) >/dev/null 2>&1 || rc=$?
assert "check_parity matching table + clean dump => exit 0" "0" "${rc}"

# Death dump supplied => FAIL (exit 1) despite the table's matching counts.
rc=0
( PROUST_DEATH_DUMPS="${died}" "${SCRIPT_DIR}/check_parity.sh" "${table}" ) >/dev/null 2>&1 || rc=$?
assert "check_parity matching table + death dump => exit 1" "1" "${rc}"

# Glob form across a dir (the parity_one.sh /tmp/rust-tests-*.txt shape).
globdir="${tmp}/dumps"
mkdir -p "${globdir}"
cp "${clean}" "${globdir}/rust-tests-a.txt"
cp "${died}"  "${globdir}/rust-tests-b.txt"
rc=0
( PROUST_DEATH_DUMPS="${globdir}/rust-tests-*.txt" "${SCRIPT_DIR}/check_parity.sh" "${table}" ) >/dev/null 2>&1 || rc=$?
assert "check_parity matching table + glob with one death dump => exit 1" "1" "${rc}"

echo
if [[ "${fails}" -ne 0 ]]; then
    echo "parity_death_scan_test: ${fails} assertion(s) FAILED" >&2
    exit 1
fi
echo "parity_death_scan_test: all assertions passed"
