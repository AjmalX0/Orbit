# Quick start

This tutorial takes you from an empty directory to a running workflow served
by a Python worker and a TypeScript worker. You create a project with
`orbit init`, run the runtime and the workers, start an execution, inspect its
history, and then add a failure path to see retries and compensation at work.
It takes about 15 minutes.

The outputs on this page come from real runs. Execution ids and timings differ
on your machine.

## Prerequisites

- The `orbit` CLI, version 0.1.0 or later. See [Installation](installation.md).
- Python 3.9 or later.
- Node.js 22.18 or later. The TypeScript worker is a `.ts` file that Node.js
  runs directly, without a build step.

You use three terminals: one for the runtime, one for each worker. Run every
command from the project directory unless the step says otherwise.

## 1. Create a project

```bash
orbit init hello-orbit
```

```text
  created hello-orbit/workers/typescript/package.json
  created hello-orbit/workers/python/requirements.txt
  created hello-orbit/.gitignore
  created hello-orbit/workflows/hello.orbit
  created hello-orbit/workers/python/worker.py
  created hello-orbit/workers/typescript/worker.ts
  created hello-orbit/README.md

Next: cd hello-orbit && orbit check workflows/hello.orbit
```

`orbit init` refuses to write into a directory that exists and is not empty.
The project contains:

```text
hello-orbit/
├── .gitignore
├── README.md
├── workflows/
│   └── hello.orbit            the workflow and its task contracts
└── workers/
    ├── python/
    │   ├── requirements.txt   orbit-lang-sdk>=0.1,<0.2
    │   └── worker.py          implements greeting.compose
    └── typescript/
        ├── package.json       depends on orbit-lang ^0.1.0
        └── worker.ts          implements greeting.decorate
```

## 2. Read the workflow

Open `workflows/hello.orbit`:

```orbit
// A two-language hello world: Python composes, TypeScript decorates.

task "greeting.compose" {
    version "1.0.0"
    input { name: String }
    output { message: String }
}

task "greeting.decorate" {
    version "1.0.0"
    input { message: String, excitement: Int }
    output { message: String }
}

workflow Hello {
    version "1.0.0"

    input {
        name: String
        excitement: Int = 3
    }

    output { greeting: String }

    step compose {
        worker "greeting.compose"
        using python
        with { name: input.name }
        retry { attempts: 3, backoff: exponential, initial_delay: 500ms }
    }

    step decorate {
        worker "greeting.decorate"
        using typescript
        with { message: compose.message, excitement: input.excitement }
        timeout 10s
    }

    return { greeting: decorate.message }
}
```

The file has two parts:

- **Task contracts.** Each `task` declares a typed interface: a name, a
  contract version, an input and an output. Workers implement contracts; the
  compiler checks every step against them.
- **A workflow.** `Hello` takes a `name` and an optional `excitement` (default
  `3`), runs two steps and returns a greeting. `decorate` reads
  `compose.message`, so it depends on `compose` and runs after it. `using
  python` and `using typescript` route each step only to workers of that
  language.

Type-check the file:

```bash
cd hello-orbit
orbit check workflows/hello.orbit
```

```text
ok Hello@1.0.0 (2 steps)
```

## 3. Install the worker dependencies

Install the Python SDK into a virtual environment, and the TypeScript SDK with
npm:

```bash
python3 -m venv .venv
. .venv/bin/activate
pip install -r workers/python/requirements.txt
(cd workers/typescript && npm install)
```

On Windows, activate the virtual environment with `.venv\Scripts\activate`.

## 4. Start the runtime

In the first terminal, from the project directory:

```bash
orbit server
```

```text
INFO orbit: starting ORBIT runtime db=orbit.db backend="sqlite" http_adapter=false auth=false ui=true
INFO orbit_runtime::server: ORBIT runtime listening addr=127.0.0.1:7233 tls=false ui=true
```

The runtime listens on `127.0.0.1:7233` and stores everything in `orbit.db` in
the current directory. Leave it running. Without a token it only accepts
connections from the local machine; see [Security](../runtime/security.md)
before exposing it.

## 5. Start the workers

Each worker is an ordinary long-running program. It registers the contracts it
implements and then long-polls the runtime for tasks.

### Python worker

`workers/python/worker.py` implements `greeting.compose`:

```python
"""Python worker for the Hello workflow.

Run:  pip install -r workers/python/requirements.txt
      python workers/python/worker.py
"""
from orbit_sdk import Worker

worker = Worker("hello-python")


@worker.task("greeting.compose", version="1.0.0")
def compose(input, ctx):
    ctx.log(f"composing a greeting for {input['name']}")
    return {"message": f"Hello, {input['name']}"}


if __name__ == "__main__":
    worker.run()
```

