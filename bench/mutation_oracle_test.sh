#!/usr/bin/env bash
# Oracle gate: on the fixture, `proust --mutate` and Infection must agree on which
# mutants ESCAPE for the V1 mutator set. Runs in a pcov-equipped image with composer
# network access (CI + local Docker). Fails on any disagreement.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROJ="$HERE/fixtures/mutation_oracle"
PROUST="${PROUST:-$HERE/../target/release/proust}"
# Resolve PROUST to an absolute path BEFORE we cd into the fixture — a relative
# override (e.g. CI's `PROUST=target/release/proust`) would break after the cd.
case "$PROUST" in /*) ;; *) PROUST="$(pwd)/$PROUST" ;; esac

cd "$PROJ"

if [ ! -x vendor/bin/infection ] || [ ! -x vendor/bin/phpunit ]; then
    composer install --no-interaction --no-progress --quiet
fi

# --- Infection: ground-truth escaped set as "MutatorName line" (php, no jq) ---
rm -f infection.json
vendor/bin/infection --no-progress --threads=4 --no-ansi >/tmp/infection_run.log 2>&1 || true
# Infection buckets as "MutatorName line". proust folds Timeout (and fatal crashes)
# into its `killed` set, so the Infection-side equivalent unions killed + timeouted +
# errored — all three are "caught" mutants (loop mutators inevitably time out).
inf_set() { php -r '$d=json_decode(file_get_contents("infection.json"),true)?:[];foreach((array)$argv[1]===[]?[]:explode(",",$argv[1]) as $k){foreach($d[$k]??[] as $m){echo $m["mutator"]["mutatorName"]." ".$m["mutator"]["originalStartLine"]."\n";}}' "$1"; }
inf_set escaped | sort > /tmp/infection_escaped.txt
inf_set killed,timeouted,errored | sort > /tmp/infection_killed.txt

# --- proust: same shape from --mutation-escaped-json (holds escaped AND killed) ---
rm -f proust_escaped.json
"$PROUST" --project . --mutate --workers 4 --mutation-escaped-json proust_escaped.json \
    >/tmp/proust_run.log 2>&1 || true
if [ ! -f proust_escaped.json ]; then
    echo "proust produced no results JSON — its output was:" >&2
    cat /tmp/proust_run.log >&2
    exit 1
fi
proust_set() { php -r '$d=json_decode(file_get_contents("proust_escaped.json"),true)?:[];foreach($d[$argv[1]]??[] as $m){echo $m["mutator"]." ".$m["line"]."\n";}' "$1"; }
proust_set escaped | sort > /tmp/proust_escaped.txt
proust_set killed  | sort > /tmp/proust_killed.txt

echo "=== escaped: infection | proust ==="; paste /tmp/infection_escaped.txt /tmp/proust_escaped.txt
echo "=== killed count: infection=$(wc -l < /tmp/infection_killed.txt) proust=$(wc -l < /tmp/proust_killed.txt) ==="

fail=0
if ! diff -u /tmp/infection_escaped.txt /tmp/proust_escaped.txt; then
    echo "ORACLE MISMATCH: escaped sets differ" >&2; fail=1
fi
if ! diff -u /tmp/infection_killed.txt /tmp/proust_killed.txt; then
    echo "ORACLE MISMATCH: killed sets differ (a mutator was generated/classified differently)" >&2; fail=1
fi
if [ "$fail" -ne 0 ]; then exit 1; fi
echo "oracle OK: killed AND escaped sets match ($(( $(wc -l < /tmp/proust_killed.txt) + $(wc -l < /tmp/proust_escaped.txt) )) mutants)"
