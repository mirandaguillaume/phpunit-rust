#!/usr/bin/env bash
#
# Docker benchmark wrapper: runs proust + vanilla PHPUnit inside a
# container that has PHP 8.4 + pcntl. Use this for projects that require a
# newer PHP than the host (phpunit-itself currently requires >= 8.4).
#
# Build the image once:
#   docker build -f bench/Dockerfile.php84 -t proust-bench:php84 .
#
# Then run a single-project bench (composer install runs on first use):
#   bench/bench_docker.sh phpunit-itself
#
# Override defaults via env:
#   RUNS=5 WORKERS=8 bench/bench_docker.sh phpunit-itself

set -euo pipefail

PROJECT="${1:-phpunit-itself}"
SMOKE="${SMOKE:-/tmp/proust-smoke}"
IMAGE="${IMAGE:-proust-bench:php84}"
RUNS="${RUNS:-3}"
WORKERS="${WORKERS:-4}"
BINARY="${BINARY:-/home/gumiranda/PHPUnit_rust/target/release/proust}"
PHP_SCRIPTS="${PHP_SCRIPTS:-/home/gumiranda/PHPUnit_rust/php}"

PROJ_DIR="$SMOKE/$PROJECT"
[ -d "$PROJ_DIR" ] || { echo "$PROJ_DIR not found" >&2; exit 1; }
[ -x "$BINARY" ]   || { echo "$BINARY not built (cargo build --release)" >&2; exit 1; }

# Detect where vanilla phpunit lives (varies by project layout).
# phpunit-itself: top-level ./phpunit
# everyone else:  vendor/bin/phpunit
#
# `-d memory_limit=-1`: the bench image ships PHP's default 128M limit, which
# carbon's full 6169-test suite blows through near the end (Fatal error:
# Allowed memory size exhausted), so vanilla dies before printing a summary
# and the count shows "?". proust never hit this because each forked
# worker gets --worker-memory-limit. Lift the cap for vanilla so the
# comparison is apples-to-apples.
PHP_VANILLA="php -d memory_limit=-1"
if   [ -x "$PROJ_DIR/phpunit" ];           then VANILLA="$PHP_VANILLA phpunit"
elif [ -x "$PROJ_DIR/vendor/bin/phpunit" ]; then VANILLA="$PHP_VANILLA vendor/bin/phpunit"
else
    echo "no phpunit binary in $PROJ_DIR (top-level ./phpunit or vendor/bin/phpunit expected)" >&2
    exit 1
fi

ms_now() { date +%s%3N; }