In the second terminal, activate the virtual environment and start it:

```bash
. .venv/bin/activate
python workers/python/worker.py
```

```text
2026-11-15 09:34:37,324 INFO orbit worker hello-python registered (1 tasks)
```

### TypeScript worker

`workers/typescript/worker.ts` implements `greeting.decorate`:

```ts
// TypeScript worker for the Hello workflow.
//
// Run:  (cd workers/typescript && npm install) && node workers/typescript/worker.ts
//       Node.js 22.18+ runs TypeScript directly.
import { Worker } from "orbit-lang";

const worker = new Worker({ workerId: "hello-typescript" });

worker.task<{ message: string; excitement: number }, { message: string }>(
  "greeting.decorate",
  { version: "1.0.0" },
  async (input) => ({ message: input.message + "!".repeat(input.excitement) }),
);

await worker.run();
```

In the third terminal:

```bash
node workers/typescript/worker.ts
```

```text
[orbit] worker hello-typescript registered (1 tasks)
```

Both workers connect to `http://127.0.0.1:7233` by default. Set
`ORBIT_SERVER` to point them at another runtime.

Confirm that the runtime sees them. You can run client commands from any
terminal; this tutorial uses the second one, but a fourth works as well:

```bash
orbit workers list
```

```text
WORKER                       RUNTIME      STATE    INFLIGHT  TASKS
hello-python                 python       online   0  greeting.compose@1.0.0
hello-typescript             typescript   online   0  greeting.decorate@1.0.0
```

## 6. Run the workflow

```bash
orbit run workflows/hello.orbit --input '{"name": "Ada"}'
```

```text
deployed Hello@1.0.0 (sha256:505930b4d466)
started execution 99e811dd-7448-41cc-ac28-bc58d50200d2
  ✓ compose                        succeeded 122ms
  ✓ decorate                       succeeded 140ms

COMPLETED 99e811dd-7448-41cc-ac28-bc58d50200d2  Hello@1.0.0
  elapsed: 269ms
{
  "greeting": "Hello, Ada!!!"
}
```

`orbit run` did three things:

1. **Compiled and deployed** the file. The runtime registered `Hello@1.0.0`
   under the content hash `sha256:505930b4d466...`. Running the same file
   again prints `up to date` instead of `deployed`.
2. **Started an execution** with the input `{"name": "Ada"}`. The runtime
   applied the default `excitement: 3` and validated the input against the
   workflow's `input` type.
3. **Followed the execution** until it finished and printed the output.

`orbit run` exits with status 0 when the execution completes and 1 when it
fails, so you can use it in scripts. Pass `--no-wait` to return right after
the execution starts.

## 7. Inspect the execution

Every execution keeps its complete history. Use the execution id printed by
`orbit run` (yours differs).

`orbit status` shows the outcome and one row per step:

```bash
orbit status 99e811dd-7448-41cc-ac28-bc58d50200d2
```

```text
COMPLETED 99e811dd-7448-41cc-ac28-bc58d50200d2  Hello@1.0.0
  elapsed: 269ms

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  compose                      SUCCEEDED          1    122ms  worker hello-python
  decorate                     SUCCEEDED          1    140ms  worker hello-typescript

  output: {"greeting":"Hello, Ada!!!"}
```

`orbit logs` prints the event history as a timeline. These events are the
authoritative record; the runtime rebuilds an execution's state from them
after a restart:

```bash
orbit logs 99e811dd-7448-41cc-ac28-bc58d50200d2
```

```text
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     2ms   #3  StepScheduled             step=compose attempt=1
+     3ms   #4  StepDispatched            step=compose attempt=1 worker_id=hello-python
+   125ms   #5  StepSucceeded             step=compose attempt=1
+   127ms   #6  StepScheduled             step=decorate attempt=1
+   128ms   #7  StepDispatched            step=decorate attempt=1 worker_id=hello-typescript
+   268ms   #8  StepSucceeded             step=decorate attempt=1
+   269ms   #9  WorkflowCompleted
```

`orbit inspect` prints the full state as JSON, including each step's input,
output and idempotency key (abridged here):

```bash
orbit inspect 99e811dd-7448-41cc-ac28-bc58d50200d2
```

