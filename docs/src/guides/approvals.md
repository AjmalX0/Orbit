# Human approvals

This guide shows how to put people into a workflow: a decision that waits for
a reviewer, several levels of approval, comments that can arrive at any time,
deadlines that escalate, and limits on what each person's tool can see and
do. Every decision is a typed signal, so a reviewer's tool cannot send a
decision the workflow does not understand, and every decision is recorded in
the execution's history. The complete files are
[`examples/expense.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/expense.orbit)
and
[`examples/expense.test.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/expense.test.orbit);
a single-level version is
[`examples/approval.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/approval.orbit).

## The building blocks

| Need | ORBIT construct |
|------|-----------------|
| A typed decision | `signal review { approved: Bool, by: String }` |
| Wait for it, with a deadline | `step manager { await manager_review  timeout 3d }` |
| Only ask when a rule says so | `when rules.needs_manager` on the `await` step |
| Act on the decision | a later step with `when coalesce(manager.approved, false)` |
| Messages that can come any number of times | `on event comment { ... }` |
| What to do when nobody answers | the `await` fails with `SIGNAL_TIMEOUT`; `on failure` runs |
| Show a reviewer where things stand | `query status = { ... }` |
| Limit what a reviewer's tool can do | a client identity with `actions = ["read", "signal", "query"]` |

## Two levels of approval

Claims of 100 or more need the employee's manager; claims of 5000 or more
also need finance, after the manager approved. The thresholds are a
`compute` step, so they are part of the reviewed definition:

```orbit
signal manager_review { approved: Bool, by: String }
signal finance_review { approved: Bool, by: String }

step rules {
    compute {
        needs_manager: input.amount >= 100.0
        needs_finance: input.amount >= 5000.0
    }
}

step manager {
    await manager_review
    when rules.needs_manager
    timeout 3d
}

step finance {
    await finance_review
    when rules.needs_finance && coalesce(manager.approved, false)
    timeout 5d
}

step pay {
    worker "payments.pay"
    when (!rules.needs_manager || coalesce(manager.approved, false))
        && (!rules.needs_finance || coalesce(finance.approved, false))
    with { claim_id: input.claim_id, amount: input.amount }
}
```

A long condition can continue on the next line when that line starts with
an operator such as `&&` or `||`.

`finance` reads `manager.approved`, so it waits for the manager's step to
finish; a finance decision sent early is buffered until `finance` waits for
it (see [Buffering and ordering](../language/signals.md#buffering-and-ordering)).
A step whose `when` is false is skipped and reads as `null` afterwards, which
is why the conditions use `coalesce`.

## Comments at any time

A comment is not a decision: it can arrive any number of times, and the
workflow should not wait for one. An `on event` block runs once per comment,
alongside the main steps, with the payload as `event`:

```orbit
signal comment { by: String, text: String }

on event comment {
    step forward {
        worker "people.notify"
        with { to: input.employee, message: event.by + " commented: " + event.text }
    }
}
```

The execution finishes only after every started block is done. See
[Event handlers](../language/events.md).

## Deadlines and escalation

When a review times out, the `await` step fails with `SIGNAL_TIMEOUT`, the
execution compensates what already happened, and `on failure` runs. Here it
tells the manager:

```orbit
on failure {
    step escalate {
        worker "people.notify"
        with { to: input.manager, message: "Claim " + input.claim_id + " was not decided in time: " + failure.message }
    }
}
```

To remind people instead of failing, keep the long `timeout` and run a
[schedule](../runtime/schedules.md) that lists waiting executions
(`orbit ps --workflow ExpenseClaim --status RUNNING`), reads their `status`
query and sends reminders.

## Where things stand

A query is a typed, read-only view, evaluated on demand. Steps that have not
run read as `null`:

```orbit
query status = {
    waiting_for: if coalesce(rules.needs_manager, false) && is_null(manager) then "manager"
        else if coalesce(rules.needs_finance, false) && coalesce(manager.approved, false) && is_null(finance) then "finance"
        else "nobody"
    paid: !is_null(pay)
}
```

```bash
orbit query claim-17 status
```

A query exposes only the fields you put in it. Give a reviewer's tool the
query, not the execution's full input and history, when the input contains
data the reviewer should not see.

## Sending a decision

From the CLI, the HTTP API or an SDK client:

```bash
orbit signal claim-17 manager_review --data '{"approved": true, "by": "bo"}'
```

```bash
curl -X POST -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"approved": true, "by": "bo"}' \
  http://127.0.0.1:7233/v1/executions/claim-17/signals/manager_review
```

The runtime validates the payload against the signal's declaration and
applies its defaults. A payload with a missing or mistyped field is refused
with `422 SIGNAL_PAYLOAD_INVALID` and is not recorded; a signal the workflow
does not declare is `404 UNKNOWN_SIGNAL`. The
[web UI](../runtime/web-ui.md) generates a form for each declared signal.

## Who may decide

Give the approval tool a client identity limited to this workflow and to
reading, querying and signalling:

```toml
[[client]]
id = "approvals-app"
token_sha256 = "..."   # from `orbit token --client approvals-app --workflow ExpenseClaim --action read --action query --action signal`
workflows = ["ExpenseClaim"]
actions = ["read", "query", "signal"]
```

The signal payload records who decided (`by`), so the history shows each
decision with its author. The server authenticates the tool, not the person:
the tool is responsible for putting the signed-in user into `by`.

## Testing every path

Signals in tests are sent when the execution waits for them (or, for
`on event` signals, as soon as it is idle), and deadlines are simulated:

```console
$ orbit test examples/expense.test.orbit
running 6 tests
ok      a small claim is paid without review (0ms simulated)
ok      a medium claim needs the manager only (0ms simulated)
ok      a large claim needs the manager, then finance (0ms simulated)
ok      finance can refuse what the manager approved (0ms simulated)
ok      comments are forwarded while the claim waits (0ms simulated)
ok      a claim nobody reviews fails after three days and escalates (3d 0h simulated)

test result: ok. 6 passed, 0 failed, 0 filtered out (0.02s)
```

## See also

- [Signals and queries](../language/signals.md)
- [Event handlers](../language/events.md)
- [Typed business process contracts](business-processes.md)
- [Security: client identities](../runtime/security.md#client-identities)
