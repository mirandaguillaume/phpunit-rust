#!/usr/bin/env bash
# bench/run_oss_bench.sh — clone + composer-install the pinned OSS suites,
# run bench_host.sh against them, and write the reference table to $1.
#
# Usage:
#   bench/run_oss_bench.sh /tmp/bench-table.md
#
# Env (all optional):
#   BINARY   — path to phpunit-rust binary (default: target/release/phpunit-rust)
#   SMOKE    — parent directory for cloned suites (default: /tmp/phpunit-rust-smoke)
#   RUNS     — bench runs per suite (default: 3)
#   WORKERS  — worker count passed to phpunit-rust (default: 4)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="${SCRIPT_DIR}/oss_suites.tsv"
OUTPUT_FILE="${1:?Usage: $0 <output-file>}"

BINARY="${BINARY:-${SCRIPT_DIR}/../target/release/phpunit-rust}"
SMOKE="${SMOKE:-/tmp/phpunit-rust-smoke}"
RUNS="${RUNS:-3}"
WORKERS="${WORKERS:-4}"

mkdir -p "$SMOKE"

# ---------------------------------------------------------------------------
# Phase 1: clone + composer install (idempotent, per-suite non-fatal)
# ---------------------------------------------------------------------------
prepared_suites=()

while IFS= read -r line; do
    # Split with cut, NOT `IFS=$'\t' read`: tab is IFS-whitespace, so read
    # collapses consecutive tabs and would drop an empty middle field (a suite
    # with composer_args but no extra_env). cut -f preserves empty fields.
    name="$(printf '%s' "$line" | cut -f1)"
    [[ "$name" == "name" || -z "$name" ]] && continue   # skip header / blank
    git_url="$(printf '%s' "$line" | cut -f2)"
    ref="$(printf '%s' "$line" | cut -f3)"
    composer_args="$(printf '%s' "$line" | cut -f5)"     # optional `composer install` flags

    suite_dir="${SMOKE}/${name}"

    if [[ -d "${suite_dir}" ]]; then
        echo "[oss-bench] SKIP clone ${name}: ${suite_dir} already exists" >&2
    else
        echo "[oss-bench] Cloning ${name} @ ${ref} ..." >&2
        if ! git clone --depth 1 --branch "${ref}" "${git_url}" "${suite_dir}" 2>&1; then
            echo "[oss-bench] ERROR: clone failed for ${name} — skipping suite" >&2
            continue
        fi

        echo "[oss-bench] composer install for ${name} ..." >&2
        if ! (
            export COMPOSER_NO_INTERACTION=1
            cd "${suite_dir}"
            composer install --no-interaction --prefer-dist --no-progress ${composer_args} 2>&1
        ); then
            echo "[oss-bench] ERROR: composer install failed for ${name} — skipping suite" >&2
            rm -rf "${suite_dir}"
            continue
        fi
    fi

    prepared_suites+=("$name")
done < "$MANIFEST"

