# scoped-api

A template for an API built on Nanocode that scales out like Durable Objects: identical,
stateless replica processes over one Postgres, each scope (a key the API is addressed by) owned
by exactly one replica at a time, with no coordination code in the app.

```text
POST /scope/{id}/append  {"k": "...", "v": "..."}  ->  {"seq": n}
GET  /scope/{id}                                     ->  {"head": n, "values": {k: v}}
GET  /scope/{id}/log                                 ->  [[seq, body], ...]
GET  /admin/held | /admin/stats      POST /admin/drain
```

A scope's rows live in the partitioned turn log (`nanocode_turnlog::parted`). `place` maps a
scope to a partition, and each partition's lease row (`nanocode_log_fence`) names the replica
that holds it, by its listen address.

- Ownership comes only from the lease. Every replica heartbeats a membership row on the
  database clock, computes the rendezvous assignment over live members
  (`nanocode_partitions::plan`), claims the partitions assigned to it, renews them, and
  releases the ones assigned elsewhere. There are no locks, advisory locks or leader election.
- A claimed partition gets one owner task. It holds every live scope's fold by value, takes
  requests from a bounded queue, and commits each batch as one fenced transaction at its epoch
  (`PartStore::commit`). Reads of a loaded scope come from the snapshot it publishes after each
  commit. A commit refused as `Fenced` means the lease moved: the owner answers `lost` and stops.
- A request on any replica goes to the scope's owner: in process when the replica holds the
  partition, otherwise forwarded over HTTP to the holder read from the lease row (cached, and
  re-read and retried once on a refusal). A replica with nothing to forward to answers
  `unowned` (503) and the client retries.
- A planned handoff (`POST /admin/drain`) leaves membership, drains each owner (its batch in
  flight lands, queued work is refused as `draining`) and releases each lease, so the next
  assignee claims it on its next tick. A killed replica's partitions are claimed once their
  leases lapse (`lease_ms` after the last renewal, plus `grace_ms`); membership only decides who
  claims them.
- Schema changes run only in `migrate`. `serve` checks the schema once and exits if it is
  behind. The replicas' Postgres role carries `statement_timeout`, `lock_timeout` and
  `idle_in_transaction_session_timeout`, and lease traffic uses its own pool so data load never
  delays a heartbeat or renewal.

## Running it

```sh
CARGO_TARGET_DIR=target cargo build --release -p nanocode-scoped-api
examples/scoped-api/run.sh "R=1" "R=2" "R=4" "R=4 kill=0@10" "R=4 handoff=0@10"
```

`run.sh` starts `postgres:18` in Docker (`log_lock_waits=on`, `deadlock_timeout=100ms`) on
port 5546, and for each argument set creates a database, migrates it, starts R replicas, runs the
driver for `DURATION` seconds (default 30) and prints one JSON line. See its header for every
option.

The driver (`scoped-api drive`) runs C closed-loop clients (default 20) over K scopes (default
10000), half appends and half reads, each client pinned to one replica so most requests are
forwarded. It reports throughput, p50/p99 latency, errors and retries by kind, and checks at
the end that every acknowledged append is in its scope's log at its seq with its body and that
every log is gapless. A sampler polls Postgres every 100 ms for backends waiting on a lock, for
ungranted `pg_locks` and for the statements that wait; `run.sh` adds the Postgres log's
`still waiting for`, statement-timeout, lock-timeout and deadlock counts. With `kill=I@S` or
`handoff=I@S`, `event.stall_ms_max` is the longest gap between acknowledged appends in the
partitions that moved, from the event on. The clients are closed-loop, so while they retry on
moved scopes they stop issuing work elsewhere: `unmoved_gap_ms_max` shows that effect.
