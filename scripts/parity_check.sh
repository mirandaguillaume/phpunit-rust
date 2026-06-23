#!/usr/bin/env bash
#
# parity_check.sh — guard against test-count regressions between vanilla
# PHPUnit and the proust runner.
#
# Given a PHP project directory, this script:
#   1. Runs the project's vendored PHPUnit       ($PROJECT/vendor/bin/phpunit)
#   2. Runs the built proust binary         (--project $PROJECT)
#   3. Extracts the "Tests: N, Assertions: M" summary counts from each
#   4. Exits non-zero with a clear diff if the Tests (or Assertions) counts
#      disagree.
#
# Both runners deliberately keep a failing fixture in sample_project, so a
# non-zero exit code from either runner is EXPECTED and is NOT, on its own, a
# parity failure. Parity is defined purely by the summary counts matching.
#
# Note on Assertions: vanilla PHPUnit prints "Assertions: M" in its summary;
# the current proust summary does not. When one side does not report an
# assertion count we skip the assertion comparison (and say so) rather than
# fail — the Tests count is the authoritative regression signal. If/when
# proust starts emitting assertion counts, this check tightens
# automatically.
#
# Usage:
#   scripts/parity_check.sh <project-dir> [path-to-proust-binary]
#
# The proust binary is located, in order of preference:
#   1. the optional 2nd argument
#   2. $PHPUNIT_RUST_BIN
#   3. ./target/debug/proust then ./target/release/proust
#      (relative to the repo root containing this script)
#   4. `proust` on $PATH

set -euo pipefail

die() {
    echo "parity_check: $*" >&2
    exit 1
}

# ----------------------------------------------------------------------------
# Argument / environment resolution
# ----------------------------------------------------------------------------
[ "$#" -ge 1 ] || die "usage: $0 <project-dir> [proust-binary]"

PROJECT="$1"
[ -d "$PROJECT" ] || die "project directory not found: $PROJECT"
# Absolute path so we can invoke the runner from anywhere.
PROJECT="$(cd "$PROJECT" && pwd)"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

resolve_rust_bin() {
    if [ "$#" -ge 2 ] && [ -n "${2:-}" ]; then
        echo "$2"; return 0
    fi
    if [ -n "${PHPUNIT_RUST_BIN:-}" ]; then
        echo "$PHPUNIT_RUST_BIN"; return 0
    fi
    if [ -x "$REPO_ROOT/target/debug/proust" ]; then
        echo "$REPO_ROOT/target/debug/proust"; return 0
    fi
    if [ -x "$REPO_ROOT/target/release/proust" ]; then
        echo "$REPO_ROOT/target/release/proust"; return 0
    fi
    command -v proust 2>/dev/null && return 0
    return 1
}

RUST_BIN="$(resolve_rust_bin "$@")" \
    || die "could not locate proust binary (build it with: cargo build -p proust)"
[ -x "$RUST_BIN" ] || command -v "$RUST_BIN" >/dev/null 2>&1 \
    || die "proust binary is not executable: $RUST_BIN"

PHPUNIT_BIN="$PROJECT/vendor/bin/phpunit"

# ----------------------------------------------------------------------------
# Count extraction
# ----------------------------------------------------------------------------
# Pull the first integer following a "<label>:" token out of a summary blob.
# Echoes the integer, or nothing if the label is absent. Works for both the
# vanilla "Tests: 15, Assertions: 14, ..." line and the proust
# "Tests: 15 total, ..." line.
extract_count() {
    local label="$1" blob="$2"
    printf '%s\n' "$blob" \
        | grep -oE "${label}: [0-9]+" \
        | head -n1 \
        | grep -oE '[0-9]+' || true
}

run_and_capture() {
    # Runs "$@" and echoes combined stdout+stderr. Tolerates a non-zero exit
    # (the fixture intentionally fails one test) — set -e must not abort here.
    local out
    out="$("$@" 2>&1)" || true
    printf '%s' "$out"
}

