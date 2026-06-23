#!/usr/bin/env bash
#
# Host benchmark for proust vs vanilla PHPUnit (and optionally ParaTest).
# Runs each project N times and reports the median wall time.
#
# Requires: PHP with pcntl extension, composer-installed project deps.
#
# Usage:
#   bench/bench_host.sh                     # bench all projects, 3 runs each
#   bench/bench_host.sh carbon              # bench only one project
#   bench/bench_host.sh carbon doctrine-orm # bench a subset
#   RUNS=5 bench/bench_host.sh              # override run count
#   WORKERS=8 bench/bench_host.sh           # override worker count
#   SKIP_VANILLA=1 bench/bench_host.sh      # skip vanilla PHPUnit comparison
#   PARATEST=1 bench/bench_host.sh          # also measure ParaTest if installed

set -euo pipefail

BINARY="${BINARY:-/home/gumiranda/PHPUnit_rust/target/release/proust}"
SMOKE="${SMOKE:-/tmp/proust-smoke}"
RUNS="${RUNS:-3}"
WORKERS="${WORKERS:-4}"
SKIP_VANILLA="${SKIP_VANILLA:-0}"
PARATEST="${PARATEST:-0}"
PARATEST_BIN="${PARATEST_BIN:-/tmp/vendor/bin/paratest}"

ALL_PROJECTS=(
    brick-math
    carbon
    doctrine-collections
    doctrine-orm
    faker
    flysystem
    guzzle-psr7
    mockery
    php-parser
    phpunit-itself
    psalm
    ramsey-uuid
    symfony-http-foundation
    symfony-string
    symfony-validator
)

if [ $# -gt 0 ]; then
    PROJECTS=("$@")
else
    PROJECTS=("${ALL_PROJECTS[@]}")
fi

# Per-project extras: some projects need explicit phpunit args.
#
# phpunit-itself: restrict vanilla to the `unit` testsuite. PHPUnit's own suite
# splits into `unit` (regular *Test.php classes) and `end-to-end` (1000+ `.phpt`
# files). proust discovers tests by reflecting *Test.php CLASSES, so it
# cannot run the file-based `.phpt` format at all — comparing against vanilla's
# full run (unit + .phpt) would be apples-to-oranges. `--testsuite unit` makes
# the comparison like-for-like. The `.phpt` gap is documented in COMPATIBILITY.md
# ("Not yet supported") and tracked as a close-the-gaps follow-up.
declare -A EXTRA_VANILLA=(
    [mockery]="--no-configuration --bootstrap tests/Bootstrap.php tests/"
    [phpunit-itself]="--testsuite unit"
)

# Per-project vanilla phpunit executable, relative to the suite dir. Defaults to
# vendor/bin/phpunit. PHPUnit's OWN repo ships its binary as ./phpunit
# (composer.json "bin": ["phpunit"]) and does not depend on phpunit/phpunit, so
# vendor/bin/phpunit never exists there — using it errors out in ~40 ms and
# silently yields a "?" test count.
declare -A VANILLA_BIN=(
    [phpunit-itself]="phpunit"
)

ms_now() { date +%s%3N; }

# Median of a list of integers, by parameter list.
median() {
    local -a sorted=($(printf '%s\n' "$@" | sort -n))
    local n=${#sorted[@]}
    if (( n == 0 )); then
        echo "?"
    elif (( n % 2 == 1 )); then
        echo "${sorted[n/2]}"
    else
        echo "${sorted[n/2-1]}"
    fi
}

# Run a command N times, capture each wall time in ms. Echoes the median.
# Also extracts the reported test count from the last run.
# Args: <label> <count_runs> -- <cmd ...>
bench_runs() {
    local times=()
    local out tests
    local last_out=""
    for ((i=1; i<=RUNS; i++)); do
        local t0=$(ms_now)
        out=$("$@" 2>&1) || true
        local t1=$(ms_now)
        times+=($((t1-t0)))
        last_out="$out"
    done
    # PHPUnit may print "Tests: N" multiple times (per suite, plus diff
    # output in self-tests); the final summary is the LAST occurrence.
    tests=$(echo "$last_out" | grep -oE 'Tests: [0-9]+' | tail -1 | grep -oE '[0-9]+' || true)
    if [ -z "${tests:-}" ]; then
        tests=$(echo "$last_out" | grep -oE 'OK \([0-9]+' | tail -1 | grep -oE '[0-9]+' || true)
    fi
    [ -z "${tests:-}" ] && tests="?"
    echo "$(median "${times[@]}") $tests"
}

printf "| %-22s | %-15s | %7s | %6s | %10s |\n" \
    "Project" "Runner" "Workers" "Tests" "Wall(med)"
printf "|%s|%s|%s|%s|%s|\n" \
    "------------------------" "-----------------" "---------" "--------" "------------"

for name in "${PROJECTS[@]}"; do
    path="$SMOKE/$name"
    [ -d "$path" ] || { echo "(skip $name: not under $SMOKE)" >&2; continue; }

    # vanilla PHPUnit
    if [ "$SKIP_VANILLA" != "1" ]; then
        extra="${EXTRA_VANILLA[$name]:-}"
        phpunit_bin="${VANILLA_BIN[$name]:-vendor/bin/phpunit}"
        read median_ms tests < <(
            cd "$path" && \
            bench_runs php "$phpunit_bin" $extra
        )
        printf "| %-22s | %-15s | %7s | %6s | %7s ms |\n" \
            "$name" "vanilla-phpunit" "1" "$tests" "$median_ms"
    fi

    # proust. `--worker-memory-limit=-1` so the comparison is
    # apples-to-apples: vanilla above runs with `-d memory_limit=-1` (and the
    # bench image lifts the cap globally). At the default 512M, memory-heavy
    # suites (doctrine's SecondLevelCache*, php-parser's 47-row
    # testExtraAttributes, carbon's MacroTest) can fatal a worker mid-run on
    # slower machines — the orchestrator then reports the queued batches as
    # `<class>: worker process terminated`, which is exactly the parity-gate
    # drift the forensics pinned (exit code 255 worker deaths, CI-only).
    read median_ms tests < <(bench_runs "$BINARY" --project "$path" --workers "$WORKERS" --worker-memory-limit=-1)
    printf "| %-22s | %-15s | %7s | %6s | %7s ms |\n" \
        "$name" "proust" "$WORKERS" "$tests" "$median_ms"

    # ParaTest (optional, must be installed at $PARATEST_BIN)
    if [ "$PARATEST" = "1" ] && [ -x "$PARATEST_BIN" ]; then
        read median_ms tests < <(
            cd "$path" && \
            bench_runs "$PARATEST_BIN" --processes="$WORKERS" --no-coverage
        )
        printf "| %-22s | %-15s | %7s | %6s | %7s ms |\n" \
            "$name" "paratest" "$WORKERS" "$tests" "$median_ms"
    fi
done
