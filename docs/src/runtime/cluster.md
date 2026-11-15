# Running several servers

With `--cluster`, you can run two or more `orbit server` processes against the same PostgreSQL database. One of them, the leader, runs the engine and serves every route. The others are standbys: they serve read-only routes from the database and take over within about a second if the leader stops. This page explains the architecture, how the leader is elected and fenced, what happens during a failover, how to put a load balancer in front of the servers, and the limits of the design.

Cluster mode is for availability, not for throughput. Only one server executes workflows at a time.

## Architecture

```text
        clients, CLI, workers
                 │  one address
                 ▼
        load balancer  ── health check: GET /v1/ready (200 only on the leader)
          │          │
          ▼          ▼
   ┌───────────┐  ┌───────────┐
   │ orbit-0   │  │ orbit-1   │
   │ LEADER    │  │ STANDBY   │   standby: reads from the database,
   │ engine    │  │ read-only │   writes answer 503 NOT_LEADER
   └─────┬─────┘  └─────┬─────┘
         │ store + election session    │ store + election session
         ▼                             ▼
   ┌──────────────────────────────────────────┐
   │ PostgreSQL                                │
   │  execution_events, workflow_executions …  │
   │  orbit_cluster (term, leader)             │
   │  advisory lock: held by the leader        │
   └──────────────────────────────────────────┘
```

Each server keeps two connections to PostgreSQL: the store connection that every single-node server has, and a separate election session that holds the leader lock.

| Role | Engine | Routes served | `/v1/ready` |
|---|---|---|---|
| `leader` | Full engine, recovered from history when elected. Ticks timers, dispatches tasks, accepts results. | All | `200` |
| `standby` | Read-only engine: it loads no executions into memory and cannot write. | `GET` routes, read from the database. Every other route answers `503 NOT_LEADER`. | `503` |
| `single` | A server without `--cluster`. | All | `200` |

## Start a cluster

Point every server at the same database and give each one a node id and the address other servers should hand out for it:

```bash
orbit server --cluster --db "$ORBIT_DB" --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" \
  --node-id orbit-0 --advertise-addr http://orbit-0.orbit.internal:7233
orbit server --cluster --db "$ORBIT_DB" --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" \
  --node-id orbit-1 --advertise-addr http://orbit-1.orbit.internal:7233
```

| Option | Environment | Default | Meaning |
|---|---|---|---|
| `--cluster` | `ORBIT_CLUSTER` | off | Take part in leader election. Requires a PostgreSQL `--db`. |
| `--node-id` | `ORBIT_NODE_ID` | the host name (`$HOSTNAME`, then `/etc/hostname`) | This server's name in health output, logs and the `orbit_cluster` table. Requires `--cluster`. |
| `--advertise-addr` | `ORBIT_ADVERTISE_ADDR` | `http(s)://<--addr>` when `--addr` is not a wildcard address, otherwise none | URL that standbys return in `NOT_LEADER` responses while this server leads. Requires `--cluster`. |

Every other option works as on a single server. Give all servers the same credentials, workers file, TLS settings and engine options such as `--session-timeout`, because whichever server is elected applies its own settings.

`--cluster` with a SQLite database is refused:

```console
$ orbit server --cluster --db orbit.db
error: --cluster requires a PostgreSQL database (--db postgres://...): the servers share it for leader election and history
```

A server starts as a standby, binds its listener immediately and joins the election:

```console
INFO orbit_runtime::cluster: cluster mode: standby until elected node_id=node-1 advertise_addr="http://127.0.0.1:47975"
INFO orbit_runtime::server: ORBIT runtime listening addr=127.0.0.1:47975 tls=false
INFO orbit_runtime::cluster: cluster: elected leader; recovering executions from history node_id=node-1 term=1
INFO orbit_runtime::cluster: cluster: leader is serving node_id=node-1 term=1 active_executions=0 recovery_ms=80
```

A server that finds a leader already in place follows it:

```console
INFO orbit_runtime::cluster: cluster: standby; following the leader term=1 leader="node-1"
```

## Leader election

