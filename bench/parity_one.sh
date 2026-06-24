#!/usr/bin/env bash
#
# bench/parity_one.sh — clone + run ONE OSS suite at workers=1 and assert that
# proust executed the SAME number of tests as vanilla PHPUnit.
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
# Env:   SMOKE (clone parent dir), BINARY (proust path)
# Exit:  0 = parity; 1 = count mismatch / crash / no data; 2 = usage / setup error.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Default registry is the gated, at-parity production suites. The candidate lane
# (bench-candidates.yml) overrides this with OSS_MANIFEST=.../oss_candidates.tsv
# to vet suites that are NOT yet in the daily gate/table.
MANIFEST="${OSS_MANIFEST:-${SCRIPT_DIR}/oss_suites.tsv}"

# Worker-death markers the runner/worker write into the dump's message field
# (Class::method|Status|message) when a child is lost mid-batch. A death on a
# single-row method synthesises EXACTLY one Error per lost method, so the
# executed-count is preserved — the count-equality gate would PASS while those
# tests never actually ran. count_worker_deaths() lets the gate refuse such a
# run. Markers, with their source:
#   "worker process died: signal N" / "...: exit code N"  runner.rs (slot_died)
#   "worker process crashed before reporting this test"   runner.rs (EOF reap)
#   "worker process terminated before this class could run" worker_fork.php
# The common substring "worker process " (note the trailing space) matches all
# three without false-positiving on benign user messages that merely say
# "worker" (e.g. "expected the worker pool size to be 4").
WORKER_DEATH_RE='worker process '

# count_worker_deaths <dump-path> — echo the number of dump rows whose MESSAGE
# field carries a worker-death marker (0 if the dump is missing/empty). Only
# the message field (3rd '|'-delimited column) is inspected so a test NAMED
# after a worker (Class::testWorkerProcess...) cannot trip the scan.
count_worker_deaths() {
    local dump="$1"
    [[ -s "${dump}" ]] || { echo 0; return; }
    awk -F'|' -v re="${WORKER_DEATH_RE}" \
        'index($3, re) > 0 { n++ } END { print n+0 }' "${dump}"
}

# gate_verdict <name> <van> <rust> <dump> — the gate's decision, factored out
# so it is testable in isolation and shares ONE policy between the PASS and
# FAIL paths. Echoes the human verdict and RETURNS the exit code the gate must
# use: 0 = true parity, 1 = mismatch OR death-despite-matching-counts.
#
# The death check runs even when counts match: that is the whole point — a
# synthesised one-error-per-lost-method death keeps the count equal, so without
# this the gate would green-light a run in which tests silently never executed.
gate_verdict() {
    local name="$1" van="$2" rust="$3" dump="$4"
    if [[ "${van}" =~ ^[0-9]+$ && "${rust}" =~ ^[0-9]+$ && "${van}" == "${rust}" ]]; then
        local deaths
        deaths="$(count_worker_deaths "${dump}")"
        if [[ "${deaths}" -gt 0 ]]; then
            echo "[parity] ${name} FAIL — ${deaths} worker-death row(s) present despite matching counts (van=${van} rust=${rust}); a lost single-row method synthesises one Error and hides the loss behind an equal count" >&2
            return 1
        fi
        echo "[parity] ${name} PASS (workers=${WORKERS:-?}: ${rust} == vanilla ${van})"
        return 0
    fi
    echo "[parity] ${name} FAIL — workers=${WORKERS:-?}: rust=${rust:-?} vs vanilla=${van:-?} (not a like-for-like run)" >&2
    return 1
}

# When sourced (PARITY_ONE_SOURCED=1, e.g. by parity_death_scan_test.sh) stop
# here: expose the functions above without running the clone/build/gate body.
[[ "${PARITY_ONE_SOURCED:-0}" == "1" ]] && return 0

SUITE="${1:?Usage: $0 <suite-name>}"
SMOKE="${SMOKE:-/tmp/proust-smoke}"
BINARY="${BINARY:-${SCRIPT_DIR}/../target/release/proust}"
mkdir -p "$SMOKE"

# Look up the suite's clone URL / ref / extra_env / composer_args from the manifest.
row="$(awk -F'\t' -v s="$SUITE" '$1==s{print; exit}' "$MANIFEST")"
[[ -n "$row" ]] || { echo "parity_one: unknown suite '$SUITE'" >&2; exit 2; }
# Split with cut, NOT `IFS=$'\t' read`: tab is an IFS-whitespace char, so read
# COLLAPSES consecutive tabs and would drop an empty middle field (a suite with
# composer_args but no extra_env: name\turl\tref\t\t--flag). cut -f preserves
# empty fields and yields "" for a missing trailing field on shorter rows.
# Field 4 = extra_env (env var, e.g. CALCULATOR=GMP); field 5 = optional extra
# `composer install` flags (e.g. monolog's --ignore-platform-req=ext-mongodb,
# whose mongodb/mongodb dev dep needs an extension absent from the CI image).
name="$(printf '%s' "$row" | cut -f1)"
git_url="$(printf '%s' "$row" | cut -f2)"
ref="$(printf '%s' "$row" | cut -f3)"
extra_env="$(printf '%s' "$row" | cut -f4)"
composer_args="$(printf '%s' "$row" | cut -f5)"

