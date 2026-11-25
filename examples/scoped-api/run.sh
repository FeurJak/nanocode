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
# DRIVE_THREADS (driver executor threads, default 2), PG_SYNC (synchronous_commit for the app
# role, default on; off is only a diagnostic),
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
  -c "ALTER ROLE app SET idle_in_transaction_session_timeout = '15s'" \
  -c "ALTER ROLE app SET synchronous_commit = '${PG_SYNC:-on}'"
# Resource counters around each drive: Postgres CPU from the container's cgroup, process CPU from
# /proc ticks, commits from pg_stat_database, WAL bytes and fsyncs from pg_stat_wal and pg_stat_io.
ticks() { awk '{print $14 + $15}' "/proc/$1/stat" 2>/dev/null || echo 0; }
pg_usec() { docker exec "$name" awk '$1 == "usage_usec" {print $2}' /sys/fs/cgroup/cpu.stat; }
pg_counts() {
  sql -At -F' ' -d "$1" -c "SELECT (SELECT xact_commit FROM pg_stat_database WHERE datname = '$1'),
    (SELECT wal_bytes FROM pg_stat_wal),
    (SELECT coalesce(sum(fsyncs), 0) FROM pg_stat_io WHERE object = 'wal')"
}
hz=$(getconf CLK_TCK)
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
  read -r commits0 wal0 syncs0 <<<"$(pg_counts "$db")"
  pg0=$(pg_usec) t0=$(date +%s.%N) app0=()
  for pid in "${pids[@]}"; do app0+=("$(ticks "$pid")"); done
  pinned "$bin" drive "postgres://postgres@127.0.0.1:$port/$db" replicas="$list" parts="$parts" \
    concurrency="$concurrency" seconds="$duration" threads="${DRIVE_THREADS:-2}" "${extra[@]}" \
    >"$out/result-$n.json" 2>"$out/drive-$n.log" &
  driver=$! drive_ticks=0
  while kill -0 "$driver" 2>/dev/null; do
    now=$(ticks "$driver"); [ "$now" -gt 0 ] && drive_ticks=$now
    sleep 0.5
  done
  wait "$driver" && result=$(cat "$out/result-$n.json") || result='{"failed":true}'
  [ -n "$result" ] || result='{"failed":true}'
  t1=$(date +%s.%N) pg1=$(pg_usec) app=()
  for i in "${!pids[@]}"; do app+=("$(ticks "${pids[$i]}") ${app0[$i]}"); done
  read -r commits1 wal1 syncs1 <<<"$(pg_counts "$db")"
  usage=$(printf '%s\n' "${app[@]}" | jq -R -s -c --argjson s "$(awk -v a="$t1" -v b="$t0" 'BEGIN {print a - b}')" --argjson hz "$hz" \
    --argjson pg "$(( pg1 - pg0 ))" --argjson commits "$(( commits1 - commits0 ))" \
    --argjson wal "$(( wal1 - wal0 ))" --argjson syncs "$(( syncs1 - syncs0 ))" --argjson drive "$drive_ticks" \
    '(split("\n") | map(select(length > 0) | split(" ") | map(tonumber) | (.[0] - .[1]) / $hz / $s * 100 | round)) as $app
     | {pg_cpu_pct: ($pg / 1e6 / $s * 100 | round), app_cpu_pct: $app, app_cpu_pct_sum: ($app | add),
        drive_cpu_pct: ($drive / $hz / $s * 100 | round), pg_commits_per_s: ($commits / $s | round),
        wal_bytes_per_s: ($wal / $s | round), wal_fsyncs_per_s: ($syncs / $s | round)}')
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
  logs=$(docker logs --since "$since" "$name" 2>&1)
  count() { grep -c "$1" <<<"$logs" || true; }
  jq -c --arg spec "$spec" --argjson waits "$(count 'still waiting for')" \
    --argjson statement "$(count 'canceling statement due to statement timeout')" \
    --argjson lock "$(count 'canceling statement due to lock timeout')" \
    --argjson deadlocks "$(count 'deadlock detected')" --argjson load "$(cut -d' ' -f1 /proc/loadavg)" \
    --argjson usage "$usage" \
    '{spec: $spec} + . + $usage + {rows_per_commit: ((.replica_stats.rows // 0) / ((.replica_stats.commits // 0) + 1e-9) * 100 | round / 100),
      forward_pct: ((.replica_stats.forwarded // 0) / (((.replica_stats.requests // 0) - (.replica_stats.forwarded // 0)) + 1e-9) * 100 | round)}
      + {load1: $load, pg_log: {still_waiting_for: $waits, statement_timeouts: $statement, lock_timeouts: $lock, deadlocks: $deadlocks}}' <<<"$result"
  sql -c "DROP DATABASE $db WITH (FORCE)"
done
