#!/usr/bin/env bash
# Oracle gate: on the fixture, `proust --mutate` and Infection must agree on which
# mutants ESCAPE for the V1 mutator set. Runs in a pcov-equipped image with composer
# network access (CI + local Docker). Fails on any disagreement.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROJ="$HERE/fixtures/mutation_oracle"
PROUST="${PROUST:-$HERE/../target/release/proust}"

cd "$PROJ"

if [ ! -x vendor/bin/infection ] || [ ! -x vendor/bin/phpunit ]; then
    composer install --no-interaction --no-progress --quiet
fi

# --- Infection: ground-truth escaped set as "MutatorName line" (php, no jq) ---
rm -f infection.json
vendor/bin/infection --no-progress --threads=4 --no-ansi >/tmp/infection_run.log 2>&1 || true
php -r '$d=json_decode(file_get_contents("infection.json"),true)?:[];foreach($d["escaped"]??[] as $m){echo $m["mutator"]["mutatorName"]." ".$m["mutator"]["originalStartLine"]."\n";}' \
    | sort > /tmp/infection_escaped.txt

# --- proust: same shape from --mutation-escaped-json ---
rm -f proust_escaped.json
"$PROUST" --project . --mutate --workers 4 --mutation-escaped-json proust_escaped.json \
    >/tmp/proust_run.log 2>&1 || true
if [ ! -f proust_escaped.json ]; then
    echo "proust produced no escaped JSON — its output was:" >&2
    cat /tmp/proust_run.log >&2
    exit 1
fi
php -r '$d=json_decode(file_get_contents("proust_escaped.json"),true)?:[];foreach($d["escaped"]??[] as $m){echo $m["mutator"]." ".$m["line"]."\n";}' \
    | sort > /tmp/proust_escaped.txt

echo "=== Infection escaped ==="; cat /tmp/infection_escaped.txt
echo "=== proust escaped ===";    cat /tmp/proust_escaped.txt

if diff -u /tmp/infection_escaped.txt /tmp/proust_escaped.txt; then
    echo "oracle OK: escaped sets match"
else
    echo "ORACLE MISMATCH: proust and Infection disagree on escaped mutants" >&2
    exit 1
fi
