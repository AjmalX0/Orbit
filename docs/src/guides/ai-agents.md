# AI agents with guardrails

This guide shows how to run an AI agent as an ORBIT workflow, so that the
agent's tool calls are durable, typed and bounded: a research agent that
plans searches with a language model, runs them in parallel, drafts an
answer, has the draft checked, asks a human editor when the check is not
convincing, and publishes, withdrawing the article if anything later fails.
The model provider is not part of the workflow: workers call whatever model
you use. The complete files are
[`examples/agent.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/agent.orbit)
and
[`examples/agent.test.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/agent.test.orbit).

## Why a workflow

An agent loop written as a script loses its progress when the process dies,
retries tool calls by hand, and trusts whatever shape the model returns.
Running it as a workflow gives you:

| Problem | What the workflow does |
|---------|------------------------|
| The process crashes halfway | Every step's result is recorded; the run continues from the history, without repeating finished tool calls |
| A model or tool call fails | `retry` with backoff and `timeout` per step; non-retryable errors stop at once |
| The model returns the wrong shape | Each model call is a task contract; a result that does not match it fails the step instead of flowing on |
| The agent does too much | `limits { max_fan_out }` caps the parallel tool calls; a `compute` step enforces the budget before any tool runs |
| An answer needs a human | An `await` on a typed signal, only when the guardrail check asks for it |
| A side effect must be undone | `compensate` on the step that has the effect |
| You need to know what happened | The history records every call, result, decision and who approved it |

## Model and tool calls are typed tasks

Each call the agent can make is a task contract. The worker that implements
`llm.plan` calls the model and returns this shape; anything else is rejected
by the runtime:

```orbit
task "llm.plan" {
    version "1.0.0"
    input { question: String, max_queries: Int }
    output { queries: List<String>, estimated_cost_usd: Float }
}

task "tools.search" {
    version "1.0.0"
    input { query: String }
    output { query: String, snippets: List<String>, sources: List<String> }
}
```

Put the model's structured-output schema in the worker, matching the
contract. The worker can validate before returning, and the runtime validates
again: an `OUTPUT_CONTRACT_VIOLATION` failure in the history means the model or the
worker broke the contract.

## Bounding what the agent does

The plan comes from the model, so the workflow checks it before acting on
it. `limits` sets ceilings no plan can exceed, and a `compute` step enforces
the budget deterministically:

```orbit
limits { max_fan_out: 10, max_history_events: 1000 }

step budget {
    compute {
        within: plan.estimated_cost_usd <= input.budget_usd && len(plan.queries) <= input.max_queries
    }
}

step search {
    for query in plan.queries
    max_parallel 4
    worker "tools.search"
    when budget.within
    with { query: query }
    retry { attempts: 3, backoff: exponential, initial_delay: 1s }
    timeout 30s
}
```

A plan over budget skips every tool call. The timeline shows nothing ran
after the check:

```text
+     0ms   #5  StepSucceeded             step=plan attempt=1
+     0ms   #6  StepSucceeded             step=budget attempt=1
+     0ms   #7  StepSkipped               step=search reason=`when` condition was false
+     0ms   #8  StepSkipped               step=draft reason=`when` condition was false
...
+     0ms  #14  WorkflowCompleted
```

A `for` over more than `max_fan_out` elements fails the step with
`LIMIT_EXCEEDED` before any element is scheduled, whatever the budget check
says. See [Resource limits](../runtime/server.md#resource-limits).

Steps guarded by `when budget.within` read each other without `coalesce`:
`draft`, `check` and `gate` all share that condition, so the compiler knows
the steps they read have run. See
[Steps that share a condition](../language/conditions.md#steps-that-share-a-condition).

## Guardrail check and human review

A separate task scores the draft. When it is not confident, or it flags
something, the workflow waits for an editor, who can approve, reject or
replace the text:

```orbit
signal editor_review { approved: Bool, editor: String, edited_text: String? }

step check {
    worker "guardrails.check"
    when budget.within
    with { text: draft.text, citations: draft.citations }
}

step gate {
    compute {
        needs_editor: check.confidence < 0.8 || len(check.flags) > 0
    }
    when budget.within
}

step review {
    await editor_review
    when coalesce(gate.needs_editor, false)
    timeout 2d
}
```

The guardrail can be a model, a rules engine or both; it is a worker like any
other. The threshold (`0.8`) is in the workflow, where it is reviewed and
versioned.

## Undoing a side effect

Publishing is the agent's side effect, so it declares its undo action. If a
later step fails, the article is withdrawn before `on failure` alerts the
team:

```orbit
step publish {
    worker "content.publish"
    when budget.within && (!coalesce(gate.needs_editor, true) || coalesce(review.approved, false))
    with { slug: input.slug, text: coalesce(review.edited_text, draft.text) }
    idempotency_key: "publish-" + input.slug
    compensate {
        worker "content.unpublish"
        with { slug: input.slug }
    }
}
```

`content.publish` declares `compensated_by "content.unpublish"`, so
`orbit check --strict` fails if someone removes the `compensate` block. The
idempotency key means a retried or redelivered publish does not post twice,
if the worker passes the key to the publishing service.

## Testing the agent without a model

Tests replace every model and tool with a mock, so the paths through the
agent are tested deterministically and in milliseconds:

```console
$ orbit test examples/agent.test.orbit
running 6 tests
ok      a confident, unflagged draft is published without an editor (0ms simulated)
ok      a low-confidence draft waits for an editor, who approves an edited text (0ms simulated)
ok      an editor rejection publishes nothing (0ms simulated)
ok      a plan over budget stops before any tool is called (0ms simulated)
ok      a model error fails the run and alerts the team (0ms simulated)
ok      a failure after publishing withdraws the article (0ms simulated)

test result: ok. 6 passed, 0 failed, 0 filtered out (0.04s)
```

Mocks are type-checked against the contracts: a mock that returns
`queries: "not a list"` does not compile, which is the same mistake the
runtime would catch from a real model.

## Writing the workers

A worker is an ordinary program in any SDK language. A Python sketch of
`llm.plan`, with the model call left to your provider's client:

```python
from orbit_sdk import Worker, TaskError

worker = Worker("agent-workers", server="http://127.0.0.1:7233", token=TOKEN)

@worker.task("llm.plan", version="1.0.0")
def plan(input, ctx):
    result = call_model(               # your provider's client
        prompt=f"Plan at most {input['max_queries']} web searches for: {input['question']}",
        response_schema={"queries": [str], "estimated_cost_usd": float},
    )
    if not result.get("queries"):
        raise TaskError("EMPTY_PLAN", "the model returned no queries", retryable=True)
    return result

worker.run()
```

See [Python SDK](../sdks/python.md) for the exact API, and
[How workers work](../sdks/overview.md) for retries, timeouts and
idempotency keys.

## What this does not do

- It does not make a model deterministic. The workflow records the model's
  answers, so a replay or a crash recovery reuses them instead of asking
  again.
- It does not stream tokens. A step returns when the model call finishes.
- Agent loops of unknown length (call tools until the model says stop) need
  a bounded `for`, a chain of child workflows, or
  [continue-as-new](../language/continue-as-new.md); there is no `while`.

## See also

- [Human approvals](approvals.md)
- [Fan-out](../language/fan-out.md)
- [Compensation](../language/compensation.md)
- [Testing workflows](../tooling/testing.md)
