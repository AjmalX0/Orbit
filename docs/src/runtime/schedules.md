# Schedules

A schedule starts executions of a workflow at times given by a cron
expression or a fixed interval: a nightly report, a reconciliation every 15
minutes, a weekly cleanup. Schedules are stored in the database next to the
executions, so they survive restarts and failovers, and every server of a
cluster sees the same ones. This page covers creating and managing schedules,
how their times are computed, the execution ids they produce, the overlap and
catch-up policies, and how firing works on one server and on several.

## Creating a schedule

With the CLI ([`orbit schedule`](../reference/cli.md#orbit-schedule)):

```console
$ orbit schedule create nightly-orders --workflow OrderProcessing --cron "0 2 * * *" --timezone Europe/Berlin --input '{"order_id": "N-1", "customer_id": "cust-1", "amount": 120.0, "items": [{"sku": "BOOK-1", "quantity": 1}]}' --description "Nightly reconciliation order"
active nightly-orders  cron "0 2 * * *" Europe/Berlin
  workflow:   OrderProcessing (latest version)
  input:      {"order_id":"N-1","customer_id":"cust-1","amount":120.0,"items":[{"sku":"BOOK-1","quantity":1}]}
  overlap:    skip   catchup: skip_missed
  description: Nightly reconciliation order
  fired:      0   skipped: 0
  last fire:  -
  last run:   -
  next fires:
    2026-11-16T01:00:00Z
    2026-11-17T01:00:00Z
    2026-11-18T01:00:00Z
    2026-11-19T01:00:00Z
    2026-11-20T01:00:00Z
```

Or with the HTTP API ([`POST /v1/schedules`](../reference/http-api.md#post-v1schedules)):

```console
$ curl -s -X POST localhost:7233/v1/schedules -H 'content-type: application/json' -d '{"schedule_id": "orders-every-10s", "workflow": "OrderProcessing", "interval": "10s", "input": {"order_id": "R-1", "customer_id": "cust-1", "amount": 120.0, "items": [{"sku": "BOOK-1", "quantity": 1}]}}'
{"schedule_id":"orders-every-10s","workflow":"OrderProcessing","input":{"order_id":"R-1","customer_id":"cust-1","amount":120.0,"items":[{"sku":"BOOK-1","quantity":1}]},"interval":"10s","overlap":"skip","catchup":"skip_missed","paused":false,"next_fire_at":"2026-11-15T18:51:10Z","next_fire_times":["2026-11-15T18:51:10Z","2026-11-15T18:51:20Z","2026-11-15T18:51:30Z","2026-11-15T18:51:40Z","2026-11-15T18:51:50Z"],"last_fire_at":null,"last_execution_id":null,"fired":0,"skipped":0,"last_error":null,"created_at":"2026-11-15T18:51:00.347Z","updated_at":"2026-11-15T18:51:00.347Z"}
```

The workflow must be registered, and the input is validated against its
`input` type (defaults applied) when the schedule is created or changed, and
again at each start. Without `version`, each start uses the latest registered
version at that moment, so deploying a new version changes what the schedule
starts; give `version` to pin one. The first occurrence is the first one after
the schedule was created.

A schedule has these settings:

| Setting | Default | Meaning |
|---|---|---|
| `workflow`, `version`, `input` | latest version, `{}` | What to start. |
| `cron` or `interval` | required, exactly one | When to start it. |
| `timezone` | UTC | The time zone of a cron expression. |
| `overlap` | `skip` | What to do when the previous execution is still running. |
| `catchup` | `skip_missed` | What to do with occurrences missed while no server could fire them. |
| `start_at`, `end_at` | none | No occurrence before `start_at` or after `end_at`. |
| `jitter` | none | Delay each fire by a deterministic amount below this duration. |
| `paused` | `false` | A paused schedule does not fire. |
| `description` | none | Free text. |

## Timing

### Cron expressions

A cron expression has five fields, separated by spaces:

| Field | Values | Names |
|---|---|---|
| minute | `0`-`59` | |
| hour | `0`-`23` | |
| day of month | `1`-`31` | |
| month | `1`-`12` | `JAN`-`DEC` |
| day of week | `0`-`7` (`0` and `7` are Sunday) | `SUN`-`SAT` |

Each field is `*`, a value, a range `a-b`, a step `*/n`, `a-b/n` or `a/n`
(from `a` to the end of the range, every `n`), or a comma-separated list of
these. Names are case-insensitive. As in Vixie cron, when both the day of
month and the day of week are restricted (neither starts with `*`), a day
matches if either matches: `0 0 13 * 5` runs on the 13th and on every Friday.
`@hourly`, `@daily` (or `@midnight`), `@weekly`, `@monthly` and `@yearly` (or
`@annually`) stand for `0 * * * *`, `0 0 * * *`, `0 0 * * 0`, `0 0 1 * *` and
`0 0 1 1 *`. Seconds and the Quartz extensions (`L`, `W`, `#`, `?`) are not
supported.

| Expression | Fires |
|---|---|
| `0 2 * * *` | Every day at 02:00. |
| `*/15 * * * *` | Every 15 minutes, on the quarter hour. |
| `30 9 * * MON-FRI` | At 09:30 on weekdays. |
| `0 0 1 JAN,JUL *` | At midnight on 1 January and 1 July. |
| `5/20 * * * *` | At minutes 5, 25 and 45 of every hour. |
| `0 0 29 2 *` | At midnight on 29 February. |

An invalid expression is rejected with `400 INVALID_SCHEDULE`, for example
``cron `0 25 * * *`: `25` is out of range for the hour field (0-23)``.

### Time zones

A cron expression is evaluated in UTC unless the schedule has an IANA
`timezone` such as `Europe/Berlin` or `America/New_York`. The time-zone
database is compiled into the `orbit` binary (it does not depend on the
host's `/usr/share/zoneinfo`), so every server computes the same times;
upgrading ORBIT updates it. Times in the API are always shown in UTC: the
`nightly-orders` schedule above fires at 02:00 in Berlin, which is
`01:00:00Z` in winter and `00:00:00Z` in summer.

Around daylight-saving changes:

- A local time that does not exist (02:30 on the night the clocks go forward)
  fires once, at the corresponding time after the change (03:30 local).
- A local time that occurs twice (02:30 on the night the clocks go back)
  fires once, at its first occurrence.

### Intervals

`interval` is a duration: whole numbers with the units `ms`, `s`, `m`, `h`
and `d`, which may be combined (`1h30m`). The minimum is `1s`. Occurrences
are aligned to `start_at` when it is set, and to the Unix epoch otherwise:
`every 15m` fires at :00, :15, :30 and :45 of every hour, whenever the
schedule was created. Intervals are absolute and do not take a `timezone`.

### Start, end and jitter

`start_at` and `end_at` are RFC 3339 times. There is no occurrence before
`start_at` (an interval schedule's first occurrence is `start_at` itself) or
after `end_at`; after the last one, the schedule is listed as `ended`, with
`next_fire_at: null`, and stays until you delete it.

`jitter` delays each fire by an amount between zero and the jitter, derived
from a hash of the schedule id and the occurrence, so that many schedules on
the same minute do not all start at once. Every server computes the same
delay. `next_fire_at` includes it; `next_fire_times` and the execution id use
the nominal time:

```console
$ orbit schedule list
SCHEDULE                 WORKFLOW             TIMING                           STATE    NEXT FIRE              LAST EXECUTION
hourly-check             OrderProcessing      every 1h                         active   2026-11-15T19:00:12.359Z -
nightly-orders           OrderProcessing      cron "0 2 * * *" Europe/Berlin   active   2026-11-16T01:00:00Z   -
```

(`hourly-check` has `--jitter 30s`.)

## Execution ids

Each occurrence starts an execution with the id
`<schedule id>:<occurrence in RFC 3339, UTC>`:

```console
$ orbit ps --workflow OrderProcessing
EXECUTION                              WORKFLOW                 VERSION    STATUS        AGE
orders-every-10s:2026-11-15T18:51:30Z  OrderProcessing          1.0.0      COMPLETED     259ms
orders-every-10s:2026-11-15T18:51:20Z  OrderProcessing          1.0.0      COMPLETED     10.3s
orders-every-10s:2026-11-15T18:51:10Z  OrderProcessing          1.0.0      COMPLETED     20.3s
```

Starting an execution with an id that exists is a no-op
([Starting an execution](executions.md#starting-an-execution)), so an
occurrence is never started twice, whatever happens to the servers. The
execution records the schedule in `schedule_id` (and in its `WorkflowCreated`
event); `orbit status` shows it:

```console
$ orbit status nightly-orders:manual:2026-11-15T18:52:22.296Z
COMPLETED nightly-orders:manual:2026-11-15T18:52:22.296Z  OrderProcessing@1.0.0
  schedule: nightly-orders
  elapsed: 33ms
```

A manual [trigger](#pause-resume-trigger-change-delete) uses
`<schedule id>:manual:<now, with milliseconds>`. Schedule ids are therefore
limited to letters, digits, `-`, `_` and `.`, and to 100 characters.

## Overlap policy

`overlap` decides what happens when an occurrence is due while the execution
the schedule started last is still running (`RUNNING` or `COMPENSATING`):

| Policy | Behavior |
|---|---|
| `skip` (default) | Start nothing; the occurrence counts as skipped. The next occurrence is checked again. |
| `allow` | Start anyway. Executions of the schedule may run at the same time. |
| `cancel_previous` | Cancel the previous execution (with the reason ``schedule `<id>`: replaced by the next occurrence (overlap cancel_previous)``; its compensation and `on cancel` handler run as for any cancellation) and start the new one at once, without waiting for the previous one to finish. |

The policy looks only at the latest execution the schedule started
(`last_execution_id`), including a manual trigger. With `allow`, earlier
executions may still be running too. An execution that
[continues as new](../language/continue-as-new.md) counts as running until
its final run ends.

## Catch-up after downtime

Occurrences are fired by running servers. When no server could fire for a
while (all servers were down, or the database was unreachable), the next
server to look finds one or more occurrences in the past. `catchup` decides
what happens to them:

| Policy | Behavior |
|---|---|
| `skip_missed` (default) | Skip them. Only an occurrence at most 60 seconds late is still started (the most recent one, if several are that recent); everything older counts as skipped. |
| `run_once` | Start one execution for the most recent missed occurrence, however late; the older ones count as skipped. |

Either way, firing then continues with the first occurrence in the future,
and the schedule's `skipped` count and `orbit_schedules_skipped_total{reason="missed"}`
count the skipped occurrences. A `run_once` start uses the id of the
occurrence it stands for, for example `s:2026-11-15T00:30:00Z` for a 10-minute
schedule whose server was down from 00:00 to 00:35.

Pausing is not downtime: a resumed schedule continues with its next
occurrence after now, and starts nothing for the occurrences that passed
while it was paused.

## Pause, resume, trigger, change, delete

| Action | CLI | API |
|---|---|---|
| Pause | `orbit schedule pause <id>` | `POST /v1/schedules/{id}/pause` |
| Resume | `orbit schedule resume <id>` | `POST /v1/schedules/{id}/resume` |
| Start an execution now | `orbit schedule trigger <id>` | `POST /v1/schedules/{id}/trigger` |
| Change timing, input or policies | `orbit schedule update <id> ...` | `PATCH /v1/schedules/{id}` |
| Delete | `orbit schedule delete <id>` | `DELETE /v1/schedules/{id}` |

A trigger starts an execution with the schedule's workflow, version and
input, also while the schedule is paused, and applies the overlap policy (with
`skip` it starts nothing while the previous execution runs). It does not change
the schedule's timing.

```console
$ orbit schedule trigger nightly-orders
started execution nightly-orders:manual:2026-11-15T18:52:22.296Z
$ orbit schedule trigger nightly-orders
not started: the previous execution of nightly-orders is still running (overlap: skip)
```

The [web UI](web-ui.md#schedules) lists schedules and can create, pause, resume, trigger and delete them.

A change recomputes the next occurrence from now and keeps the counters and
the last execution. Deleting a schedule does not affect the executions it
started.

## How firing works

Every server that can write (a single server, a cluster leader, every server
in shared mode) looks for due schedules from its tick loop, about once a
second, and immediately after a schedule changed on it. A schedule therefore
fires up to about a second after its time (plus its jitter). For each due
occurrence the server:

1. **Claims it** with a compare-and-set on the schedule's row:
   `UPDATE orbit_schedules SET claimed_by = <this server>, claimed_until_ms = <now + 30 s> WHERE schedule_id = ... AND next_fire_ms = <the occurrence> AND paused = 0 AND <no live claim>`.
   Of several servers that see the same occurrence, one update matches.
2. **Decides** with the catch-up and overlap policies, and starts the
   execution (or cancels the previous one first, with `cancel_previous`).
3. **Advances** the schedule with a second compare-and-set: `next_fire_ms`
   moves to the next occurrence, the counters and `last_execution_id` are
   updated and the claim is released, but only if the row still holds this
   server's claim on this occurrence. If the schedule was changed, paused or
   deleted meanwhile, the change wins and the server logs it.

If the start fails (for example `429 RESOURCE_EXHAUSTED` under
`--max-active-executions`, or the workflow was not found), the schedule is not
advanced: the claim is released with a retry time 5 seconds later, the error
is shown in `last_error`, and the same occurrence is tried again. An
occurrence that stays blocked for more than 60 seconds is subject to the
catch-up policy like any other late occurrence.

If a server dies after claiming an occurrence and before advancing it, the
claim blocks the occurrence for at most 30 seconds. A single server or a new
cluster leader drops all claims when it starts, because they can only come
from a process that is gone; in shared mode, the claim expires and another
server takes the occurrence. In both cases the deterministic execution id
makes the repeated start a no-op if the execution was already created.

The [`Schedules`](https://github.com/AjmalX0/Orbit/blob/main/tests/e2e/test_e2e.py)
end-to-end test kills a server with `SIGKILL` while a 1-second schedule runs,
restarts it, and checks that every occurrence between the first and the last
execution was either started exactly once or counted as skipped.

On several servers, see [Running several servers](cluster.md#schedules)
(active/standby: only the leader fires) and
[shared mode](cluster.md#schedules-in-shared-mode) (any server fires, one per
occurrence).

## Access control

With [client identities](security.md#client-identities), managing a schedule
needs the `schedule` action for the schedule's workflow: creating, reading,
changing, pausing, resuming, triggering and deleting it. `GET /v1/schedules`
lists only the schedules of workflows the client may `schedule`. The action
grants nothing on executions: reading or cancelling the executions a schedule
started needs `read` or `cancel` as usual. Worker credentials cannot use
`/v1/schedules`.

## Metrics

| Metric | Meaning |
|---|---|
| `orbit_schedules_fired_total` | Executions this server started for schedules, including triggers. |
| `orbit_schedules_skipped_total{reason="overlap"}` | Starts the `skip` overlap policy prevented. |
| `orbit_schedules_skipped_total{reason="missed"}` | Missed occurrences the catch-up policy did not start. |

The schedule's own `fired` and `skipped` counts (`GET /v1/schedules/{id}`)
are stored in the database and cover every server; the metrics are per server
and start from zero when it starts. See [Observability](observability.md#metric-reference).

## Limitations

- Schedules are managed through the API and the CLI only. There is no
  declarative `schedule` form in `.orbit` files, and `orbit deploy` does not
  create schedules.
- Firing has a granularity of about one second; occurrences more often than
  every second are not possible (`interval` is at least `1s`).
- Due times are compared with the clock of the server that fires. Keep server
  clocks synchronized; in shared mode a server whose clock runs ahead fires
  early.
- In shared mode, an occurrence claimed by a server that died waits up to 30
  seconds for the claim to expire.
- A schedule stores at most one pending occurrence. With `allow`, a schedule
  whose executions take longer than its interval accumulates running
  executions; `skip` and `cancel_previous` keep at most one running.

## See also

- [HTTP API: schedules](../reference/http-api.md#schedules)
- [CLI: orbit schedule](../reference/cli.md#orbit-schedule)
- [Executions and history](executions.md)
- [Running several servers](cluster.md)
- [Storage: schedule table](storage.md#schedule-table)
- [Continue-as-new](../language/continue-as-new.md)
- [Web UI: schedules](web-ui.md#schedules)
