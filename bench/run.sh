#!/usr/bin/env bash
# Multi-PHP-version benchmark for phpunit-rust via Docker.
#
# Runs each (PHP_VERSION × PROJECT × WORKERS) combination inside the
# matching `php:VERSION-cli` Docker container and captures wall-clock,
# max-RSS, and CPU%. Writes a markdown table to stdout.
#
# Assumes:
#   - phpunit-rust release binary is built (./target/release/phpunit-rust)
#   - Projects are cloned + composer-installed under /tmp/phpunit-rust-smoke/
#   - Docker is on $PATH
#
# Usage:
#   bench/run.sh                        # default matrix
#   bench/run.sh --quick                # one PHP version, one worker config
#   bench/run.sh > bench/results.md     # capture output
set -euo pipefail

REPO=/home/gumiranda/PHPUnit_rust
SMOKE=/tmp/phpunit-rust-smoke
BINARY="$REPO/target/release/phpunit-rust"

# Project entries: "name|host_path|min_php_ver|extra_env|phpunit_args"
#
#   name          — label shown in the table
#   host_path     — absolute path on the host (mounted read-only as /proj)
#   min_php_ver   — minimum PHP version required; skip older containers
#   extra_env     — optional env var(s) prepended to both commands (e.g. CALCULATOR=Native)
#   phpunit_args  — extra args appended to vanilla ./vendor/bin/phpunit
#                   (use "tests/" for projects that have no phpunit.xml)
PHP_VERSIONS=(8.1 8.2 8.3 8.4 8.5)
PROJECTS=(
    "fixture|$REPO/fixtures/sample_project|8.1||tests/"
    "guzzle-psr7|$SMOKE/guzzle-psr7|7.2||"
    "php-parser|$SMOKE/php-parser|7.4||"
    "mockery|$SMOKE/mockery|8.1||"
    "faker|$SMOKE/faker|8.1||"
    "psalm|$SMOKE/psalm|8.1||"
    "doctrine-orm|$SMOKE/doctrine-orm|8.1||"
    "ramsey-uuid|$SMOKE/ramsey-uuid|8.2||"
    "carbon|$SMOKE/carbon|8.1||"
    "doctrine-collections|$SMOKE/doctrine-collections|8.4||"
    "phpunit-itself|$SMOKE/phpunit-itself|8.4||"
    "symfony-validator|$SMOKE/symfony-validator|8.4||"
    "brick-math|$SMOKE/brick-math|8.2|CALCULATOR=Native|"
)
WORKERS=(1 4 22)

if [ "${1:-}" = "--quick" ]; then
    PHP_VERSIONS=(8.4)
    WORKERS=(4)
fi

if [ ! -x "$BINARY" ]; then
    echo "binary not built: $BINARY" >&2
    echo "run: cd $REPO && cargo build --release" >&2
    exit 1
fi

# Write a reusable runner script into a temp file.
# The script is mounted read-only into each container at /bench-runner.sh.
# It installs GNU time (not bundled in official php:VERSION-cli images),
# then runs its first argument under /usr/bin/time -v, writing:
#   - stdout  →  /bench-stdout.txt   (the command's own output)
#   - stderr  →  /bench-time.txt     (GNU time stats)
RUNNER_SCRIPT=$(mktemp /tmp/bench-runner.XXXXXX.sh)
cat > "$RUNNER_SCRIPT" << 'RUNNER_EOF'
#!/bin/bash
apt-get update -qq >/dev/null 2>&1
apt-get install -qq -y time >/dev/null 2>&1
exec /usr/bin/time -v sh -c "$1" >/bench-stdout.txt 2>/bench-time.txt
RUNNER_EOF
chmod 755 "$RUNNER_SCRIPT"
trap 'rm -f "$RUNNER_SCRIPT"' EXIT

echo "| PHP | Project | Runner | Workers | Tests | Wall | MaxRSS(MB) | CPU% |"
echo "|---|---|---|---|---|---|---|---|"