Once per second, every standby runs `SELECT pg_try_advisory_lock(...)` on its election session, with a fixed key. PostgreSQL grants a session-level advisory lock to one session at a time, so at most one server holds it. The holder is the leader.

PostgreSQL releases the lock when the session ends. A leader that exits, crashes or is killed loses its connection, the server ends the session, and the next standby to try gets the lock, usually within one second.

The leader re-checks its election session every second: that the session still answers, that it still holds the lock and that the term has not moved on. It serves only while its last successful check is less than 3 seconds old (the lease). When a check fails, the leader steps down in-process: it stops serving immediately, drops its engine and its election session, installs a read-only engine and rejoins the election as a standby. It does not exit, so no supervisor is needed for it to recover:

```console
ERROR orbit_runtime::cluster: cluster: lost leadership; stepped down to standby node_id=b reason=the election session failed: connection closed
INFO orbit_runtime::cluster: cluster: standby; following the leader term=3 leader="a"
```

The election session sets `idle_session_timeout` to 5 seconds (PostgreSQL 14 and later). If the leader's host becomes unreachable without closing its connection, PostgreSQL ends the silent session after 5 seconds and releases the lock. The leader's own lease of 3 seconds expires first, so the old leader has stopped serving before a new one can be elected. On older PostgreSQL versions, TCP keepalives (5 seconds idle) take that role. Recovery after election suspends the idle timeout so that a long recovery cannot release the lock.

## Fencing

Timing alone does not guarantee a single writer: a leader that is paused (a long garbage-collection-like stall, a frozen VM) may resume after another server was elected. Fencing makes that harmless.

On election, the new leader increments the term:

```sql
UPDATE orbit_cluster SET term = term + 1, leader_node = $1, leader_addr = $2, since_ms = $3
WHERE id = 1 RETURNING term
```

It then opens its store in fenced mode. Every history append runs, in the same transaction as the events:

```sql
SELECT term FROM orbit_cluster WHERE id = 1 FOR SHARE
```

and aborts if the term is not the one the server was elected with. The row lock orders the two: a new leader's term increment waits for an append in progress, and an append that starts after the increment sees the new term. A deposed leader therefore cannot append anything once a successor exists. Its requests fail with `STORAGE_ERROR`:

```text
STORAGE_ERROR: fenced: this server led term 1, but the cluster is at term 2; another server is the leader
```

and its in-memory state is unchanged, because the engine updates memory only after an append commits. Its next leadership check notices the new term and it steps down.

The check costs one extra statement per batch of events, in cluster mode only. Single-node servers do not read the cluster table.

## Failover timeline

These logs come from the end-to-end test: two servers on one database, a load balancer that routes to whichever answers `200` on `/v1/ready`, and Python and TypeScript workers. `node-1` is killed with `SIGKILL` while a payment that takes 3 seconds is in flight.

```console
# node-1 (leader, term 1), killed at about 13:55:46.19
INFO orbit_runtime::engine: execution started execution_id=ea6d4cde-f8f6-4679-adaa-9632c14dbfa6 workflow=OrderProcessing version=1.0.0

# Python worker: its long polls break, it keeps retrying
13:55:46,194 WARNING orbit poll failed: cannot reach ORBIT runtime at http://127.0.0.1:35689: Remote end closed connection without response; retrying in 0.2s

# node-2 (standby until now)
13:55:46.810072Z INFO orbit_runtime::cluster: cluster: elected leader; recovering executions from history node_id=node-2 term=2
13:55:46.859078Z INFO orbit_runtime::engine: recovered active executions from history executions=1 in_flight=1
13:55:46.860022Z INFO orbit_runtime::cluster: cluster: leader is serving node_id=node-2 term=2 active_executions=1 recovery_ms=49
13:55:46.886881Z INFO orbit_runtime::engine: worker registered worker_id=orders-python runtime=python tasks=5
13:55:46.966959Z INFO orbit_runtime::engine: worker registered worker_id=orders-typescript runtime=typescript tasks=4
```

