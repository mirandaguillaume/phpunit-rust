#!/usr/bin/env bash
# bench/parity_functional_pg.sh — parallel functional-test parity gate.
#
# Proves that a framework app whose functional suite isolates each test through a
# PHPUnit <extensions> bootstrap (DAMADoctrineTestBundle) runs IN PARALLEL at
# parity under proust's per-worker DB provisioning (--provision-db). Clones a
# pinned Symfony app, points it at PostgreSQL, loads schema+fixtures, then
# compares vanilla PHPUnit (the oracle) against `proust --workers N
# --provision-db`. FAILS (non-zero) if the Tests/failed+errored counts diverge.
#
# Everything runs inside the CI image (which carries pdo_pgsql); the container
# reaches the Postgres at $PG_HOST via the chosen docker network.
#
# Env (all optional, with CI-friendly defaults):
#   IMAGE       CI image with pdo_pgsql            (default proust-ci:php84)
#   BINARY      proust release binary on the host  (default target/release/proust)
#   DOCKER_NET  docker network for the runs        (default host)
#   PG_HOST/PG_PORT/PG_USER/PG_PASS/PG_DB          (default 127.0.0.1/5432/postgres/pg/app_test)
#   APP_REF     Symfony Demo git ref               (default v3.0.2)
#   WORKERS     proust worker count                (default 4)
#   WORK        scratch dir for the clone          (default a mktemp dir)
set -uo pipefail

IMAGE="${IMAGE:-proust-ci:php84}"
BINARY="${BINARY:-$(pwd)/target/release/proust}"
DOCKER_NET="${DOCKER_NET:-host}"
PG_HOST="${PG_HOST:-127.0.0.1}"; PG_PORT="${PG_PORT:-5432}"
PG_USER="${PG_USER:-postgres}"; PG_PASS="${PG_PASS:-pg}"; PG_DB="${PG_DB:-app_test}"
APP_REF="${APP_REF:-v3.0.2}"
WORKERS="${WORKERS:-4}"
WORK="${WORK:-$(mktemp -d)}"
APP="$WORK/symfony-demo"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BASEURL="postgresql://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${PG_DB}?serverVersion=16"
PROVBASE="postgres://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${PG_DB}"

fail(){ echo "PARITY-FUNCTIONAL FAIL: $*" >&2; exit 1; }

echo "== clone Symfony Demo @ ${APP_REF} =="
[ -d "$APP/.git" ] || git clone --depth 1 --branch "$APP_REF" https://github.com/symfony/demo "$APP" || fail "clone failed"

# Run a command inside the CI image against the app, with the pg env wired in.
app(){ docker run --rm --network "$DOCKER_NET" -v "$APP":/app -w /app \
        -e APP_ENV=test -e "DATABASE_URL=$BASEURL" "$IMAGE" bash -c "$1"; }

echo "== composer install + disable the app's own per-worker dbname suffix =="
app 'git config --global --add safe.directory /app; composer install --no-interaction --prefer-dist --no-progress --ignore-platform-reqs' >/dev/null 2>&1 \
  || fail "composer install failed"
# proust's per-worker clone DSN must be authoritative — turn off the app's own
# TEST_TOKEN-based dbname suffix so it doesn't append _test to the clone name.
app "sed -i \"s/dbname_suffix: '_test%env(default::TEST_TOKEN)%'/dbname_suffix: ''/\" config/packages/doctrine.yaml" >/dev/null 2>&1

echo "== create schema + load fixtures into the pg base =="
app 'php bin/console doctrine:schema:create 2>&1 | tail -2' || fail "schema:create failed"
app 'php bin/console doctrine:fixtures:load --no-interaction 2>&1 | tail -2' || fail "fixtures:load failed"

echo "== vanilla PHPUnit (oracle) =="
VAN=$(app 'vendor/bin/phpunit 2>&1' || true)
v_total=$(printf '%s' "$VAN" | grep -oiE 'Tests: [0-9]+|OK \([0-9]+' | grep -oE '[0-9]+' | head -1)
v_fail=$(printf '%s' "$VAN" | grep -oiE 'Failures: [0-9]+' | grep -oE '[0-9]+' | head -1)
v_err=$(printf '%s' "$VAN" | grep -oiE 'Errors: [0-9]+' | grep -oE '[0-9]+' | head -1)
v_bad=$(( ${v_fail:-0} + ${v_err:-0} ))
echo "  vanilla: total=${v_total:-?} failed+errored=${v_bad}"
[ -n "${v_total:-}" ] || fail "could not parse vanilla output:\n$VAN"

echo "== proust --workers $WORKERS --provision-db =="
PR=$(docker run --rm --init --network "$DOCKER_NET" --tmpfs /tmp:rw,exec,size=2g \
  -e "DATABASE_URL=$BASEURL" \
  -v "$BINARY":/opt/proust/bin/proust:ro -v "$REPO/php":/opt/php:ro -v "$APP":/app "$IMAGE" \
  /opt/proust/bin/proust --project /app --workers "$WORKERS" --provision-db "$PROVBASE" 2>&1 || true)
echo "$PR" | grep -iE 'provision|event bridge active|locked|^Tests:' | sed 's/^/  /'
p_line=$(printf '%s' "$PR" | grep -iE '^Tests:' | tail -1)
p_total=$(printf '%s' "$p_line" | grep -oE '[0-9]+ total' | grep -oE '[0-9]+')
p_fail=$(printf '%s' "$p_line" | grep -oE '[0-9]+ failed' | grep -oE '[0-9]+')
p_err=$(printf '%s' "$p_line" | grep -oE '[0-9]+ errored' | grep -oE '[0-9]+')
p_bad=$(( ${p_fail:-0} + ${p_err:-0} ))
echo "  proust : total=${p_total:-?} failed+errored=${p_bad}"
[ -n "${p_total:-}" ] || fail "could not parse proust output:\n$PR"

printf '%s' "$PR" | grep -qi 'database is locked' && fail "cross-worker contention: 'database is locked' at --workers $WORKERS"

echo "== parity gate =="
[ "$p_total" = "$v_total" ] || fail "test count differs (proust $p_total vs vanilla $v_total)"
[ "$p_bad" = "$v_bad" ]     || fail "failed+errored differs (proust $p_bad vs vanilla $v_bad)"
echo "PARITY-FUNCTIONAL OK: proust --workers $WORKERS matches vanilla ($p_total tests, $p_bad non-passing) with per-worker DB isolation."
