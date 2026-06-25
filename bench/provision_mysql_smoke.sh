#!/usr/bin/env bash
# bench/provision_mysql_smoke.sh — MySQL provisioning gate.
#
# Runs bench/provision_mysql_smoke.php inside the CI image (which carries
# pdo_mysql) against a MySQL server: seeds a template with an FK table, clones it
# per worker via the MysqlProvisioner, and asserts the clone copies schema+data,
# stays isolated from the template, is reachable through dbHandle's credential
# extraction, and is reclaimed by gc. FAILS (non-zero) on any divergence.
#
# Env (all optional, CI-friendly defaults):
#   IMAGE       CI image with pdo_mysql            (default proust-ci:php84)
#   DOCKER_NET  docker network for the run         (default host)
#   MY_HOST/MY_PORT/MY_USER/MY_PASS/MY_DB          (default 127.0.0.1/3306/root/root/app_test)
set -uo pipefail

IMAGE="${IMAGE:-proust-ci:php84}"
DOCKER_NET="${DOCKER_NET:-host}"
MY_HOST="${MY_HOST:-127.0.0.1}"; MY_PORT="${MY_PORT:-3306}"
MY_USER="${MY_USER:-root}"; MY_PASS="${MY_PASS:-root}"; MY_DB="${MY_DB:-app_test}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

exec docker run --rm --network "$DOCKER_NET" -v "$REPO":/w -w /w \
    -e MY_HOST="$MY_HOST" -e MY_PORT="$MY_PORT" -e MY_USER="$MY_USER" \
    -e MY_PASS="$MY_PASS" -e MY_DB="$MY_DB" \
    "$IMAGE" php /w/bench/provision_mysql_smoke.php
