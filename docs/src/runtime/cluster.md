# Running several servers

With `--cluster`, you can run two or more `orbit server` processes against the same PostgreSQL database. One of them, the leader, runs the engine and serves every route. The others are standbys: they serve read-only routes from the database and take over within about a second if the leader stops. This page explains the architecture, how the leader is elected and fenced, what happens during a failover, how to put a load balancer in front of the servers, and the limits of the design.

This default mode, `--cluster-mode standby`, is for availability, not for throughput: only one server executes workflows at a time. With `--cluster-mode shared`, every server is active and they share the work; see [Shared mode](#shared-mode-several-active-servers).

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
| `role` | `leader`, `standby`, `shared` in [shared mode](#health-fields-1), or `single` without `--cluster`. |
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

## Shared mode (several active servers)

With `--cluster --cluster-mode shared`, every server is active. Each one serves every route, starts and runs executions, and hands tasks to the workers that poll it. When a server dies, the others take over its executions. Use it when one server is not enough for your load, or when you want every replica to take traffic.

The sections above describe the default mode, `--cluster-mode standby`. Shared mode needs no election: there is no leader, no term and no `NOT_LEADER` answer.

### Architecture

```text
              clients, CLI, workers
                       │  one address
                       ▼
        load balancer (round robin) ── health check: GET /v1/ready (200 on every live server)
          │              │              │
          ▼              ▼              ▼
   ┌────────────┐ ┌────────────┐ ┌────────────┐
   │ orbit-0    │ │ orbit-1    │ │ orbit-2    │   every server: full engine for the
   │ engine:    │ │ engine:    │ │ engine:    │   executions it owns, claims tasks for
   │ owned exec │ │ owned exec │ │ owned exec │   the workers that poll it, reads
   └─────┬──────┘ └─────┬──────┘ └─────┬──────┘   everything else from the database
         │              │              │   store, queue, inbox, notifications
         ▼              ▼              ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ PostgreSQL                                                          │
   │  execution_events, workflow_executions …  history, as before        │
   │  orbit_nodes      heartbeats                                        │
   │  orbit_owners     owner and epoch per running execution             │
   │  orbit_tasks      shared task queue                                 │
   │  orbit_inbox      commands for an execution's owner                 │
   │  orbit_sessions   worker sessions, usable through any server        │
   │  LISTEN/NOTIFY    orbit_tasks, orbit_inbox, orbit_done, orbit_defs  │
   └─────────────────────────────────────────────────────────────────────┘
```

Each server keeps seven connections to PostgreSQL: the store (history appends), four for request-path statements (claims, forwarding, sessions), one for its heartbeat, takeovers and inbox, and one that listens for notifications. Size `max_connections` for that, plus your other clients.

| What | Where it lives | Who acts on it |
|---|---|---|
| History of an execution | `execution_events`, as on one server | Only its owner appends. Any server reads. |
| Running executions | In the memory of their owner, rebuilt from history on takeover | The owner ticks timers, retries, deadlines and child workflows. |
| Worker tasks | `orbit_tasks` | Any server claims them for the workers polling it. |
| Commands for an execution owned elsewhere | `orbit_inbox` | Its owner applies them in order. |
| Worker sessions | `orbit_sessions`, cached by every server that serves the worker | A worker can poll, heartbeat and report through any server. |
| Workflow definitions | `workflow_definitions`, as on one server | Any server registers; the others reload on a notification. |

### Start a shared cluster

Point every server at the same database, with `--cluster --cluster-mode shared` and a unique node id:

```bash
orbit server --cluster --cluster-mode shared --db "$ORBIT_DB" --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" \
  --node-id orbit-0 --advertise-addr http://orbit-0.orbit.internal:7233
orbit server --cluster --cluster-mode shared --db "$ORBIT_DB" --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" \
  --node-id orbit-1 --advertise-addr http://orbit-1.orbit.internal:7233
orbit server --cluster --cluster-mode shared --db "$ORBIT_DB" --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" \
  --node-id orbit-2 --advertise-addr http://orbit-2.orbit.internal:7233
```

| Option | Environment | Default | Meaning |
|---|---|---|---|
| `--cluster-mode` | `ORBIT_CLUSTER_MODE` | `standby` | `shared` makes every server active. Requires `--cluster`. |
| `--node-timeout` | `ORBIT_NODE_TIMEOUT` | `5` | Seconds without a heartbeat after which a server is dead and the others take over its executions. At least `2`. |
| `--node-id` | `ORBIT_NODE_ID` | the host name | Must be unique among running servers. A server that finds its id heartbeating from another process waits up to three node timeouts for it to expire (a quick restart), then refuses to start. |
| `--advertise-addr` | `ORBIT_ADVERTISE_ADDR` | from `--addr` | Shown for this server in the `nodes` list of `/v1/health`. |

Give all servers the same credentials, workers file, TLS settings, `--session-timeout` and `--node-timeout`. A server registers, resumes the executions it owned before a restart, and then reports ready:

```console
INFO orbit_runtime::shared: cluster mode: shared (every server active) node_id=node-1 advertise_addr="http://127.0.0.1:36263" node_timeout_ms=3000
INFO orbit_runtime::server: ORBIT runtime listening addr=127.0.0.1:36263 tls=false
INFO orbit_runtime::shared: shared: node is serving node_id=node-1 resumed=0
```

### Ownership and fencing

The server that starts an execution owns it, and records that in `orbit_owners` with epoch `1` in the same transaction as `WorkflowCreated`. A child workflow is owned by its parent's owner and shares its root execution id, so a parent and its children always move together.

Only the owner appends to an execution's history. Every append checks, in its own transaction:

```sql
SELECT node_id, epoch FROM orbit_owners WHERE execution_id = $1 FOR SHARE
```

and aborts unless the row still names this server at the epoch it holds. This is the same pattern as the term check of standby mode, per execution. The row lock orders an append and a takeover: a takeover waits for an append in progress, and an append that starts after a takeover sees the new epoch. A server that lost an execution, because it was paused longer than the node timeout for example, cannot write to it, and its request fails without changing anything:

```text
STORAGE_ERROR: fenced: execution `f-1` is owned by node `node-new` at epoch 2; this node held epoch 1
```

The server then drops the execution from memory. When an execution reaches a terminal status, its ownership row is deleted in the same transaction.

Every server writes its heartbeat to `orbit_nodes` every second, with the database clock. A server whose heartbeat is older than `--node-timeout` is dead. Every second, each live server looks for executions owned by dead servers and takes over its share of them:

```sql
UPDATE orbit_owners SET node_id = $me, epoch = epoch + 1, since_ms = <now>
WHERE root_id = $root AND node_id = $dead RETURNING execution_id, epoch
```

The `WHERE node_id = $dead` makes the takeover single-winner: when two servers try at once, the second waits for the first, re-reads the row, and updates nothing. The share is chosen by rendezvous hashing of each root execution id over the live servers, so the executions of a dead server are spread evenly over the survivors without coordination; a server takes them in batches of 100 execution trees. It then rebuilds the executions from history, as a restarted server does: queued tasks go back on the queue, timers continue, and attempts in flight stay in flight while their workers are alive.

A server that stops gracefully (`SIGTERM`, Ctrl+C) sets its heartbeat to `0` on the way out, so the others take its executions over at once instead of after the node timeout:

```console
INFO orbit_runtime::shared: shared: left the cluster; other servers take over this server's executions
```

A server that restarts with the same `--node-id` increments the epoch of the executions it still owns, which fences the previous process, and resumes them.

### Task queue

When the owner schedules a worker task (`StepScheduled`, including retries, redeliveries, compensations and handler steps), it inserts a row into `orbit_tasks` in the same transaction and sends `NOTIFY orbit_tasks`. Built-in tasks (the HTTP adapter) and child workflows are not queued: the owner runs them itself.

A worker polls any server. That server claims the oldest row matching the worker's contracts and `using` language:

```sql
WITH c AS (SELECT id FROM orbit_tasks WHERE state = 'queued' AND (task_name, contract_version) IN (...)
           AND (language IS NULL OR language = 'any' OR language = $runtime)
           ORDER BY enqueued_ms, id LIMIT 1 FOR UPDATE SKIP LOCKED)
UPDATE orbit_tasks t SET state = 'claimed', token = ... FROM c WHERE t.id = c.id
```

`SKIP LOCKED` lets servers claim different rows at the same time without waiting for each other. In the same transaction, the claiming server checks the owner. If it owns the execution itself, it records `StepDispatched` right after the claim; if it dies in between, the server that takes the execution over puts the task back on the queue. Otherwise it writes a `dispatched` entry into the owner's inbox, so that the owner records `StepDispatched` with the worker id and session, and the claiming server returns the task to the worker at once. Task tokens have the form `<execution id>/<uuid>`, so any server can tell which execution a result belongs to.

Long polls wake up on `NOTIFY orbit_tasks`; every second they also look again, in case a notification was missed.

When the owner no longer needs an attempt (it timed out, the execution is being cancelled or failed, the step was redelivered), it marks the row `cancelled` in the same transaction. The next heartbeat of the worker's session, on whichever server, returns that token in its `cancel` list, once, as on a single server.

### Inbox

A command for an execution that another server owns is written to `orbit_inbox` and announced with `NOTIFY orbit_inbox` to the owner:

| Kind | Sent when | Applied by the owner as |
|---|---|---|
| `dispatched` | A server claimed a task of the execution. | `StepDispatched`, if the unit still waits for exactly that offer; otherwise ignored, and the worker is told to cancel. |
| `complete`, `fail` | A worker reported a result through a server that does not own the execution. | `POST /v1/tasks/complete` or `fail`, as on one server. |
| `uncertain` | A worker unregistered while holding a task. | `StepAttemptUncertain` and redelivery. |
| `signal`, `cancel` | A client sent a signal or a cancellation to a server that does not own the execution. | As on one server; the reply goes back to the waiting server. |

The owner applies its entries in `seq` order, through the same engine methods a request would use, and deletes each entry in the same transaction as the events it causes. Ordering matters for one case: a worker that is fast enough reports its result before the owner has recorded the dispatch. Both entries are in the inbox, the dispatch first, so the result finds its token.

A signal or cancellation that reaches the wrong server is validated there first: the execution must exist and be running, the signal must be declared and its payload valid. Errors such as `404 UNKNOWN_SIGNAL`, `409 EXECUTION_FINISHED` or `422 SIGNAL_PAYLOAD_INVALID` are answered at once, exactly as on one server. A valid one goes to the owner's inbox, and the receiving server waits up to 10 seconds for the owner's reply, so the response carries the signal's `seq` and the cancelled execution's state as usual.

Starting an execution needs no inbox: the server that receives the request owns the new execution. Reads (`GET` executions, history, lists, queries, `wait`) work on every server from the database; `wait` wakes on `NOTIFY orbit_done`.

### Worker sessions

Sessions are stored in `orbit_sessions`, so a session registered through one server works through all of them. A load balancer can send every request of a worker to a different server. When a server dies, its workers' polls fail, the SDK retries through the load balancer and reaches another server with the same session: no new registration, and the tasks the worker is running stay in flight. Their results reach the new owner through the inbox.

Servers write the sessions' last contact to the database every 250 ms, and owners read it for the sessions holding their attempts every 500 ms. A worker that goes silent is declared lost one `--session-timeout` after its last contact, plus about one second, and its attempts are redelivered with the same idempotency key.

### Failure handling

These logs come from the setup of the end-to-end test `SharedCluster`: three servers with `--session-timeout 3 --node-timeout 3`, a round-robin proxy that sends each new connection to the next server answering `200` on `/v1/ready`, and the Python and TypeScript example workers. `node-2` owns two orders whose charge takes 3 seconds; it is killed with `SIGKILL` while both charges run.

```console
# test driver
KILL node-2 at 2026-11-15T16:33:26.037Z

# Python worker: its polls through node-2 break; it retries through the proxy
2026-11-15 16:33:26,041 WARNING orbit poll failed: cannot reach ORBIT runtime at http://127.0.0.1:44679: Remote end closed connection without response; retrying in 0.2s

# node-3, one node timeout later
2026-11-15T16:33:29.189807Z  WARN orbit_runtime::shared: shared: taking over executions of a node that stopped heartbeating from=node-2 executions=2
2026-11-15T16:33:29.192739Z  INFO orbit_runtime::engine: shared: took ownership of executions executions=2 in_flight=2 from=node-2
2026-11-15T16:33:29.244878Z  INFO orbit_runtime::engine: execution completed execution_id=sh-D91
2026-11-15T16:33:29.260564Z  INFO orbit_runtime::engine: execution completed execution_id=sh-D90
```

The charges finished on the worker at about 16:33:29.0, while `node-2` was dead. The worker reported through the other servers, which stored the results in `node-2`'s inbox; `node-3` applied them right after the takeover. The charge's history shows one dispatch and one success:

```text
seq 12  16:33:25.919  StepScheduled   process_payment
seq 13  16:33:25.986  StepDispatched  process_payment  worker_session=3ed3dc41-ffb6-4453-a1fa-cfe24fce9f6f
seq 14  16:33:29.194  StepSucceeded   process_payment
```

When the worker dies together with the server, the new owner redelivers the attempt one session timeout after the worker's last contact (test `test_c_kill_a_node_and_a_worker_mid_run`; the Python worker was killed with the server and started again):

```console
# node-2
2026-11-15T16:26:03.423315Z  WARN orbit_runtime::shared: shared: taking over executions of a node that stopped heartbeating from=node-3 executions=1
2026-11-15T16:26:03.424848Z  INFO orbit_runtime::engine: shared: took ownership of executions executions=1 in_flight=1 from=node-3
2026-11-15T16:26:03.676284Z  WARN orbit_runtime::engine: attempt outcome uncertain; redelivering execution_id=sh-SH-C91 step=process_payment attempt=1 reason=worker `orders-python` stopped responding

# restarted Python worker
2026-11-15 16:26:03,690 INFO orbit executing {"execution_id": "sh-SH-C91", "step": "process_payment", "attempt": 1, "task": "payments.charge", "redelivery": true}
```

| Time after the kill | What happens |
|---|---|
| 0 s | The server's connections close. Workers' polls and result reports through it fail, and the SDKs retry; the proxy sends the retries to the other servers. The workers' sessions are in the database, so the other servers accept them. |
| until `--node-timeout` | The dead server's executions wait: nobody ticks them or records their results. Results reported meanwhile are stored in its inbox. Executions owned by the other servers continue. |
| `--node-timeout` | The other servers see the stale heartbeat, take over the dead server's executions (spread by rendezvous hashing), rebuild them from history and apply the waiting inbox entries. |
| next result | Results of attempts that were in flight are accepted by the new owner: their tokens are in the history it rebuilt. |
| `--node-timeout` + `--session-timeout` | An attempt the dead server handed out is redelivered if its result has not arrived, because the worker may never have received it. A late result of the first attempt is still accepted. |
| one `--session-timeout` after a worker's last contact | Attempts of workers that died too are uncertain and redelivered (`StepAttemptUncertain`, `redelivery: true`). |

| Failure | What happens |
|---|---|
| A server crashes or is killed | As above: about `--node-timeout` until its executions continue elsewhere. Committed events are kept; an uncommitted request is lost and the client gets a connection error; retry it (with an `execution_id` for starts). |
| A server stops gracefully | Its executions are taken over within about a second. |
| A server pauses (stop-the-world, frozen VM) longer than `--node-timeout` | Its executions are taken over. When it resumes, its appends fail with `fenced:`; it drops those executions from memory and continues with the ones it still owns. |
| A server loses the database | It cannot heartbeat; after `--node-timeout` its `/v1/ready` answers `503` and the others take over. When it reconnects, it compares its executions with `orbit_owners` and drops those it lost. |
| A worker dies | Its attempts are redelivered after the session timeout, by their owners, as on one server. |
| PostgreSQL fails over | All servers reconnect to the new primary and continue. |

### Load balancer configuration

Every live server accepts every request, so balance over all of them and health-check `GET /v1/ready`: it answers `200` on a live server and `503` while a server resumes its executions at startup, cannot write its heartbeat, or shares its node id with another running server. Keep `/v1/health` for liveness.

Sessions are shared, so no stickiness is needed. Long polls and waits last up to 60 and 300 seconds; set the backend timeout above that.

HAProxy:

```text
backend orbit
    mode http
    balance roundrobin
    option httpchk GET /v1/ready
    http-check expect status 200
    default-server inter 1s fall 2 rise 1
    retry-on conn-failure
    server orbit-0 orbit-0.orbit.internal:7233 check
    server orbit-1 orbit-1.orbit.internal:7233 check
    server orbit-2 orbit-2.orbit.internal:7233 check
    timeout server 330s
```

nginx:

```nginx
upstream orbit {
    server orbit-0.orbit.internal:7233 max_fails=1 fail_timeout=2s;
    server orbit-1.orbit.internal:7233 max_fails=1 fail_timeout=2s;
    server orbit-2.orbit.internal:7233 max_fails=1 fail_timeout=2s;
}
server {
    listen 7233;
    location / {
        proxy_pass http://orbit;
        proxy_next_upstream error timeout;
        proxy_read_timeout 330s;
    }
}
```

Do not add `non_idempotent` to `proxy_next_upstream` in shared mode: a request that reached a server that died may have been applied.

On Kubernetes, use a `Deployment` or `StatefulSet` with several replicas, `--node-id` from the pod name (`ORBIT_NODE_ID` from `metadata.name`), `ORBIT_CLUSTER_MODE=shared`, and the readiness probe on `/v1/ready` from [Kubernetes](#kubernetes). The `Service` balances over all ready pods.

### Health fields

```console
$ curl -s http://127.0.0.1:46825/v1/health
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.3.0","ir_versions":["orbit-ir/1","orbit-ir/2"],"active_executions":0,"worker_sessions":2,"role":"shared","node_id":"node-1","term":null,"leader":null,"owned_executions":0,"tasks_claimed":18,"nodes":[{"node_id":"node-1","addr":"http://127.0.0.1:46825","live":true,"heartbeat_age_ms":2,"owned_executions":0},{"node_id":"node-3","addr":"http://127.0.0.1:47377","live":true,"heartbeat_age_ms":248,"owned_executions":0}]}
$ curl -s http://127.0.0.1:46825/v1/ready
{"role":"shared","node_id":"node-1","term":null,"leader":null,"owned_executions":0,"tasks_claimed":0,"nodes":[{"node_id":"node-1","addr":"http://127.0.0.1:46825","live":true,"heartbeat_age_ms":1,"owned_executions":0},{"node_id":"node-2","addr":"http://127.0.0.1:36959","live":true,"heartbeat_age_ms":709,"owned_executions":0},{"node_id":"node-3","addr":"http://127.0.0.1:47377","live":true,"heartbeat_age_ms":274,"owned_executions":0}],"ready":true}
```

The first response was taken after `node-2` was killed: it is no longer listed, because it is not live and owns nothing.

| Field | Meaning |
|---|---|
| `role` | `shared`. |
| `node_id` | This server's `--node-id`. |
| `term`, `leader` | Always `null` in shared mode. |
| `active_executions`, `owned_executions` | Running executions this server owns. |
| `tasks_claimed` | Tasks this server handed to workers since it started. |
| `nodes` | Every server that is live or still owns executions, with `addr`, `live`, `heartbeat_age_ms` and `owned_executions`. |

Metrics (`/v1/metrics`) stay per server: each counts the events it committed and the executions it owns. Sum them across servers in your monitoring.

### Throughput

These numbers were measured on the machine this page was written on, so read them as the shape of the scaling, not as capacity: 4 vCPUs, PostgreSQL 16 on the same machine with default settings (`synchronous_commit = on`), a release build, and every server, worker and client sharing those 4 CPUs and one disk. The workload is the end-to-end setup: `OrderProcessing` (six worker tasks per order) with the Python and TypeScript example workers (concurrency 8 each), 600 orders started by 64 concurrent clients that each wait for their order to complete, and shared servers behind the round-robin proxy.

| Servers | Worker processes | Orders per second |
|---|---|---|
| 1, single server (no `--cluster`) | 1 Python, 1 TypeScript | 37.2, 38.2 |
| 1, single server (no `--cluster`) | 2 Python, 2 TypeScript | 32.6 |
| 1, shared mode | 1 Python, 1 TypeScript | 28.4, 29.9 |
| 2, shared mode | 1 Python, 1 TypeScript | 37.3 |
| 3, shared mode | 1 Python, 1 TypeScript | 42.6, 41.4 |
| 3, shared mode | 2 Python, 2 TypeScript | 42.6 |

Two runs of the same configuration are listed where there were two. What they show:

- One shared server is about 25% slower than a single server: every task also writes a queue row, a claim and an ownership check.
- Throughput grows with the number of shared servers, by about 30% for the second and 10% for the third here, where all of them compete for the same CPUs and the same PostgreSQL. Three shared servers did about 10% more than one single server.
- More worker processes did not help: the servers and PostgreSQL were the limit, not the workers.

On separate machines the servers do not compete for CPU, and PostgreSQL's commit rate becomes the limit: each task commits its dispatch and its result, as on one server, plus the claim.

### Limits

- **PostgreSQL only, and PostgreSQL is the bottleneck.** Every dispatch, result and heartbeat is a database round trip, as on one server, plus the queue and inbox writes. Adding servers adds CPU for the engines and connections for workers; it does not add database capacity.
- **Failover of an execution waits for `--node-timeout`.** A lower value detects dead servers faster but turns long pauses into takeovers; each server must be able to write its heartbeat within it.
- **A server that dies can cause redeliveries.** Attempts it handed out whose results do not arrive within `--node-timeout` plus `--session-timeout` are redelivered, even if the worker is still running them. Idempotency keys make this safe, as for any redelivery.
- **Clocks.** Liveness uses the database clock, but timers, retries and timeouts use the owner's clock. Keep server clocks synchronized.
- **Limits are per server.** `--max-active-executions` limits the executions each server owns.
- **One owner per execution tree.** An execution and its child workflows run on one server. A single very large execution does not spread.
- **Node ids must be unique.** Two running processes with the same `--node-id` are refused or stop serving; they would otherwise take each other's executions.
- **No mixed modes.** Do not run `standby` and `shared` servers, or servers without `--cluster`, on the same database.
- **Version skew.** During a rolling upgrade, servers of both versions run at once. Upgrade one server at a time, and do not roll back to a version without shared mode while executions are owned in `orbit_owners`.

## See also

- [Running the server](server.md)
- [Storage](storage.md#cluster-table)
- [Storage: shared-mode tables](storage.md#shared-mode-tables)
- [Deployment](deployment.md#kubernetes-with-two-replicas)
- [Delivery guarantees](guarantees.md)
- [HTTP API: GET /v1/ready](../reference/http-api.md#get-v1ready)
- [CLI: orbit server](../reference/cli.md#orbit-server)
