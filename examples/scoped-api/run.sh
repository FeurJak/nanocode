#!/usr/bin/env bash
# examples/scoped-api/run.sh: start Postgres 18 in Docker (port PG_PORT, default 5546) if it is not
# running, then for each argument set: a fresh database, migrate, R replicas, the driver, and one
# JSON results line on stdout with the Postgres log counts added under pg_log.
#   examples/scoped-api/run.sh "R=1" "R=2" "R=4" "R=4 kill=0@10" "R=4 handoff=0@10"
# An argument set takes R (replicas, default 1), C (concurrency, default 20), kill=I@S (SIGKILL
# replica I at S seconds) or handoff=I@S (drain and release replica I at S seconds), and passes any
# other KEY=VALUE to the driver (scopes=, writes=, retry_ms=, ...).
# Env: PG_PORT, PG_NAME (container, default scoped-api-pg), PG_IMAGE (default postgres:18),
# PG_CPUS (docker --cpuset-cpus), APP_CPUS (taskset list for replicas and driver), PARTS (default
# 32), DURATION (seconds per run, default 30), REPLICA_PORT (first replica port, default 7100),
# THREADS (executor threads per replica, default 2), POOL (connections per replica, default 8),
# DRIVE_THREADS (driver executor threads, default 2),
# SCOPED_BIN (default $CARGO_TARGET_DIR/release/scoped-api, else target/release/scoped-api).
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$root"
port=${PG_PORT:-5546}
name=${PG_NAME:-scoped-api-pg}
image=${PG_IMAGE:-postgres:18}
parts=${PARTS:-32}
duration=${DURATION:-30}
base=${REPLICA_PORT:-7100}
bin=${SCOPED_BIN:-${CARGO_TARGET_DIR:-target}/release/scoped-api}
out=output/scoped-api
mkdir -p "$out"
pinned() { if [ -n "${APP_CPUS:-}" ]; then exec taskset -c "$APP_CPUS" "$@"; fi; exec "$@"; }
sql() { docker exec -i "$name" psql -U postgres -X -q -v ON_ERROR_STOP=1 "$@"; }
if ! docker ps --format '{{.Names}}' | grep -qx "$name"; then
  docker run -d --rm --name "$name" -p "127.0.0.1:$port:5432" -e POSTGRES_HOST_AUTH_METHOD=trust \
    --shm-size=1g ${PG_CPUS:+--cpuset-cpus="$PG_CPUS"} "$image" \
    -c max_connections=400 -c shared_buffers=1GB -c max_wal_size=16GB -c log_lock_waits=on \
    -c deadlock_timeout=100ms >/dev/null
fi
until docker exec "$name" pg_isready -U postgres -q 2>/dev/null; do sleep 0.2; done
until sql -c "SELECT 1" >/dev/null 2>&1; do sleep 0.2; done
sql -c "DO \$\$ BEGIN CREATE ROLE app LOGIN; EXCEPTION WHEN duplicate_object THEN NULL; END \$\$"
sql -c "ALTER ROLE app SET statement_timeout = '15s'" \
  -c "ALTER ROLE app SET lock_timeout = '5s'" \
  -c "ALTER ROLE app SET idle_in_transaction_session_timeout = '15s'"
n=0
for spec in "$@"; do
  n=$((n + 1))
  replicas=1 concurrency=20 event="" extra=()
  for pair in $spec; do
    case "$pair" in
      R=*) replicas=${pair#R=} ;;
      C=*) concurrency=${pair#C=} ;;
      kill=* | handoff=*) event=$pair ;;
      *) extra+=("$pair") ;;
    esac
  done
  db="scoped_$$_$n"
  sql -c "CREATE DATABASE $db OWNER app"
  url="postgres://app@127.0.0.1:$port/$db"
  "$bin" migrate "$url" parts="$parts" >/dev/null
  since=$(date +%s.%N)
  pids=() list=""
  for ((i = 0; i < replicas; i++)); do
    pinned "$bin" serve "$url" listen="127.0.0.1:$((base + i))" parts="$parts" \
      threads="${THREADS:-2}" pool="${POOL:-8}" >"$out/replica-$n-$i.log" 2>&1 &
    pids+=($!)
    list="$list${list:+,}127.0.0.1:$((base + i))"
  done
  case "$event" in
    kill=*) at=${event#kill=}; victim=${at%@*}
      extra+=("kill=${pids[$victim]}@${at#*@}" "victim=127.0.0.1:$((base + victim))") ;;
    handoff=*) at=${event#handoff=}; victim=${at%@*}
      extra+=("handoff=127.0.0.1:$((base + victim))@${at#*@}") ;;
  esac
  result=$(pinned "$bin" drive "postgres://postgres@127.0.0.1:$port/$db" replicas="$list" parts="$parts" \
    concurrency="$concurrency" seconds="$duration" threads="${DRIVE_THREADS:-2}" "${extra[@]}" 2>"$out/drive-$n.log") || result='{"failed":true}'
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
  logs=$(docker logs --since "$since" "$name" 2>&1)
  count() { grep -c "$1" <<<"$logs" || true; }
  jq -c --arg spec "$spec" --argjson waits "$(count 'still waiting for')" \
    --argjson statement "$(count 'canceling statement due to statement timeout')" \
    --argjson lock "$(count 'canceling statement due to lock timeout')" \
    --argjson deadlocks "$(count 'deadlock detected')" --argjson load "$(cut -d' ' -f1 /proc/loadavg)" \
    '{spec: $spec} + . + {load1: $load, pg_log: {still_waiting_for: $waits, statement_timeouts: $statement, lock_timeouts: $lock, deadlocks: $deadlocks}}' <<<"$result"
  sql -c "DROP DATABASE $db WITH (FORCE)"
done