```json
{
  "id": "99e811dd-7448-41cc-ac28-bc58d50200d2",
  "workflow": "Hello",
  "version": "1.0.0",
  "definition_hash": "sha256:505930b4d4668ff2f8cb95e04519d5292ef2fbf0630d575349aaf92c0e2b42be",
  "input": {
    "name": "Ada",
    "excitement": 3
  },
  "status": "COMPLETED",
  "units": {
    "compose": {
      "status": "SUCCEEDED",
      "attempt": 1,
      "input": {
        "name": "Ada"
      },
      "idempotency_key": "99e811dd-7448-41cc-ac28-bc58d50200d2/compose",
      "output": {
        "message": "Hello, Ada"
      },
      "redelivery": false,
      "worker_id": "hello-python",
      "tokens": "***"
    },
    "decorate": { "...": "..." }
  },
  "output": {
    "greeting": "Hello, Ada!!!"
  },
  "last_seq": 9
}
```

`orbit ps` lists recent executions:

```bash
orbit ps
```

```text
EXECUTION                              WORKFLOW                 VERSION    STATUS        AGE
99e811dd-7448-41cc-ac28-bc58d50200d2   Hello                    1.0.0      COMPLETED     314ms
```

## 8. Add a failure path

Real workflows fail. In this step you add two steps and make the workers fail
on purpose, so that you can see a retry, a permanent failure and a
compensation.

- `publish` (Python) stores the greeting and returns a `post_id`. Its
  `compensate` block declares how to undo it: `greeting.unpublish`.
- `notify` (TypeScript) rejects messages longer than 40 characters with a
  non-retryable error.

Replace `workflows/hello.orbit` with the following. The version changes to
`1.1.0`; the next section explains why.

```orbit
// A two-language hello world: Python composes, TypeScript decorates.

task "greeting.compose" {
    version "1.0.0"
    input { name: String }
    output { message: String }
}

task "greeting.decorate" {
    version "1.0.0"
    input { message: String, excitement: Int }
    output { message: String }
}

task "greeting.publish" {
    version "1.0.0"
    input { message: String }
    output { post_id: String }
}

task "greeting.unpublish" {
    version "1.0.0"
    input { post_id: String }
    output { removed: Bool }
}

task "greeting.notify" {
    version "1.0.0"
    input { post_id: String, message: String }
    output { delivered: Bool }
}

workflow Hello {
    version "1.1.0"

    input {
        name: String
        excitement: Int = 3
    }

    output { greeting: String, post_id: String }

    step compose {
        worker "greeting.compose"
        using python
        with { name: input.name }
        retry { attempts: 3, backoff: exponential, initial_delay: 500ms }
    }

    step decorate {
        worker "greeting.decorate"
        using typescript
        with { message: compose.message, excitement: input.excitement }
        timeout 10s
    }

    step publish {
        worker "greeting.publish"
        using python
        with { message: decorate.message }
        compensate {
            worker "greeting.unpublish"
            with { post_id: publish.post_id }
        }
    }

    step notify {
        worker "greeting.notify"
        using typescript
        with { post_id: publish.post_id, message: decorate.message }
    }

    return { greeting: decorate.message, post_id: publish.post_id }
}
```

Replace `workers/python/worker.py`. `compose` now fails its first attempt for
names that start with `Flaky`, with a retryable error. `publish` uses the
task's idempotency key so that a redelivered task returns the same post
instead of creating a second one:

```python
import uuid

from orbit_sdk import TaskError, Worker

worker = Worker("hello-python")
posts = {}  # stands in for a database


@worker.task("greeting.compose", version="1.0.0")
def compose(input, ctx):
    # Simulate a flaky dependency: names starting with "Flaky" fail once.
    if input["name"].startswith("Flaky") and ctx.attempt == 1:
        raise TaskError("NAME_SERVICE_BUSY", "try again shortly", retryable=True)
    ctx.log(f"composing a greeting for {input['name']}")
    return {"message": f"Hello, {input['name']}"}


@worker.task("greeting.publish", version="1.0.0")
def publish(input, ctx):
    post_id = posts.get(ctx.idempotency_key) or f"post-{uuid.uuid4().hex[:8]}"
    posts[ctx.idempotency_key] = post_id
    ctx.log(f"published {post_id}")
    return {"post_id": post_id}


@worker.task("greeting.unpublish", version="1.0.0")
def unpublish(input, ctx):
    ctx.log(f"removed {input['post_id']}")
    return {"removed": True}


if __name__ == "__main__":
    worker.run()
```

Replace `workers/typescript/worker.ts`:

