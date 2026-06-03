#!/usr/bin/env bash
# bench/update_benchmarks_table.sh — splice a fresh reference table into BENCHMARKS.md.
#
# Usage:
#   bench/update_benchmarks_table.sh <table-file>
#
# <table-file>  path to a file whose contents replace everything between
#               <!-- BENCH:TABLE:START … --> and <!-- BENCH:TABLE:END -->
#               in BENCHMARKS.md.  Markers themselves are preserved.
#
# Fails loudly if either marker is absent.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCHMARKS="${SCRIPT_DIR}/../BENCHMARKS.md"
TABLE_FILE="${1:?Usage: $0 <table-file>}"

if [[ ! -f "$TABLE_FILE" ]]; then
    echo "ERROR: table file not found: ${TABLE_FILE}" >&2
    exit 1
fi

if [[ ! -f "$BENCHMARKS" ]]; then
    echo "ERROR: BENCHMARKS.md not found at: ${BENCHMARKS}" >&2
    exit 1
fi

# Verify both markers exist
if ! grep -qF 'BENCH:TABLE:START' "$BENCHMARKS"; then
    echo "ERROR: marker <!-- BENCH:TABLE:START --> not found in ${BENCHMARKS}" >&2
    exit 1
fi
if ! grep -qF 'BENCH:TABLE:END' "$BENCHMARKS"; then
    echo "ERROR: marker <!-- BENCH:TABLE:END --> not found in ${BENCHMARKS}" >&2
    exit 1
fi

TABLE_CONTENT="$(cat "${TABLE_FILE}")"
# Ensure content does not end with newline issues for awk inline variable
# We'll use a temp file approach for safety
TMPFILE="$(mktemp /tmp/benchmarks-splice-XXXXXX.md)"
trap 'rm -f "$TMPFILE"' EXIT

awk \
    -v table_file="${TABLE_FILE}" \
    '
    /BENCH:TABLE:START/ {
        print  # print the START marker line itself
        # Inject the table content
        while ((getline line < table_file) > 0) {
            print line
        }
        close(table_file)
        skip = 1
        next
    }
    /BENCH:TABLE:END/ {
        skip = 0
        print  # print the END marker line itself
        next
    }
    skip { next }
    { print }
    ' \
    "$BENCHMARKS" > "$TMPFILE"

# Sanity: verify markers are still present in the output
if ! grep -qF 'BENCH:TABLE:START' "$TMPFILE"; then
    echo "ERROR: splice produced output missing START marker — aborting, original file unchanged" >&2
    exit 1
fi
if ! grep -qF 'BENCH:TABLE:END' "$TMPFILE"; then
    echo "ERROR: splice produced output missing END marker — aborting, original file unchanged" >&2
    exit 1
fi

mv "$TMPFILE" "$BENCHMARKS"
echo "BENCHMARKS.md updated successfully."
