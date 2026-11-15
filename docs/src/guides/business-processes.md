# Typed business process contracts

This guide shows how to write a business process in ORBIT so that the process
definition is a contract you can review, check and audit: customer onboarding
with identity verification, sanctions screening, a compliance officer's
review, and an account that is closed again if anything later fails. It
covers what the compiler guarantees, how to test every path, and how to read
the record an execution leaves. The complete files are
[`examples/onboarding.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/onboarding.orbit)
and
[`examples/onboarding.test.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/onboarding.test.orbit).

## What "contract" means here

In a process written as code, the rules live in branches, helper functions
and conventions. In ORBIT they are declarations the compiler checks:

| Rule | Where it is declared | What checks it |
|------|----------------------|----------------|
| Each service call has a fixed input and output shape | a `task` contract | The compiler, on every step; the runtime, on every result |
| A side effect has an undo action | `compensated_by` on the contract, `compensate` on the step | `W0601`/`W0602`, errors under `orbit check --strict` |
| A human decision has a typed payload and a deadline | `signal` and `await ... timeout` | The compiler (payload use), the runtime (payload validation), `W0606` for waits without a timeout |
| Business rules are visible | `compute` steps and `when` conditions | The type checker; the runtime evaluates them deterministically |
| What happens on failure, cancellation and timeout | `on failure`, `on cancel`, `on timeout` | The compiler; the runtime runs them after compensation |
| Who may start, signal or read a process | client identities in the workers file | The server, per workflow and action |
| A change does not break running callers | `version`, `orbit compat` | `orbit compat` before you deploy |

## The contracts

Each external service is a task contract. `accounts.open` declares that its
effect is undone by `accounts.close`, so every workflow that opens an account
must say how it closes it:

```orbit
task "accounts.open" {
    version "1.0.0"
    input { customer_id: String, product: String, risk: String }
    output { account_id: String }
    compensated_by "accounts.close"
}

task "accounts.close" {
    version "1.0.0"
    input { account_id: String, reason: String }
    output { closed: Bool }
}
```

Contracts can live in a shared file that several teams import, so the
worker team and the process owner agree on one definition.

## The process

The risk rules are a `compute` step, written in the process where a reviewer
can read them, and evaluated by the runtime with no worker:

```orbit
step risk {
    compute {
        level: if !identity.verified || screening.hits > 0 then "high"
            else if identity.confidence < 0.9 then "medium"
            else "low"
        needs_review: !identity.verified || screening.hits > 0 || identity.confidence < 0.9
    }
}

step decision {
    await review
    when risk.needs_review
    timeout 5d
}

step open_account {
    worker "accounts.open"
    when !risk.needs_review || coalesce(decision.approved, false)
    with { customer_id: input.customer_id, product: input.product, risk: risk.level }
    idempotency_key: "open-" + input.customer_id
    compensate {
        worker "accounts.close"
        with { account_id: open_account.account_id, reason: "onboarding did not complete" }
    }
}
```

The officer's decision is a typed signal. Sending one with a missing or
mistyped field is refused by the server with `422 SIGNAL_PAYLOAD_INVALID`
and never reaches the process:

```orbit
signal review { approved: Bool, officer: String, note: String? }
```

## What the compiler refuses

Delete the `compensate` block from `open_account` and the strict check fails,
naming the action that would be left in place:

```text
warning[W0601]: step `open_account` calls `accounts.open` but has no compensation
  --> onboarding.orbit:96:16
   |
96 |         worker "accounts.open"
   |                ^^^^^^^^^^^^^^^
   = note: `accounts.open` declares `compensated_by "accounts.close"`: if a later step fails, its effect would be left in place
   = help: add `compensate { worker "accounts.close" with { ... } }`

error: 1 warning(s) treated as errors (--strict)
```

Run `orbit check --strict` in CI so this cannot be merged. Other checks that
apply here: a `with` field of the wrong type (`E0400`), a missing required
field (`E0403`), reading a value that may be `null` without handling it
(`E0405`), a condition that is always false or always true (`W0603`,
`W0604`), and an `await` without a timeout (`W0606`). See
[Static safety checks](../language/safety.md).

## Testing every path

Each path through the process is a `test` block with mocked services. Tests
are type-checked against the contracts, so a mock that returns the wrong
shape does not compile:

```orbit
test "a failure after the account is opened closes it again" {
    workflow Onboarding
    input { customer_id: "c-5", full_name: "T. Hoe", document_id: "P111", country: "BE" }
    mock "identity.verify" returns { verified: true, confidence: 0.99, reasons: [] }
    mock "sanctions.screen" returns { hits: 0, lists: [] }
    mock "accounts.open" returns { account_id: "acc-" + input.customer_id }
    mock "audit.record" fails "AUDIT_UNAVAILABLE" "the audit store rejected the write"
    mock "accounts.close" returns { closed: true }
    expect status FAILED
    expect error "AUDIT_UNAVAILABLE"
    expect compensated open_account
}
```

```console
$ orbit test examples/onboarding.test.orbit
running 5 tests
ok      a verified, clean customer is opened without review (0ms simulated)
ok      a sanctions hit waits for an officer, who approves (0ms simulated)
ok      an officer rejection opens nothing and is audited (0ms simulated)
ok      a review nobody answers fails after five days and is recorded (5d 0h simulated)
ok      a failure after the account is opened closes it again (0ms simulated)

test result: ok. 5 passed, 0 failed, 0 filtered out (0.04s)
```

The five-day deadline is simulated: durable time is fast-forwarded, so the
whole suite runs in milliseconds. See [Testing workflows](../tooling/testing.md).

## The record of an execution

Every execution's history is the audit trail: each decision, each attempt,
the officer's signal and who sent it, and the outcome. `orbit simulate` shows
the same timeline locally, here for a sanctions hit that an officer approves:

```text
+     0ms   #9  StepSucceeded             step=risk attempt=1
+     0ms  #10  SignalAwaitStarted        step=decision signal=review
+     0ms  #11  SignalReceived            name=review
+     0ms  #12  SignalDelivered           step=decision signal_seq=11
+     0ms  #13  StepScheduled             step=open_account attempt=1
...
+     0ms  #22  WorkflowCompleted

  output: {"status":"opened","account_id":"acc-c-9","risk":"high"}
```

On a server, `orbit logs <id>` prints this history and
`GET /v1/executions/{id}/history` returns it as JSON. The history is
append-only and records the definition hash the execution ran, so you can
show exactly which version of the process made each decision. See
[Executions and history](../runtime/executions.md).

## Limiting who can act

A compliance tool should be able to read onboarding executions and send
`review` signals, and nothing else. Give it a client identity:

```toml
[[client]]
id = "compliance-console"
token_sha256 = "..."   # from `orbit token --client compliance-console`
workflows = ["Onboarding"]
actions = ["read", "signal", "query"]
```

It cannot start executions, deploy workflows or touch other workflows. See
[Security: client identities](../runtime/security.md#client-identities).

## Changing the process

Treat a change to the process like an API change. `orbit compat` compares the
old and new definitions and reports breaking changes to inputs, outputs,
signals and queries and the version bump they require; running executions
keep the definition they started with. See [Versioning](../language/versioning.md).

## See also

- [Human approvals](approvals.md)
- [Orders with compensation](saga.md)
- [Outcome handlers](../language/handlers.md)
- [Static safety checks](../language/safety.md)