suite_dir="${SMOKE}/${name}"
if [[ ! -d "${suite_dir}" ]]; then
    echo "[parity] cloning ${name} @ ${ref} ..." >&2
    if ! git clone --depth 1 --branch "${ref}" "${git_url}" "${suite_dir}" 2>&1; then
        echo "parity_one: clone failed for ${name}" >&2; exit 2
    fi
    if ! ( cd "${suite_dir}" && COMPOSER_NO_INTERACTION=1 \
           composer install --no-interaction --prefer-dist --no-progress ${composer_args} 2>&1 ); then
        echo "parity_one: composer install failed for ${name}" >&2; exit 2
    fi
fi

# One run at WORKERS (the matrix sets it: 4 = gate / real path, 1 = diagnosis).
# bench_host handles the per-suite vanilla quirks (phpunit-itself's
# ./phpunit --testsuite unit, mockery's bootstrap, etc.).
#
# PROUST_DUMP_TESTS makes the runner write its exact EXPANDED test list
# (one line per data row: Class::method|Status) — the forensic input that
# --list-tests can't provide. Costs one file write; only read on failure.
WORKERS="${WORKERS:-4}"
DUMP="${TMPDIR:-/tmp}/rust-tests-${name}.txt"
out="$(
    export BINARY SMOKE RUNS="${RUNS:-1}" WORKERS
    export PROUST_DUMP_TESTS="${DUMP}"
    [[ -n "${extra_env}" ]] && export "${extra_env?}"
    "${SCRIPT_DIR}/bench_host.sh" "${name}"
)"
echo "${out}"

# bench_host rows: | name | runner | workers | tests | wall ms |  → tests is field 5.
van="$(awk -F'|'  '/vanilla-phpunit/ {gsub(/[^0-9?]/,"",$5); print $5; exit}' <<<"${out}")"
rust="$(awk -F'|' '/proust/    {gsub(/[^0-9?]/,"",$5); print $5; exit}' <<<"${out}")"

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
    cut -d'|' -f1 "${DUMP}" | sort | uniq -c | awk '{print $2 "\t" $1}' | sort > "${rcounts}"

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

    # The WHY: sample error messages for the divergent methods. One distinct
    # message usually names the root cause outright.
    echo "[forensics] sample error messages for divergent methods (top 8):"
    join -t$'\t' -a1 -a2 -e 0 -o 0,1.2,2.2 "${vcounts}" "${rcounts}" \
      | awk -F'\t' '$2 != $3 {print $1}' | head -8 \
      | while IFS= read -r m; do
            grep -F "${m}|" "${DUMP}" | awk -F'|' -v m="${m}" '$3 != "" {print "  " m ": " $3}' | sort -u | head -2
        done

    # Worker deaths. Verified empirically (poison-OOM repro): the PHP fatal's
    # text NEVER reaches the orchestrator's streams — the child's stderr is
    # swallowed — so grepping the run output is useless. Two instruments DO
    # name the killer: (1) the dump already attributes the IN-FLIGHT victim
    # ("worker process died" on a concrete test), and (2) the per-slot batch
    # traces (PROUST_TRACE_BATCHES): a trace whose last line is START
    # with no matching END is the batch that took its worker down.
    if grep -q 'worker process' "${DUMP}"; then
        echo "[forensics] in-flight victims (test running when its worker died):"
        grep -a 'worker process died' "${DUMP}" | cut -d'|' -f1 | sort | uniq -c | head -10

        echo "[forensics] re-running with batch traces to name the killer batches:"
        rerun="${TMPDIR:-/tmp}/rust-rerun-${name}.log"
        tracedir="${TMPDIR:-/tmp}/rust-traces-${name}"
        mkdir -p "${tracedir}"
        (
            [[ -n "${extra_env}" ]] && export "${extra_env?}"
            export PROUST_TRACE_BATCHES="${tracedir}"
            "${BINARY}" --project "${suite_dir}" --workers "${WORKERS}" --worker-memory-limit=-1 \
                > "${rerun}" 2>&1
        ) || true
        echo "[forensics] dangling batch traces (START without END = killer batch):"
        for f in "${tracedir}"/*; do
            [[ -f "$f" ]] || continue
            last="$(tail -1 "$f")"
            [[ "$last" == *START* ]] && echo "  ${last}"
        done
        echo "[forensics] re-run summary + tail (binary-safe):"
        tr -d '\0' < "${rerun}" | grep -aE 'Tests: [0-9]+' | tail -1
        tr -d '\0' < "${rerun}" | tail -20
    fi
}

echo "[parity] ${name} (workers=${WORKERS}): vanilla=${van:-?} rust=${rust:-?}" >&2

# Single source of truth for the verdict. gate_verdict FAILS (returns 1) when
# counts match BUT the dump carries worker-death rows — the load-bearing case
# the bare count check misses: a death on single-row methods synthesises one
# Error per lost method, so the count stays equal while the tests never ran.
if gate_verdict "${name}" "${van:-?}" "${rust:-?}" "${DUMP}"; then
    # True parity. Forensics only on explicit request (the run was clean).
    [[ "${PARITY_FORCE_FORENSICS:-0}" == "1" ]] && forensics
    exit 0
fi

# Any failure — count mismatch OR death-despite-matching-counts — runs the
# forensics so the CI log names the divergent methods / killer batch, then
# exits 1 (the mismatch code; 2 is reserved for usage/setup). The death-scan
# path benefits especially: forensics' "in-flight victims" block reads the very
# rows count_worker_deaths flagged and names the test that took its worker down.
forensics
exit 1
