# Storage

The runtime keeps workflow definitions, execution history and the worker registry in one database: a SQLite file by default, or PostgreSQL. Both backends use the same schema and give the same guarantees. This page explains how to choose and configure a backend, how TLS to PostgreSQL works, what the schema contains, how to back it up, and why one database belongs to one server.

## Choosing a backend

You select the backend with `--db` (or `ORBIT_DB`). A value that starts with `postgres://` or `postgresql://` selects PostgreSQL; anything else is a SQLite file path.

| | SQLite | PostgreSQL |
|---|---|---|
| `--db` | a file path, default `orbit.db` | `postgres://user:pass@host:5432/db` |
| Setup | none; the file is created on first start | an existing database and a user that can create tables |
| Durability | WAL mode, `synchronous=FULL` | one transaction per batch of events |
| Connection | in-process | one connection on a dedicated thread, reconnected automatically |
| Good for | development, single hosts, moderate volume | managed databases, existing backup and replication tooling, larger histories |

Both are single-writer from ORBIT's point of view: the runtime serializes all state changes through one engine. PostgreSQL does not make the runtime horizontally scalable; see [Single node](#single-node).

## SQLite

```bash
orbit server --db /var/lib/orbit/orbit.db
```

- The file and its tables are created on first start. The directory must exist and be writable.
- The database runs in WAL mode, so you will see `orbit.db-wal` and `orbit.db-shm` next to `orbit.db`. Keep the three files together.
- `synchronous=FULL` makes every commit durable before it is acknowledged, at the cost of an `fsync` per batch. Put the file on a local disk with reliable `fsync`; network file systems are not supported by SQLite's locking.
- A relative path is resolved against the server's working directory. In the container image the default is `/data/orbit.db`.

## PostgreSQL

```bash
orbit server --db 'postgres://orbit:secret@db.internal:5432/orbit?sslmode=require'
```

- The URL follows the libpq connection URI format: `postgres://user:password@host:port/database?param=value`. Percent-encode special characters in the password.
- The schema is created on first start with `CREATE TABLE IF NOT EXISTS`, so the user needs `CREATE` on the database's default schema. Use a dedicated database or schema for ORBIT.
- All statements run on one dedicated connection thread. If the connection drops, it is re-established before the next statement (`reconnected to PostgreSQL` in the log). A statement issued while the database is unreachable fails with `STORAGE_ERROR`, and the engine keeps its previous in-memory state.
- The password is redacted in logs and error messages: `postgres://orbit:***@db.internal:5432/orbit?sslmode=require`.

### TLS

TLS follows the URL's `sslmode` parameter:

| `sslmode` | Behavior |
|---|---|
| `disable` | Never use TLS. |
| `prefer` (default) | Use TLS when the server offers it. |
| `require` | Insist on TLS; fail if the server does not offer it. |

When TLS is used, the server certificate is always verified, against the public web PKI roots plus every certificate in the PEM bundle named by `ORBIT_DB_CA_CERT`:

```bash
ORBIT_DB_CA_CERT=/etc/orbit/rds-ca-bundle.pem \
orbit server --db 'postgres://orbit:secret@mydb.example.rds.amazonaws.com:5432/orbit?sslmode=require'
```

Unlike libpq's `prefer`, ORBIT never accepts a certificate it cannot verify. For a local server with a self-signed certificate, either provide its CA through `ORBIT_DB_CA_CERT` or use `sslmode=disable`. For production, use `sslmode=require`.

## Schema

The schema is the same on both backends (`BIGINT` columns are `INTEGER` in SQLite). Schema version 1:

