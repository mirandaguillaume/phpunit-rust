#!/usr/bin/env bash
#
# parity_coverage.sh — guard proust's delegated RUNTIME coverage against vanilla
# PHPUnit + a coverage driver: the per-file covered-line sets must be IDENTICAL.
#
# Given a PHP project directory, this script:
#   1. Runs vanilla PHPUnit with --coverage-clover  ($PROJECT/vendor/bin/phpunit)
#   2. Runs proust with --coverage-clover           (--project $PROJECT)
#   3. Parses both Clover reports and compares, per source file, the set of lines
#      the run marked covered (count > 0).
#   4. Exits non-zero with a precise diff if the file set or any file's covered
#      line set differs.
#
# proust drives PHPUnit's OWN php-code-coverage, so the reports MUST match
# line-for-line; any divergence is a real regression in the delegation.
#
# Requires a coverage driver (pcov or xdebug). With NO driver present this is a
# clean no-op (exit 0 with a notice), so it never fails a driverless host — the
# CI job that runs it installs pcov via setup-php's `coverage: pcov`.
#
# The fixture keeps an intentionally-failing test, so a non-zero exit from either
# runner is EXPECTED and is NOT a parity failure; parity is defined purely by the
# covered-line sets matching.
#
# Usage:
#   scripts/parity_coverage.sh <project-dir> [path-to-proust-binary]
#
# The proust binary is located, in order: 2nd arg, $PROUST_BIN,
# ./target/debug/proust, ./target/release/proust, then `proust` on $PATH.

set -euo pipefail

die() {
    echo "parity_coverage: $*" >&2
    exit 1
}

[ "$#" -ge 1 ] || die "usage: $0 <project-dir> [proust-binary]"

PROJECT="$1"
[ -d "$PROJECT" ] || die "project directory not found: $PROJECT"
PROJECT="$(cd "$PROJECT" && pwd)"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

resolve_rust_bin() {
    if [ "$#" -ge 2 ] && [ -n "${2:-}" ]; then
        echo "$2"; return 0
    fi
    if [ -n "${PROUST_BIN:-}" ]; then
        echo "$PROUST_BIN"; return 0
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

PHPUNIT_BIN="$PROJECT/vendor/bin/phpunit"
[ -f "$PHPUNIT_BIN" ] || die "vanilla phpunit not found at $PHPUNIT_BIN (does the project have a vendor/ dir?)"
command -v php >/dev/null 2>&1 || die "php not found on PATH"

# No driver -> clean no-op. proust and vanilla both need pcov/xdebug to collect.
if [ "$(php -r 'echo (extension_loaded("pcov") || extension_loaded("xdebug")) ? 1 : 0;')" != "1" ]; then
    echo "parity_coverage: no coverage driver (pcov/xdebug) loaded — skipping."
    echo "                 install the pcov extension to run this gate."
    exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
VANILLA_XML="$WORK/vanilla-clover.xml"
PROUST_XML="$WORK/proust-clover.xml"

echo "==> vanilla: php $PHPUNIT_BIN --coverage-clover (cwd=$PROJECT)"
( cd "$PROJECT" && php "$PHPUNIT_BIN" --coverage-clover "$VANILLA_XML" ) >/dev/null 2>&1 || true
[ -f "$VANILLA_XML" ] || die "vanilla phpunit did not produce a Clover report (is the driver enabled?)"

echo "==> proust:  $RUST_BIN --project $PROJECT --coverage-clover"
RUST_MIN_STACK="${RUST_MIN_STACK:-67108864}" \
    "$RUST_BIN" --project "$PROJECT" --coverage-clover "$PROUST_XML" >/dev/null 2>&1 || true
[ -f "$PROUST_XML" ] || die "proust did not produce a Clover report"

cat > "$WORK/compare.php" <<'PHP'
<?php
// Compare two Clover reports by per-file covered-line sets (count > 0).
// argv: <proust-clover> <vanilla-clover>. Exit 1 on any divergence.
declare(strict_types=1);

function parse(string $path): array
{
    $xml = @simplexml_load_file($path);
    if ($xml === false) {
        fwrite(STDERR, "parity_coverage: could not parse $path\n");
        exit(2);
    }
    $out = [];
    foreach ($xml->xpath('//file') ?: [] as $file) {
        $name = basename((string) $file['name']);
        $covered = [];
        foreach ($file->line as $line) {
            if ((string) $line['type'] === 'stmt' && (int) $line['count'] > 0) {
                $covered[(int) $line['num']] = true;
            }
        }
        ksort($covered);
        $out[$name] = array_keys($covered);
    }
    return $out;
}

[$prog, $proustPath, $vanillaPath] = $argv;
$proust = parse($proustPath);
$vanilla = parse($vanillaPath);
$status = 0;

$onlyP = array_values(array_diff(array_keys($proust), array_keys($vanilla)));
$onlyV = array_values(array_diff(array_keys($vanilla), array_keys($proust)));
if ($onlyP || $onlyV) {
    fwrite(STDERR, "PARITY FAILURE: covered file set differs\n");
    fwrite(STDERR, "    proust-only : " . implode(', ', $onlyP) . "\n");
    fwrite(STDERR, "    vanilla-only: " . implode(', ', $onlyV) . "\n");
    $status = 1;
}
foreach (array_intersect(array_keys($proust), array_keys($vanilla)) as $f) {
    $dp = array_values(array_diff($proust[$f], $vanilla[$f]));
    $dv = array_values(array_diff($vanilla[$f], $proust[$f]));
    if ($dp || $dv) {
        fwrite(STDERR, "PARITY FAILURE: $f covered lines differ\n");
        fwrite(STDERR, "    proust-only : " . implode(', ', $dp) . "\n");
        fwrite(STDERR, "    vanilla-only: " . implode(', ', $dv) . "\n");
        $status = 1;
    }
}
if ($status === 0) {
    $total = array_sum(array_map('count', $vanilla));
    echo "PARITY OK: runtime coverage identical (" . count($vanilla) . " files, $total covered lines)\n";
}
exit($status);
PHP
php "$WORK/compare.php" "$PROUST_XML" "$VANILLA_XML"