# Helper: run one command in a PHP container.
# Parses results into globals: G_WALL, G_RSS_MB, G_CPU, G_TESTS.
#
# Arguments:
#   $1  php_ver   — e.g. "8.4"
#   $2  path      — host path for the project (mounted as /proj read-only)
#   $3  cmd       — shell command to run inside the container (at cwd=/proj)
run_in_container() {
    local php_ver=$1
    local path=$2
    local cmd=$3

    local time_file stdout_file
    time_file=$(mktemp /tmp/bench-time.XXXXXX)
    stdout_file=$(mktemp /tmp/bench-stdout.XXXXXX)
    chmod 666 "$time_file" "$stdout_file"

    docker run --rm \
        -v "$REPO:/work:ro" \
        -v "$path:/proj:ro" \
        -v "$time_file:/bench-time.txt" \
        -v "$stdout_file:/bench-stdout.txt" \
        -v "$RUNNER_SCRIPT:/bench-runner.sh:ro" \
        -w /proj \
        "php:$php_ver-cli" \
        /bin/bash /bench-runner.sh "$cmd" \
        >/dev/null 2>&1 || true

    # Parse /usr/bin/time -v output (GNU time writes to stderr = /bench-time.txt):
    #   Elapsed (wall clock) time (h:mm:ss or m:ss): 0:00.07
    #   Maximum resident set size (kbytes): 25144
    #   Percent of CPU this job got: 38%
    G_WALL=$(grep -i "Elapsed.*wall clock" "$time_file" 2>/dev/null \
        | sed -E 's/.*: //' | tr -d '[:space:]' || true)
    [ -z "$G_WALL" ] && G_WALL="?"

    local rss
    rss=$(grep -i "Maximum resident set size" "$time_file" 2>/dev/null \
        | awk '{print $NF}' || true)
    if [[ -n "${rss:-}" && "$rss" =~ ^[0-9]+$ && "$rss" -gt 0 ]]; then
        G_RSS_MB=$(( rss / 1024 ))
    else
        G_RSS_MB="?"
    fi

    local cpu_raw
    cpu_raw=$(grep -i "Percent of CPU" "$time_file" 2>/dev/null \
        | awk '{print $NF}' | tr -d '%' || true)
    [ -z "${cpu_raw:-}" ] && cpu_raw="?"
    G_CPU="$cpu_raw"

    # Test count: phpunit-rust   → "Tests: N total, P passed, ..."
    #             vanilla PHPUnit → "Tests: N, Assertions: ..."  or
    #                               "OK (N tests, ...)"
    G_TESTS=$(grep -oE "Tests: [0-9]+" "$stdout_file" 2>/dev/null \
        | head -1 | awk '{print $2}' || true)
    if [ -z "${G_TESTS:-}" ]; then
        G_TESTS=$(grep -oE "OK \([0-9]+ tests?" "$stdout_file" 2>/dev/null \
            | head -1 | grep -oE "[0-9]+" | head -1 || true)
    fi
    [ -z "${G_TESTS:-}" ] && G_TESTS="?"

    rm -f "$time_file" "$stdout_file"
}

for php_ver in "${PHP_VERSIONS[@]}"; do
    for project_entry in "${PROJECTS[@]}"; do
        IFS="|" read -r name path min_php extra_env phpunit_args <<< "$project_entry"

        # Skip if PHP version is too old for the project.
        if [ "$(printf '%s\n' "$min_php" "$php_ver" | sort -V | head -1)" != "$min_php" ]; then
            continue
        fi

        # Skip silently if the project path doesn't exist.
        if [ ! -d "$path" ]; then
            continue
        fi

        # Prefix env vars if provided (e.g. "CALCULATOR=Native ").
        env_prefix=""
        if [ -n "${extra_env:-}" ]; then
            env_prefix="$extra_env "
        fi

        # --- Vanilla PHPUnit baseline (one run per PHP×project) ---
        # phpunit_args is empty for projects that have a phpunit.xml,
        # "tests/" for the bundled fixture which has no config file.
        run_in_container "$php_ver" "$path" \
            "${env_prefix}./vendor/bin/phpunit ${phpunit_args:-}"
        echo "| $php_ver | $name | vanilla-phpunit | 1 | $G_TESTS | $G_WALL | $G_RSS_MB | $G_CPU |"

        # --- phpunit-rust at each worker count ---
        for workers in "${WORKERS[@]}"; do
            run_in_container "$php_ver" "$path" \
                "${env_prefix}/work/target/release/phpunit-rust --project /proj --workers $workers"
            echo "| $php_ver | $name | phpunit-rust | $workers | $G_TESTS | $G_WALL | $G_RSS_MB | $G_CPU |"
        done
    done
done