```ts
import { TaskError, Worker } from "orbit-lang";

const worker = new Worker({ workerId: "hello-typescript" });

worker.task<{ message: string; excitement: number }, { message: string }>(
  "greeting.decorate",
  { version: "1.0.0" },
  async (input) => ({ message: input.message + "!".repeat(input.excitement) }),
);

worker.task<{ post_id: string; message: string }, { delivered: boolean }>(
  "greeting.notify",
  { version: "1.0.0" },
  async (input) => {
    if (input.message.length > 40) {
      throw new TaskError("MESSAGE_TOO_LONG", `${input.message.length} characters`, { retryable: false });
    }
    return { delivered: true };
  },
);

await worker.run();
```

Stop both workers with Ctrl-C and start them again so that they register the
new tasks. The Python worker now reports `registered (3 tasks)` and the
TypeScript worker `registered (2 tasks)`. Then check the workflow:

```bash
orbit check workflows/hello.orbit
```

```text
ok Hello@1.1.0 (4 steps)
```

### Why the version changed

Workflow definitions are immutable. If you had kept `version "1.0.0"`, the
runtime would reject the new content:

```text
error: DEFINITION_CONFLICT (409): Hello@1.0.0 is already registered with a different definition (sha256:505930b4d4668ff2f8cb95e04519d5292ef2fbf0630d575349aaf92c0e2b42be); workflow definitions are immutable, bump the version
```

Bumping the version registers `Hello@1.1.0` next to `Hello@1.0.0`. Executions
that started on 1.0.0 keep running on 1.0.0. See
[Versioning](../language/versioning.md).

### Run the happy path

```bash
orbit run workflows/hello.orbit --input '{"name": "Grace"}'
```

```text
deployed Hello@1.1.0 (sha256:c444589a24d2)
started execution c9bade94-bc35-4741-80a9-3a39780623d7
  ✓ compose                        succeeded 8ms
  ✓ decorate                       succeeded 7ms
  ✓ publish                        succeeded 10ms
  ✓ notify                         succeeded 6ms

COMPLETED c9bade94-bc35-4741-80a9-3a39780623d7  Hello@1.1.0
  elapsed: 45ms
{
  "greeting": "Hello, Grace!!!",
  "post_id": "post-71a73bf3"
}
```

### Run the failure path

Now use a name that triggers the flaky `compose`, and enough excitement to
make the message longer than 40 characters:

```bash
orbit run workflows/hello.orbit --input '{"name": "Flaky Grace", "excitement": 30}'
```

```text
up to date Hello@1.1.0 (sha256:c444589a24d2)
started execution d599b57e-eee0-412e-b679-df8276005035
  ✓ compose                        succeeded 6ms
  ✓ decorate                       succeeded 4ms
  ✓ publish                        succeeded 4ms
  ✗ notify                         failed 48 characters
  ✓ compensate:publish             succeeded 6ms

FAILED d599b57e-eee0-412e-b679-df8276005035  Hello@1.1.0
  elapsed: 549ms
  compensation: completed
  error: MESSAGE_TOO_LONG step `notify` failed: 48 characters
```

The status table shows that `compose` needed two attempts:

```bash
orbit status d599b57e-eee0-412e-b679-df8276005035
```

```text
FAILED d599b57e-eee0-412e-b679-df8276005035  Hello@1.1.0
  elapsed: 549ms
  compensation: completed
  error: MESSAGE_TOO_LONG step `notify` failed: 48 characters

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  compose                      SUCCEEDED          2      6ms  worker hello-python
  decorate                     SUCCEEDED          1      4ms  worker hello-typescript
  publish                      SUCCEEDED          1      4ms  worker hello-python
  notify                       FAILED             1      5ms  48 characters
  compensate:publish           SUCCEEDED          1      6ms  worker hello-python
```

The history records every decision:

```bash
orbit logs d599b57e-eee0-412e-b679-df8276005035
```