| Time after the kill | What happens |
|---|---|
| 0 s | The leader's connections close. Workers' polls and result reports fail and are retried with backoff. PostgreSQL ends the leader's sessions and releases the lock. |
| up to 1 s | A standby's next `pg_try_advisory_lock` succeeds. It increments the term, opens a fenced store and recovers active executions from history. |
| + recovery time | The new leader serves. `/v1/ready` answers `200`, the load balancer switches to it within its check interval. |
| next request | Workers' old sessions are unknown on the new leader (`404 UNKNOWN_SESSION`); every SDK registers again and continues polling. |
| when the worker reports | A result for an attempt dispatched by the old leader is accepted: the task token is in the history the new leader recovered. |
| one `--session-timeout` after the new leader started | Attempts that nobody reported are marked uncertain and redelivered with the same idempotency key. |

In the test, the payment's worker finished after the failover and its late result was accepted; history shows one `StepSucceeded` for the charge. When the worker is killed together with the leader, the new leader records `StepAttemptUncertain` after the session timeout and dispatches the charge again (`redelivery: true`), which is the same outcome as a single-server crash. See [Delivery guarantees](guarantees.md#crash-recovery).

### In-flight work

| Work in progress at the moment of failover | What happens |
|---|---|
| Events committed by the old leader | Kept. The new leader recovers them. |
| A request the old leader had not committed | Lost. The client gets a connection error or `STORAGE_ERROR` and should retry; starting an execution with an `execution_id` makes the retry safe. |
| A task a worker is running | Its result is delivered to the new leader by the SDK's retry loop and accepted. If the worker is gone too, the attempt is redelivered after the session timeout. |
| Timers, retries, deadlines | Durable in history; the new leader fires those that are due. |
| Built-in HTTP adapter calls | The old leader's call may complete without being recorded; the new leader redelivers it after the session timeout. Use idempotency keys, as for any crash. |
| Worker sessions, metrics | In memory only. Workers register again; counters start from zero on the new leader. |
| `GET /v1/executions/{id}/wait` | A wait on the old leader breaks; a wait on a standby keeps working, re-reading the database every second. |

## Standby behavior

A standby answers `GET` routes from the database: executions, history, `wait`, queries, workflow definitions (re-read from the database on each request), the worker list and metrics. `active_executions` and `worker_sessions` in its health output count only its own, read-only engine, so they are `0`.

Every other route, including the worker protocol, answers `503` with the code `NOT_LEADER`, a `retry-after: 1` header, and the leader's advertised address in the body and in the `x-orbit-leader` header:

```console
$ curl -si -XPOST http://127.0.0.1:7302/v1/executions -H 'content-type: application/json' -d '{"workflow":"x"}'
HTTP/1.1 503 Service Unavailable
content-type: application/json
retry-after: 1
x-orbit-leader: http://127.0.0.1:7301

{"error":{"code":"NOT_LEADER","message":"this server is a standby; the leader is `a` at http://127.0.0.1:7301","leader_node":"a","leader_addr":"http://127.0.0.1:7301"}}
```

On the gRPC binding, a standby fails every call with `UNAVAILABLE` and `orbit-error-code: NOT_LEADER`.

The gate runs before authentication, so a standby answers `NOT_LEADER` even to a request without credentials, and `/v1/ready` never requires credentials.

### Clients and workers

| Client | Behavior on `503 NOT_LEADER` |
|---|---|
| `orbit` CLI | Repeats the request once against the leader address in the response. `orbit deploy --server <standby>` works. |
| Python, TypeScript, Go, Rust, Java and C# SDK workers | Treat every `5xx` like a connection error: registration, polling and result delivery are retried with backoff. After a failover they get `404 UNKNOWN_SESSION` and register again. |
| SDK clients (`Client.start`, `signal`, ...) and your applications | Get the error. Retry, or send requests through a load balancer that routes to the leader. |

## Load balancer configuration

Give clients and workers one address that always reaches the leader. Health-check `GET /v1/ready`: it returns `200` on the leader and `503` on standbys. Keep `/v1/health` for liveness, since it returns `200` on every running server.

When the leader changes, connections that are still open to the old one must be closed, otherwise a keep-alive client can keep talking to a server that is now a standby (it would get `503 NOT_LEADER` and retry forever on the same connection).

### HAProxy