median() {
    local -a sorted=($(printf '%s\n' "$@" | sort -n))
    local n=${#sorted[@]}
    if   (( n == 0 )); then echo "?"
    elif (( n % 2 == 1 )); then echo "${sorted[n/2]}"
    else echo "${sorted[n/2-1]}"
    fi
}

# Pull the final "Tests: N" from a PHPUnit output FILE. Reading via file
# avoids two pitfalls of `$(docker run ...)` capture:
#   1. PHPUnit emits ANSI colour codes and (rarely) embedded null bytes
#      that bash strips with a warning, slowing capture noticeably.
#   2. Multi-megabyte test reports buffered into a variable take ages to
#      pipe through grep.
# Filter through `tr -d '\0' | sed 's/\x1b\[[0-9;]*m//g'` first so the
# regex sees clean text.
parse_tests_file() {
    local clean tests
    clean=$(tr -d '\0' < "$1" | sed 's/\x1b\[[0-9;]*m//g')
    # PHPUnit prints "Tests: N" only when the run is NOT all-green; a fully
    # passing run prints "OK (N tests, ...)" instead. brick-math (all-green)
    # hits the second case, so fall back to it or the count stays "?".
    tests=$(printf '%s\n' "$clean" | grep -oE 'Tests: [0-9]+' | tail -1 | grep -oE '[0-9]+' || true)
    if [ -z "$tests" ]; then
        tests=$(printf '%s\n' "$clean" | grep -oE 'OK \([0-9]+' | tail -1 | grep -oE '[0-9]+' || true)
    fi
    printf '%s' "$tests"
}

# `--init` runs tini (or docker's built-in init) as PID 1 so that signals
# the daemon forwards to the container's main process actually propagate
# to the PHP fork children. Without it, killing `docker run` from the
# host leaves orphan PHP workers spinning at 100% CPU inside the
# container (verified the hard way after a 5-hour ghost run).
#
# `--tmpfs /tmp` puts the test scratch space in RAM rather than on the
# container's overlay filesystem. Test suites that write fixture data
# per test (phpstan ~200 KB/test × 12 k tests, doctrine's database
# fixtures, rector's before/after pairs) otherwise dump multi-GB of
# disposable scratch through the storage driver — wasted disk wear,
# wasted IO bandwidth, and nothing meaningful written back. The host's
# /tmp is already tmpfs on modern Linux so the local bench wouldn't
# notice; inside Docker the overlay default makes it painful.
DOCKER_FLAGS=(
    --rm
    --init
    --tmpfs "/tmp:rw,exec,nosuid,size=${TMPFS_SIZE:-4g}"
    -v "$PROJ_DIR":/proj
    -v "$BINARY":/opt/proust/bin/proust:ro
    -v "$PHP_SCRIPTS":/opt/php:ro
    -w /proj
)

# Some suites read configuration from the environment in their bootstrap.
# brick-math's phpunit.php hard-`exit(1)`s unless CALCULATOR is set
# (GMP|BCMath|Native) — without it vanilla aborts before any test runs and
# every forked proust worker crashes (265 errored). The container must
# pass these through to BOTH vanilla and proust so the comparison is
# faithful. Per-project defaults below; override via `ENV_VARS="FOO=bar BAZ=1"`.
declare -A PROJECT_ENV=(
    [brick-math]="CALCULATOR=GMP"
)
ENV_VARS="${ENV_VARS:-${PROJECT_ENV[$PROJECT]:-}}"
for kv in $ENV_VARS; do
    DOCKER_FLAGS+=( -e "$kv" )
done

# Track every container we start so we can docker-stop them on interrupt.
# `docker run --rm` doesn't help if the daemon never sees the SIGTERM —
# Ctrl-C on the shell kills the local docker client but the container
# keeps running attached to the daemon.
CID_DIR=$(mktemp -d)
cleanup() {
    local cidfile
    for cidfile in "$CID_DIR"/*; do
        [ -f "$cidfile" ] || continue
        local cid
        cid=$(cat "$cidfile" 2>/dev/null || true)
        [ -n "$cid" ] && docker stop --time 2 "$cid" >/dev/null 2>&1 || true
    done
    rm -rf "$CID_DIR"
}
trap cleanup EXIT INT TERM

run_in_container() {
    local cidfile
    cidfile=$(mktemp -p "$CID_DIR")
    rm -f "$cidfile"  # docker refuses to write to an existing path
    docker run --cidfile "$cidfile" "${DOCKER_FLAGS[@]}" "$IMAGE" "$@"
    local rc=$?
    rm -f "$cidfile"  # successful exit: container is already gone via --rm
    return $rc
}

# Composer install on first use; idempotent if vendor is already populated.
if [ ! -f "$PROJ_DIR/vendor/autoload.php" ]; then
    echo "Installing composer deps in $PROJECT (one-time)..." >&2
    run_in_container composer install --no-interaction --prefer-dist --no-progress
fi

bench_runs() {
    local times=()
    local out_file
    out_file=$(mktemp)
    trap "rm -f $out_file" RETURN
    for ((i=1; i<=RUNS; i++)); do
        local t0=$(ms_now)
        run_in_container "$@" > "$out_file" 2>&1 || true
        local t1=$(ms_now)
        times+=($((t1-t0)))
    done
    local tests
    tests=$(parse_tests_file "$out_file")
    [ -z "$tests" ] && tests="?"
    echo "$(median "${times[@]}") $tests"
}

printf "| %-22s | %-15s | %7s | %6s | %10s |\n" \
    "Project" "Runner" "Workers" "Tests" "Wall(med)"
printf "|%s|%s|%s|%s|%s|\n" \
    "------------------------" "-----------------" "---------" "--------" "------------"

# vanilla
read median_ms tests < <(bench_runs sh -c "$VANILLA")
printf "| %-22s | %-15s | %7s | %6s | %7s ms |\n" \
    "$PROJECT" "vanilla-phpunit" "1" "$tests" "$median_ms"

# proust
read median_ms tests < <(bench_runs /opt/proust/bin/proust --project /proj --workers "$WORKERS")
printf "| %-22s | %-15s | %7s | %6s | %7s ms |\n" \
    "$PROJECT" "proust" "$WORKERS" "$tests" "$median_ms"
