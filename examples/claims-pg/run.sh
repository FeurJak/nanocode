#!/usr/bin/env bash
# examples/claims-pg/run.sh: start a private Postgres 16 under output/claims-pg/pg (port 5544) if it
# is not running, then run claims-pg once per argument set, each on a fresh database.
#   examples/claims-pg/run.sh "parts=16 claimers=64" "parts=64 claimers=64"
# Env: PG_BIN (default /usr/lib/postgresql/16/bin), PG_CPUS and BENCH_CPUS (taskset lists for the
# server and the client; default: no pinning), CLAIMS_BIN (default target/release/claims-pg),
# PLAN_CACHE_MODE (set on each run's database, such as force_generic_plan).
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$root"
pg_bin=${PG_BIN:-/usr/lib/postgresql/16/bin}
data=output/claims-pg/pg
port=5544
bin=${CLAIMS_BIN:-target/release/claims-pg}
pin() { local cpus=$1; shift; if [ -n "$cpus" ]; then taskset -c "$cpus" "$@"; else "$@"; fi; }
if [ ! -f "$data/PG_VERSION" ]; then
  mkdir -p "$data"
  "$pg_bin/initdb" -D "$data" -U nc --auth=trust >/dev/null
  cat >>"$data/postgresql.conf" <<CONF
port = $port
listen_addresses = '127.0.0.1'
unix_socket_directories = ''
max_connections = 400
shared_buffers = 4GB
wal_buffers = 64MB
max_wal_size = 16GB
checkpoint_timeout = 30min
synchronous_commit = on
fsync = on
CONF
fi
if ! "$pg_bin/pg_isready" -h 127.0.0.1 -p $port -q; then
  pin "${PG_CPUS:-}" "$pg_bin/pg_ctl" -D "$data" -l "$data/../server.log" -w start >/dev/null
fi
n=0
for args in "$@"; do
  n=$((n + 1))
  db="claims_$$_$n"
  "$pg_bin/createdb" -h 127.0.0.1 -p $port -U nc "$db"
  if [ -n "${PLAN_CACHE_MODE:-}" ]; then
    "$pg_bin/psql" -h 127.0.0.1 -p $port -U nc -X -q -d "$db" \
      -c "ALTER DATABASE $db SET plan_cache_mode = $PLAN_CACHE_MODE"
  fi
  # shellcheck disable=SC2086
  pin "${BENCH_CPUS:-}" "$bin" "postgres://nc@127.0.0.1:$port/$db" $args || true
  "$pg_bin/dropdb" -h 127.0.0.1 -p $port -U nc "$db"
done