```text
backend orbit
    mode http
    option httpchk GET /v1/ready
    http-check expect status 200
    default-server inter 1s fall 1 rise 1 on-marked-down shutdown-sessions
    server orbit-0 orbit-0.orbit.internal:7233 check
    server orbit-1 orbit-1.orbit.internal:7233 check
    timeout server 330s        # long polls and waits
```

### nginx

Open-source nginx has no active health checks. Use passive failover on `503`, which is what a standby returns for writes, and on connection errors:

```nginx
upstream orbit {
    server orbit-0.orbit.internal:7233 max_fails=1 fail_timeout=2s;
    server orbit-1.orbit.internal:7233 max_fails=1 fail_timeout=2s;
}
server {
    listen 7233;
    location / {
        proxy_pass http://orbit;
        proxy_next_upstream error timeout http_503 non_idempotent;
        proxy_next_upstream_tries 2;
        proxy_read_timeout 330s;
    }
}
```

`non_idempotent` lets nginx retry a `POST` that a standby refused; a standby never applies a request it refuses, so the retry is safe. With nginx Plus, use `health_check uri=/v1/ready match=ready;` instead.

### Kubernetes

Use a `StatefulSet` with two replicas and a readiness probe on `/v1/ready`. The `Service` then routes only to the leader, and pods that become standbys are removed from its endpoints:

```yaml
readinessProbe:
  httpGet: { path: /v1/ready, port: http }
  periodSeconds: 1
  failureThreshold: 1
livenessProbe:
  httpGet: { path: /v1/health, port: http }
  periodSeconds: 10
```

See [Deployment](deployment.md#kubernetes-with-two-replicas) for the full manifest.

## Health fields

`/v1/health` and `/v1/ready` include the node's view of the cluster:

```console
$ curl -s http://127.0.0.1:7302/v1/health
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.2.0","ir_versions":["orbit-ir/1","orbit-ir/2"],"active_executions":0,"worker_sessions":0,"role":"standby","node_id":"b","term":1,"leader":{"node_id":"a","addr":"http://127.0.0.1:7301"}}
$ curl -s http://127.0.0.1:7302/v1/ready
{"role":"standby","node_id":"b","term":1,"leader":{"node_id":"a","addr":"http://127.0.0.1:7301"},"ready":false}
```

| Field | Meaning |
|---|---|
| `role` | `leader`, `standby`, or `single` without `--cluster`. |
| `node_id` | This server's `--node-id` (`null` without `--cluster`). |
| `term` | The leadership term this server last saw. It increases by one at every election. |
| `leader` | `{"node_id", "addr"}` of the current leader as recorded in `orbit_cluster`, or `null`. |

To see the lock holders from PostgreSQL, look for the election sessions, which are named `orbit-cluster:<node id>`:

```sql
SELECT application_name, state, backend_start FROM pg_stat_activity
WHERE application_name LIKE 'orbit-cluster:%';
```

## Limits

- **One active server.** Standbys do not execute workflows, dispatch tasks or accept results. Adding servers adds availability, not throughput; size the leader as you would a single server.
- **PostgreSQL only.** The database is the single point of truth and the arbiter. Its own availability (replication, managed failover) bounds the cluster's. After a database failover, servers reconnect and elect a leader again.
- **Failover is not instant.** Expect about one second after a crash, plus recovery time, plus your load balancer's check interval; up to about 5 seconds after a network partition. Attempts nobody reports are redelivered one `--session-timeout` after the new leader started.
- **Same configuration everywhere.** The leader applies its own options. Differences between servers (session timeout, workers file, HTTP adapter) change behavior at each failover.
- **Version skew.** During a rolling upgrade, the new version may become leader before the old one is gone. Upgrade standbys first, then stop the leader; never let an older version lead a database written by a newer one.

## See also

- [Running the server](server.md)
- [Storage](storage.md#cluster-table)
- [Deployment](deployment.md#kubernetes-with-two-replicas)
- [Delivery guarantees](guarantees.md)
- [HTTP API: GET /v1/ready](../reference/http-api.md#get-v1ready)
- [CLI: orbit server](../reference/cli.md#orbit-server)