| Table | Key | Contents |
|---|---|---|
| `schema_version` | | One row: the schema version (`1`). |
| `workflow_definitions` | `(name, version)` | `definition_hash`, the full IR as JSON in `ir`, and `created_at_ms`. Rows are never updated. |
| `workflow_executions` | `execution_id` | Projection for listing: `workflow`, `version`, `definition_hash`, `status`, `created_at_ms`, `updated_at_ms`, `last_seq`. Indexed by `status`. |
| `execution_events` | `(execution_id, seq)` | The authoritative history of each execution's current run: `event_type`, `step` (or null), `payload` (the event as JSON), `timestamp_ms`. Append-only, except that a run's events move to `execution_run_events` when it ends with `continue_as_new`. |
| `execution_run_events` | `(execution_id, run, seq)` | The histories of runs that ended with [`continue_as_new`](../language/continue-as-new.md), with the same columns as `execution_events` plus `run`. Rows are never updated. |
| `orbit_schedules` | `schedule_id` | [Schedules](schedules.md): see [Schedule table](#schedule-table). |
| `worker_registry` | `worker_id` | Latest registration of each worker: `runtime`, `sdk`, `contracts` (JSON), `registered_at_ms`, `last_seen_ms`. |
| `orbit_cluster` | `id` | PostgreSQL, cluster mode only. See [Cluster table](#cluster-table). |
| `orbit_nodes`, `orbit_owners`, `orbit_tasks`, `orbit_inbox`, `orbit_sessions` | | PostgreSQL, shared mode only. See [Shared-mode tables](#shared-mode-tables). |

Each batch of events and the matching `workflow_executions` update are written in one transaction. So is a continue-as-new: the finished run's last events, the move of its history to `execution_run_events`, the next run's first events and the projection update commit together. Worker sessions, tokens in flight and metrics are kept in memory only; sessions and tokens are rebuilt from the history on startup, and metrics start from zero. Shared mode is the exception: it keeps worker sessions and the task queue in the shared-mode tables, so that every server sees them.

The tables are an implementation detail. Read execution state through the [HTTP API](../reference/http-api.md), which applies the same fold the engine uses. Do not write to the tables: a modified history no longer matches what the engine decided.

The tables are created with `CREATE TABLE IF NOT EXISTS` on every start, so a database created by an earlier version gains `execution_run_events` and `orbit_schedules` on the first start of this version, without a migration step and without changing existing rows. The schema version stays `1`.

### Schedule table

`orbit_schedules` holds one row per schedule. Every server that can write reads it about once a second for due occurrences; see [Schedules: how firing works](schedules.md#how-firing-works).

| Column | Type | Meaning |
|---|---|---|
| `schedule_id` | `TEXT`, primary key | The schedule id. |
| `spec` | `TEXT` | The specification as JSON: workflow, version, input, cron or interval, time zone, policies, start and end, jitter, description. |
| `paused` | `BIGINT` | `1` while paused. |
| `next_fire_ms` | `BIGINT` | The next occurrence (nominal time), or null when the schedule has ended. Claims and advances compare it (compare-and-set). |
| `due_ms` | `BIGINT` | When that occurrence fires: `next_fire_ms` plus its jitter. Indexed. |
| `last_fire_ms` | `BIGINT` | The latest occurrence handled. |
| `last_execution_id` | `TEXT` | The latest execution started, including triggers. |
| `fired_count`, `skipped_count` | `BIGINT` | Counters shown as `fired` and `skipped`. |
| `last_error` | `TEXT` | Why the latest start failed, if it did. |
| `claimed_by` | `TEXT` | The engine instance that holds a claim on `next_fire_ms`, or null. |
| `claimed_until_ms` | `BIGINT` | When that claim expires (30 seconds after it was taken), or when a failed start is retried. |
| `created_at_ms`, `updated_at_ms` | `BIGINT` | Creation and last change. |

Times are milliseconds since the Unix epoch, from the clock of the server that wrote them.

### Cluster table

Servers started with `--cluster` create one more table on first start, idempotently, under an advisory lock. Single-node servers neither create nor read it, and the schema version stays `1`.

| Column | Type | Meaning |
|---|---|---|
| `id` | `INT`, primary key | Always `1`: the table has one row. |
| `term` | `BIGINT` | Leadership term, incremented by every newly elected leader. Every history append of the leader checks it in the same transaction (fencing). |
| `leader_node` | `TEXT` | `--node-id` of the latest leader. |
| `leader_addr` | `TEXT` | `--advertise-addr` of the latest leader, or null. |
| `since_ms` | `BIGINT` | When the latest leader was elected. |

The leader also holds a session-level advisory lock on its own election connection (`application_name` `orbit-cluster:<node id>`). See [Running several servers](cluster.md).

### Shared-mode tables

Servers started with `--cluster --cluster-mode shared` create five more tables on first start, idempotently, under an advisory lock; they also create the base schema under an advisory lock, because shared-mode servers usually start together. Other servers neither create nor read these tables, and the schema version stays `1`. Times ending in `_ms` are milliseconds since the Unix epoch; `heartbeat_ms` and `last_seen_ms` use the database clock, the others the clock of the server that wrote them.

`orbit_nodes`: one row per server.

| Column | Type | Meaning |
|---|---|---|
| `node_id` | `TEXT`, primary key | `--node-id`. |
| `incarnation` | `TEXT` | Random id of the running process; a restarted server gets a new one. |
| `addr` | `TEXT` | `--advertise-addr`, or null. |
| `heartbeat_ms` | `BIGINT` | Last heartbeat (every second). `0` after a graceful shutdown. |
| `started_ms` | `BIGINT` | When the process registered. |

`orbit_owners`: one row per running execution.

| Column | Type | Meaning |
|---|---|---|
| `execution_id` | `TEXT`, primary key | The execution. |
| `root_id` | `TEXT` | The top-level execution of its tree: child workflows share their root's owner and move with it. |
| `node_id` | `TEXT` | The owner. Indexed. |
| `epoch` | `BIGINT` | `1` when the execution starts, incremented by every takeover. Every history append checks it in the same transaction (fencing). |
| `since_ms` | `BIGINT` | When the current owner got it. |

The row is deleted, in the same transaction, when the execution reaches a terminal status.

`orbit_tasks`: the shared task queue, one row per offer of a worker task.

| Column | Type | Meaning |
|---|---|---|
| `id` | `BIGSERIAL`, primary key | |
| `execution_id`, `unit_key`, `attempt`, `offer` | | The unit and which offer of it this is (`offer` is the unit's dispatch count when it was scheduled). Unique together. |
| `task_name`, `contract_version`, `language` | `TEXT` | What a worker must announce to get it (`language` from `using`, lowercase, or null). |
| `payload` | `TEXT` | The task assignment as JSON, without token and deadline. |
| `enqueued_ms` | `BIGINT` | When the unit was scheduled. Workers get the oldest matching row first. |
| `state` | `TEXT` | `queued`, `claimed`, `done` (the result arrived), `cancelled` (the owner no longer needs it) or `cancel_sent` (the worker was told). |
| `token`, `claimed_by`, `claimed_inc`, `claimed_session`, `claimed_worker`, `claimed_ms` | | The claim: token handed out, server and process that handed it out, worker session and worker. Indexed by `token` and `claimed_session`. |
| `updated_ms` | `BIGINT` | Last change. |

`orbit_inbox`: commands for an execution's owner, applied in `seq` order.

| Column | Type | Meaning |
|---|---|---|
| `seq` | `BIGSERIAL`, primary key | Order of arrival. |
| `execution_id` | `TEXT` | The execution. Indexed with `seq`. |
| `kind` | `TEXT` | `dispatched`, `complete`, `fail`, `uncertain`, `signal` or `cancel`. |
| `token` | `TEXT` | The task token, for task-related kinds. |
| `payload` | `TEXT` | The command as JSON. |
| `created_ms` | `BIGINT` | When it was written. |
| `done_ms`, `result` | | For `signal` and `cancel`: when the owner applied it and its reply, read by the server that is waiting to answer the client. |

The owner deletes an entry (or, for `signal` and `cancel`, marks it done) in the same transaction as the events it caused.

`orbit_sessions`: worker sessions, usable through any server.

| Column | Type | Meaning |
|---|---|---|
| `session_id` | `TEXT`, primary key | The `worker_session_id` returned by registration. |
| `worker_id`, `runtime`, `contracts` | `TEXT` | From the registration (`contracts` as JSON). Indexed by `worker_id`. |
| `node_id` | `TEXT` | The server that registered it. |
| `registered_ms` | `BIGINT` | When it registered. |
| `last_seen_ms` | `BIGINT` | Last poll, heartbeat or result, written by the servers in batches every 250 ms. |

Every server deletes, every 30 seconds: sessions silent for four session timeouts, finished queue rows and inbox entries of executions that have no owner any more (after one minute, or four session timeouts if longer), and rows of servers that have been gone for an hour and own nothing.

## Retention

The runtime never deletes history. Finished executions stay queryable through `orbit status`, `orbit logs` and the API indefinitely, and the database grows with the number of events. Plan disk space accordingly; a typical step produces three to five events. There is no built-in archival or pruning in this version. Runs that ended with `continue_as_new` are kept in `execution_run_events`: continue-as-new bounds what one run replays and holds in memory, not what the database stores.

## Backups

### SQLite

Copy a consistent snapshot with the SQLite online backup, which is safe while the server runs:

```bash
sqlite3 /var/lib/orbit/orbit.db ".backup '/backups/orbit-$(date +%F).db'"
```

Alternatively, stop the server and copy `orbit.db`, `orbit.db-wal` and `orbit.db-shm` together. Do not copy only `orbit.db` while the server is running: recent commits may still be in the WAL file.

### PostgreSQL

Use your usual tooling: `pg_dump`, continuous archiving with point-in-time recovery, or your provider's snapshots.

```bash
pg_dump --format=custom --file=/backups/orbit.dump 'postgres://orbit:secret@db.internal:5432/orbit'
```

### Restoring

Stop the server, restore the database, and start the server. It recovers every non-terminal execution from the restored history. Work that happened after the backup is lost from ORBIT's point of view: executions started later do not exist, and steps that completed later run again. Idempotency keys protect external systems from duplicate effects in this case too.

## Single node

Without `--cluster`, one database belongs to one running server. The engine keeps the active executions, the worker sessions and the in-flight tokens in memory and assumes it is the only writer. Two servers on the same database would each dispatch the same tasks and append conflicting events. Nothing in the database prevents this, so make sure your deployment cannot start a second instance: one systemd unit, one container with a restart policy, or a Kubernetes `Deployment` with `replicas: 1` and the `Recreate` strategy.

For availability, rely on fast restarts and recovery, or, with PostgreSQL, run several servers in cluster mode: they elect one leader, which is the only one that writes, and fence a deposed leader so it cannot write. In shared mode every server writes, but each execution has one owner, and every append is fenced by the execution's ownership epoch. See [Running several servers](cluster.md). You can also use the database's own replication and failover; servers reconnect to the new primary.

## See also

- [Running the server](server.md)
- [Running several servers](cluster.md)
- [Delivery guarantees](guarantees.md)
- [Deployment](deployment.md)
- [Security](security.md)
- [Environment variables](../reference/environment.md)