```text
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     2ms   #3  StepScheduled             step=compose attempt=1
+     3ms   #4  StepDispatched            step=compose attempt=1 worker_id=hello-python
+     7ms   #5  StepAttemptFailed         step=compose attempt=1 error=NAME_SERVICE_BUSY try again shortly
+     8ms   #6  RetryScheduled            step=compose delay_ms=500
+   508ms   #7  StepScheduled             step=compose attempt=2
+   511ms   #8  StepDispatched            step=compose attempt=2 worker_id=hello-python
+   517ms   #9  StepSucceeded             step=compose attempt=2
+   518ms  #10  StepScheduled             step=decorate attempt=1
+   519ms  #11  StepDispatched            step=decorate attempt=1 worker_id=hello-typescript
+   523ms  #12  StepSucceeded             step=decorate attempt=1
+   524ms  #13  StepScheduled             step=publish attempt=1
+   526ms  #14  StepDispatched            step=publish attempt=1 worker_id=hello-python
+   530ms  #15  StepSucceeded             step=publish attempt=1
+   531ms  #16  StepScheduled             step=notify attempt=1
+   535ms  #17  StepDispatched            step=notify attempt=1 worker_id=hello-typescript
+   539ms  #18  StepAttemptFailed         step=notify attempt=1 error=MESSAGE_TOO_LONG 48 characters
+   540ms  #19  StepFailed                step=notify error=MESSAGE_TOO_LONG 48 characters
+   540ms  #20  CompensationStarted       reason=step `notify` failed: 48 characters
+   540ms  #21  StepScheduled             step=compensate:publish attempt=1
+   542ms  #22  StepDispatched            step=compensate:publish attempt=1 worker_id=hello-python
+   548ms  #23  StepSucceeded             step=compensate:publish attempt=1
+   549ms  #24  CompensationCompleted
+   549ms  #25  WorkflowFailed            error=MESSAGE_TOO_LONG step `notify` failed: 48 characters
```

Reading the timeline:

- **Retry (#5 to #9).** The first `compose` attempt failed with a retryable
  error. The step's policy allows three attempts with exponential backoff
  starting at 500 ms, so the runtime waited 500 ms and dispatched attempt 2,
  which succeeded.
- **Permanent failure (#18, #19).** `notify` failed with
  `retryable: false`, so the runtime failed the step immediately, without
  using the remaining attempts. `notify` has no `retry` block, so it would
  have had only one attempt in any case.
- **Compensation (#20 to #24).** Because the workflow failed after `publish`
  succeeded, the runtime ran `publish`'s compensation, `greeting.unpublish`,
  with the `post_id` that `publish` returned. Steps without a `compensate`
  block (`compose`, `decorate`) have nothing to undo.
- **Outcome (#25).** The execution ended `FAILED` with
  `compensation: completed`: the failure was handled and the side effect was
  undone.

The Python worker's output (timestamps removed) shows the failed attempt, the retry, and both sides of the compensation:

```text
INFO orbit executing {"execution_id": "d599b57e-...", "step": "compose", "attempt": 1, "task": "greeting.compose", "redelivery": false}
INFO orbit task failed {"execution_id": "d599b57e-...", "step": "compose", "attempt": 1, "code": "NAME_SERVICE_BUSY"}
INFO orbit executing {"execution_id": "d599b57e-...", "step": "compose", "attempt": 2, "task": "greeting.compose", "redelivery": false}
INFO orbit composing a greeting for Flaky Grace {"execution_id": "d599b57e-...", "step": "compose", "attempt": 2}
INFO orbit executing {"execution_id": "d599b57e-...", "step": "publish", "attempt": 1, "task": "greeting.publish", "redelivery": false}
INFO orbit published post-2ce4c9b2 {"execution_id": "d599b57e-...", "step": "publish", "attempt": 1}
INFO orbit executing {"execution_id": "d599b57e-...", "step": "compensate:publish", "attempt": 1, "task": "greeting.unpublish", "redelivery": false}
INFO orbit removed post-2ce4c9b2 {"execution_id": "d599b57e-...", "step": "compensate:publish", "attempt": 1}
```

## 9. Clean up

Stop the workers and the runtime with Ctrl-C. All state is in `orbit.db`,
`orbit.db-wal` and `orbit.db-shm` in the project directory; the project's
`.gitignore` already excludes them. Delete them to start from an empty
runtime.

## What you learned

- A workflow file declares typed task contracts and the steps that use them.
  `orbit check` validates it before anything runs.
- Workers in any language register contracts and pull tasks from the runtime.
- `orbit run` deploys and starts an execution; `orbit status`, `orbit logs`,
  `orbit inspect` and `orbit ps` show what happened and why.
- Retries are policy in the workflow, not code in the worker. A
  `retryable: false` error stops retrying at once.
- When a workflow fails, the runtime compensates the steps that succeeded,
  newest first.
- Definitions are immutable per version; changing a workflow means bumping
  its version.

## Next steps

- [Core concepts](concepts.md): precise definitions of the terms used here.
- [Project layout](project.md): how to organize a larger project and use it in
  CI.
- [Orders with compensation](../guides/saga.md): a five-step order workflow
  with parallel steps, a condition and compensation.
- [Retries and timeouts](../language/retries-timeouts.md) and
  [Compensation](../language/compensation.md): the full rules.
- [Python](../sdks/python.md) and [TypeScript and JavaScript](../sdks/typescript.md):
  the SDK reference.