# ----------------------------------------------------------------------------
# Vanilla PHPUnit
# ----------------------------------------------------------------------------
VANILLA_TESTS=""
VANILLA_ASSERTIONS=""
if [ -x "$PHPUNIT_BIN" ] || [ -f "$PHPUNIT_BIN" ]; then
    if command -v php >/dev/null 2>&1; then
        echo "==> vanilla phpunit: php $PHPUNIT_BIN (cwd=$PROJECT)"
        VANILLA_OUT="$(cd "$PROJECT" && run_and_capture php "$PHPUNIT_BIN")"
    else
        echo "==> vanilla phpunit: $PHPUNIT_BIN (cwd=$PROJECT)"
        VANILLA_OUT="$(cd "$PROJECT" && run_and_capture "$PHPUNIT_BIN")"
    fi
    VANILLA_TESTS="$(extract_count Tests "$VANILLA_OUT")"
    VANILLA_ASSERTIONS="$(extract_count Assertions "$VANILLA_OUT")"
    if [ -z "$VANILLA_TESTS" ]; then
        echo "----- vanilla phpunit output -----" >&2
        printf '%s\n' "$VANILLA_OUT" | tail -n 30 >&2
        echo "----------------------------------" >&2
        die "could not parse 'Tests: N' from vanilla phpunit output"
    fi
    echo "    vanilla   -> Tests=$VANILLA_TESTS Assertions=${VANILLA_ASSERTIONS:-<none>}"
else
    die "vanilla phpunit not found at $PHPUNIT_BIN (does the project have a vendor/ dir?)"
fi

# ----------------------------------------------------------------------------
# proust
# ----------------------------------------------------------------------------
echo "==> proust: $RUST_BIN --project $PROJECT"
# RUST_MIN_STACK keeps debug builds from overflowing the 8 MB default stack;
# harmless for release builds. Mirrors crates/runner/tests/integration.rs.
RUST_OUT="$(RUST_MIN_STACK="${RUST_MIN_STACK:-67108864}" \
    run_and_capture "$RUST_BIN" --project "$PROJECT")"
RUST_TESTS="$(extract_count Tests "$RUST_OUT")"
RUST_ASSERTIONS="$(extract_count Assertions "$RUST_OUT")"
if [ -z "$RUST_TESTS" ]; then
    echo "----- proust output -----" >&2
    printf '%s\n' "$RUST_OUT" | tail -n 30 >&2
    echo "-------------------------------" >&2
    die "could not parse 'Tests: N' from proust output"
fi
echo "    proust -> Tests=$RUST_TESTS Assertions=${RUST_ASSERTIONS:-<none>}"

# ----------------------------------------------------------------------------
# Compare
# ----------------------------------------------------------------------------
STATUS=0

if [ "$VANILLA_TESTS" != "$RUST_TESTS" ]; then
    echo "PARITY FAILURE: Tests count differs" >&2
    echo "    vanilla phpunit : Tests=$VANILLA_TESTS" >&2
    echo "    proust    : Tests=$RUST_TESTS" >&2
    STATUS=1
fi

if [ -n "$VANILLA_ASSERTIONS" ] && [ -n "$RUST_ASSERTIONS" ]; then
    if [ "$VANILLA_ASSERTIONS" != "$RUST_ASSERTIONS" ]; then
        echo "PARITY FAILURE: Assertions count differs" >&2
        echo "    vanilla phpunit : Assertions=$VANILLA_ASSERTIONS" >&2
        echo "    proust    : Assertions=$RUST_ASSERTIONS" >&2
        STATUS=1
    fi
else
    echo "    (skipping Assertions comparison: proust does not report an assertion count)"
fi

if [ "$STATUS" -eq 0 ]; then
    echo "PARITY OK: Tests=$VANILLA_TESTS match (project: $PROJECT)"
fi

exit "$STATUS"