if [[ ${#prepared_suites[@]} -eq 0 ]]; then
    echo "[oss-bench] ERROR: no suites were successfully prepared — aborting" >&2
    exit 1
fi

echo "[oss-bench] Prepared suites: ${prepared_suites[*]}" >&2

# ---------------------------------------------------------------------------
# Phase 2: run bench_host.sh per-suite (capture raw table rows)
# ---------------------------------------------------------------------------
# We run bench_host.sh once per suite so we can inject per-suite extra_env
# and so one suite failure doesn't abort the rest.

raw_table_file="$(mktemp /tmp/oss-bench-raw-XXXXXX.txt)"
trap 'rm -f "${raw_table_file}"' EXIT

header_written=0

for suite_name in "${prepared_suites[@]}"; do
    # Look up extra_env for this suite
    suite_extra_env=""
    while IFS= read -r line; do
        name="$(printf '%s' "$line" | cut -f1)"
        [[ "$name" == "name" || -z "$name" ]] && continue
        if [[ "$name" == "$suite_name" ]]; then
            suite_extra_env="$(printf '%s' "$line" | cut -f4)"
            break
        fi
    done < "$MANIFEST"

    echo "[oss-bench] Benchmarking ${suite_name} (extra_env='${suite_extra_env}') ..." >&2

    # Run bench_host.sh for just this suite; capture stdout (the table).
    # Stderr from bench_host.sh is forwarded so timing/error context is visible.
    suite_output=""
    if ! suite_output=$(
        export BINARY SMOKE RUNS WORKERS
        # suite_extra_env is a single KEY=VALUE assignment (e.g. CALCULATOR=GMP)
        # or empty; bash accepts `export KEY=VALUE` as a single argument.
        if [[ -n "$suite_extra_env" ]]; then
            export "${suite_extra_env?}"
        fi
        "${SCRIPT_DIR}/bench_host.sh" "$suite_name"
    ); then
        echo "[oss-bench] WARNING: bench_host.sh failed for ${suite_name} — skipping" >&2
        continue
    fi

    # Extract data rows (lines starting with | <suite_name>)
    data_rows=$(echo "$suite_output" | grep "^| ${suite_name}" || true)
    if [[ -z "$data_rows" ]]; then
        echo "[oss-bench] WARNING: no output rows found for ${suite_name}" >&2
        continue
    fi

    # Write header once (from the first successful suite run)
    if [[ $header_written -eq 0 ]]; then
        echo "$suite_output" | grep -E '^\| (Project|----)' >> "${raw_table_file}" || true
        header_written=1
    fi

    echo "$data_rows" >> "${raw_table_file}"

    echo "[oss-bench] ${suite_name}: done" >&2
done

# ---------------------------------------------------------------------------
# Phase 3: parse raw rows → emit formatted reference table
# ---------------------------------------------------------------------------
# raw rows look like:
#   | carbon          | vanilla-phpunit |       1 |  6169 |    23281 ms |
#   | carbon          | phpunit-rust    |       4 |  6169 |    10753 ms |
#
# Target format:
#   | <name> | <vanTests> / <rustTests> | <van> ms | <rust> ms | <speedup>× |

# PHP, not python: the CI bench container is php:8.4-cli-based and ships no
# python3 — this step only ever runs once every gate is green, which is why
# the gap went unnoticed until the gates' first all-green run.
php -- "${raw_table_file}" "${OUTPUT_FILE}" << 'PHPEOF'
<?php
[, $rawFile, $outFile] = $argv;

$rows = []; // name => [van_tests, rust_tests, van_ms, rust_ms]

foreach (file($rawFile, FILE_IGNORE_NEW_LINES) ?: [] as $line) {
    $line = trim($line);
    if (!str_starts_with($line, '|')) continue;
    $parts = array_map('trim', explode('|', trim($line, '|')));
    if (count($parts) < 5) continue;
    [$name, $runner, , $tests, $wall] = $parts;
    // wall looks like "23281 ms" or "  821 ms"
    if (!preg_match('/(\d+)/', $wall, $m)) continue;
    $ms = (int) $m[1];
    $rows[$name] ??= [];
    if (str_contains($runner, 'vanilla')) {
        $rows[$name]['van_tests'] = $tests;
        $rows[$name]['van_ms']    = $ms;
    } elseif (str_contains($runner, 'phpunit-rust')) {
        $rows[$name]['rust_tests'] = $tests;
        $rows[$name]['rust_ms']    = $ms;
    }
}

$lines = [
    '| Project | Tests (vanilla / rust) | vanilla | phpunit-rust | speedup |',
    '|---|---|---|---|---|',
];

foreach ($rows as $name => $d) {
    $vanT   = $d['van_tests']  ?? '?';
    $rustT  = $d['rust_tests'] ?? '?';
    $vanMs  = $d['van_ms']     ?? 0;
    $rustMs = $d['rust_ms']    ?? 0;

    if ($rustMs > 0 && $vanMs > $rustMs) {
        $speedup = sprintf('%.1f×', $vanMs / $rustMs);
    } elseif ($rustMs > 0) {
        $speedup = sprintf('%.2f×', $vanMs / $rustMs);
    } else {
        $speedup = '—';
    }

    $lines[] = "| {$name} | {$vanT} / {$rustT} | {$vanMs} ms | {$rustMs} ms | {$speedup} |";
}

$table = implode("\n", $lines);
file_put_contents($outFile, $table . "\n");
echo $table, "\n";
PHPEOF

echo "[oss-bench] Table written to ${OUTPUT_FILE}" >&2

# ---------------------------------------------------------------------------
# Phase 4: one-line-per-suite summary to stdout
# ---------------------------------------------------------------------------
while IFS=$'\t' read -r name _git_url _ref _extra_env; do
    [[ "$name" == "name" ]] && continue
    [[ -z "$name" ]] && continue
    row=$(grep "^| ${name} " "${OUTPUT_FILE}" || true)
    if [[ -n "$row" ]]; then
        echo "SUITE ${name}: ${row}"
    else
        echo "SUITE ${name}: (no data)"
    fi
done < "$MANIFEST"
