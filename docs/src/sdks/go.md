# Go

The Go SDK, package `orbit` in the module `github.com/AjmalX0/Orbit/sdks/go`,
runs ORBIT task handlers written in Go. It uses only the standard library.
Handlers receive a `context.Context` that is cancelled when the runtime
cancels the attempt, when the attempt's deadline passes, or when the worker
shuts down. The SDK handles registration, long polling, heartbeats, result
delivery through runtime outages, and graceful shutdown when the context
passed to `Run` is cancelled. Read [How workers work](overview.md) first for
the concepts this page relies on.

## Install

```bash
go get github.com/AjmalX0/Orbit/sdks/go@latest
```

The module requires Go 1.21 or later. Import it under the name `orbit`:

```go
import orbit "github.com/AjmalX0/Orbit/sdks/go"
```

## A minimal worker

This worker implements `text.frequencies`, the Go step of
[`examples/polyglot.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/polyglot.orbit):

```orbit
schema WordCount { word: String, count: Int }
task "text.frequencies" { version "1.0.0" input { tokens: List<String>, top: Int } output { top: List<WordCount>, unique: Int } }

step frequencies { worker "text.frequencies" using go with { tokens: tokenize.tokens, top: input.top } }
```

```go
package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"sort"
	"syscall"

	orbit "github.com/AjmalX0/Orbit/sdks/go"
)

type input struct {
	Tokens []string `json:"tokens"`
	Top    int      `json:"top"`
}

type wordCount struct {
	Word  string `json:"word"`
	Count int    `json:"count"`
}

func frequencies(ctx context.Context, t *orbit.Task) (any, error) {
	var in input
	if err := t.Decode(&in); err != nil {
		return nil, orbit.Permanent("INVALID_INPUT", err.Error())
	}
	counts := map[string]int{}
	for _, tok := range in.Tokens {
		counts[tok]++
	}
	top := make([]wordCount, 0, len(counts))
	for word, n := range counts {
		top = append(top, wordCount{word, n})
	}
	sort.Slice(top, func(i, j int) bool {
		if top[i].Count != top[j].Count {
			return top[i].Count > top[j].Count
		}
		return top[i].Word < top[j].Word
	})
	if in.Top >= 0 && len(top) > in.Top {
		top = top[:in.Top]
	}
	return map[string]any{"top": top, "unique": len(counts)}, nil
}

func main() {
	w := orbit.NewWorker(orbit.Options{WorkerID: "text-go"})
	w.Task("text.frequencies", "1.0.0", frequencies)

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if err := w.Run(ctx); err != nil {
		log.Fatal(err)
	}
}
```

Run it against a local runtime. The workflow has four steps in four
languages, so start the other three example workers from the repository root
as well:

```bash
orbit server &
go run . &
PYTHONPATH=sdks/python python3 examples/workers/polyglot/text_worker.py &
node examples/workers/polyglot/text_worker.ts &
cargo run -p orbit-worker --example text_worker &

orbit run examples/polyglot.orbit \
  --input '{"text": "The quick brown fox jumps over the lazy dog. The dog sleeps."}'
```

## Registering tasks

```go
func (w *Worker) Task(name, version string, handler Handler) *Worker
```

`Task` registers `handler` for `name@version` and returns the worker, so calls
can be chained. The name and version must match the `task` declaration in
your `.orbit` files exactly. Registering the same name twice panics. Register
every task before calling `Run`; `Task` is not safe to call concurrently with
`Run`.

## Handlers and results

```go
type Handler func(ctx context.Context, t *orbit.Task) (any, error)
```

The handler receives the attempt's context and the task. Decode the input with
`t.Decode(&v)`, which calls `json.Unmarshal` on the raw input. Return the
output and a nil error to complete the attempt:

- The output is marshaled with `encoding/json` and must be a JSON object, so
  return a struct, a `map[string]any`, or a `json.RawMessage` holding an
  object.
- Returning a nil output reports `{}`. A typed nil map marshals to `null` and
  fails.
- Output that does not marshal to an object fails the attempt with
  `INVALID_RESULT`, which is not retried.
- The runtime validates the output against the contract's output schema; a
  mismatch fails the step with `OUTPUT_CONTRACT_VIOLATION`.

## Returning task errors

Return a `*orbit.TaskError` to fail an attempt with a structured error:

```go
if amount > 5000 {
	return nil, orbit.Permanent("CARD_DECLINED", "limit exceeded")
}
if err := callBank(ctx); err != nil {
	return nil, orbit.Retryable("BANK_UNAVAILABLE", err.Error())
}
return nil, &orbit.TaskError{
	Code:      "CARD_DECLINED",
	Message:   "limit exceeded",
	Retryable: false,
	Details:   map[string]any{"limit": 5000},
}
```

| Constructor | Retryable |
|---|---|
| `orbit.Retryable(code, message string) *TaskError` | yes |
| `orbit.Permanent(code, message string) *TaskError` | no, fails the step whatever its retry policy |

The SDK finds a task error with `errors.As`, so a wrapped task error
(`fmt.Errorf("charge: %w", te)`) keeps its code. Any other error is reported
as a retryable `UNHANDLED_EXCEPTION` with `err.Error()` as the message. A
panic in the handler is recovered and reported as a retryable `PANIC`.

## The task

`*orbit.Task` carries everything the runtime sent:

| Field | Type | Meaning |
|---|---|---|
| `Token` | `string` | Identifies this delivery. The SDK echoes it in the result. |
| `ExecutionID` | `string` | The workflow execution. |
| `Workflow` | `string` | Workflow name. |
| `WorkflowVersion` | `string` | Workflow version. |
| `StepID` | `string` | The step, or `compensate:<step>` for a compensation; `step[i]` for a fan-out element. |
| `TaskName` | `string` | The task name. |
| `ContractVersion` | `string` | The task's contract version. |
| `Input` | `json.RawMessage` | The raw task input. |
| `IdempotencyKey` | `string` | Stable across retries and redeliveries. Use it for side effects. |
| `Attempt` | `int` | 1-based attempt number. Redeliveries keep the number. |
| `Redelivery` | `bool` | `true` when the outcome of an earlier delivery of this attempt is unknown. |
| `DeadlineMs` | `*int64` | Absolute deadline in Unix epoch milliseconds, when the step has a `timeout`. |
| `TimeoutMs` | `*int64` | The step's `timeout` in milliseconds. |

| Method | Meaning |
|---|---|
| `Decode(v any) error` | Unmarshals `Input` into `v`. |

## Cancellation and timeouts

The handler's context is cancelled when any of these happens:

- the runtime lists the attempt in a heartbeat's `cancel` list, because the
  attempt timed out or its workflow was cancelled or failed;
- the attempt's `DeadlineMs` passes (the context carries it as its deadline);
- the context passed to `Run` is cancelled.

Pass `ctx` to every call that accepts one:

```go
req, _ := http.NewRequestWithContext(ctx, http.MethodPost, url, body)
resp, err := client.Do(req)
if err != nil {
	return nil, orbit.Retryable("UPSTREAM", err.Error())
}
```

Returning `ctx.Err()` reports a retryable `UNHANDLED_EXCEPTION` with the
message `context canceled` or `context deadline exceeded`. Heartbeats run on
their own goroutine, so a long handler keeps its session alive.

## Concurrency

`Options.Concurrency` (default 4) is the number of poll goroutines, and
therefore the maximum number of tasks this worker runs at once. Handlers run
on those goroutines, so make them safe for concurrent use.

## Credentials and TLS

Every connection setting has an `Options` field and an environment variable.
A non-empty field wins.

| Field | Environment | Default | Meaning |
|---|---|---|---|
| `Server` | `ORBIT_SERVER` | `http://127.0.0.1:7233` | Runtime URL. |
| `Token` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | none | Bearer token. |
| `CACertFile` | `ORBIT_CA_CERT` | none | PEM CA certificate, added to the system pool. |
| `ClientCertFile` | `ORBIT_CLIENT_CERT` | none | PEM client certificate for mutual TLS. |
| `ClientKeyFile` | `ORBIT_CLIENT_KEY` | none | PEM private key for the client certificate. Required with it. |

```go
w := orbit.NewWorker(orbit.Options{
	WorkerID:       "payments-1",
	Server:         "https://orbit.internal:7233",
	CACertFile:     "/etc/orbit/ca.pem",
	ClientCertFile: "/etc/orbit/payments-1.pem",
	ClientKeyFile:  "/etc/orbit/payments-1.key",
})
```

`NewWorker` does not return an error. When a CA or client certificate file
cannot be read, it logs the problem and continues without it; the TLS
handshake then fails and the worker keeps retrying registration. Check the
log when a worker never registers. With a per-worker identity from a workers
file, set `WorkerID` to the identity's `id` and the token to the identity's
token.

## Logging

The SDK writes registration, warnings from the runtime, poll and delivery
failures, cancellations and ignored results to `Options.Logger`, a
`*log.Logger`. The default writes to standard error with the prefix
`[orbit] `. The SDK does not log task failures; log them in your handler if
you need to.

## Shutdown

`Run` blocks until its context is cancelled. It then aborts the open long
polls and cancels the context of each running handler. It waits for the
handlers to return, delivers their results with a context that outlives the
cancelled one (retrying for up to 10 minutes), unregisters, and returns nil.
Use `signal.NotifyContext` to tie shutdown to `SIGINT` and `SIGTERM`, as in
the example above.

A handler that ignores its context finishes its work, and its result is
delivered. A handler that returns early because its context was cancelled
reports a retryable `UNHANDLED_EXCEPTION`, and that failure counts as an
attempt. With the default retry policy of one attempt, the step then fails.
Give steps served by Go workers a retry policy with more than one attempt if
you restart workers while they run tasks.

## API reference

### `NewWorker`

```go
func NewWorker(opts Options) *Worker
```

### `Options`

| Field | Type | Default | Meaning |
|---|---|---|---|
| `WorkerID` | `string` | `go-<hostname>-<pid>` | Stable identity sent at registration. |
| `Server` | `string` | `ORBIT_SERVER`, then `http://127.0.0.1:7233` | Runtime URL. |
| `Token` | `string` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | Bearer token. |
| `CACertFile` | `string` | `ORBIT_CA_CERT` | PEM CA certificate path. |
| `ClientCertFile` | `string` | `ORBIT_CLIENT_CERT` | PEM client certificate path. |
| `ClientKeyFile` | `string` | `ORBIT_CLIENT_KEY` | PEM private key path. |
| `Concurrency` | `int` | `4` | Number of poll goroutines. |
| `Runtime` | `string` | `"go"` | Announced runtime, matched against `using <language>`. |
| `PollWait` | `time.Duration` | `20 * time.Second` | `wait_ms` of each long poll. The runtime caps it at 60 seconds. |
| `Logger` | `*log.Logger` | standard error, prefix `[orbit] ` | Destination of SDK messages. |

Zero values select the default.

### `Worker`

| Method | Meaning |
|---|---|
| `Task(name, version string, handler Handler) *Worker` | Registers a handler. Panics on a duplicate name. |
| `Run(ctx context.Context) error` | Registers, polls and blocks until `ctx` is cancelled. Returns an error when no task is registered, or an `*APIError` when the runtime refuses registration or `ctx` is cancelled before registration succeeds. Connection errors and 5xx responses during registration are retried. |

### `TaskError`

```go
type TaskError struct {
	Code      string
	Message   string
	Retryable bool
	Details   any
}
```

`Error()` returns `"<Code>: <Message>"`. See
[Returning task errors](#returning-task-errors).

### `APIError`

```go
type APIError struct {
	Status  int    // HTTP status; 0 when the runtime could not be reached
	Code    string // the runtime's error code, CONNECTION, or HTTP_ERROR
	Message string
}
```

### Other declarations

| Name | Meaning |
|---|---|
| `Task` | One delivery of a task. See [The task](#the-task). |
| `Handler` | `func(ctx context.Context, t *Task) (any, error)`. |
| `Version` | The SDK version, `"0.1.0"`. |

## Testing

Handlers are plain functions. Call them with a `*orbit.Task` whose `Input`
holds the JSON you want to test:

```go
func TestFrequencies(t *testing.T) {
	task := &orbit.Task{Input: json.RawMessage(`{"tokens":["a","b","a"],"top":1}`)}
	out, err := frequencies(context.Background(), task)
	if err != nil {
		t.Fatal(err)
	}
	got := out.(map[string]any)
	if got["unique"] != 2 {
		t.Fatalf("unique = %v", got["unique"])
	}
}
```

For an integration test, start `orbit server` with a temporary database,
deploy your workflow, run `w.Run(ctx)` on a goroutine, start executions
through the HTTP API, and cancel `ctx` when done. A short `PollWait` keeps
polls responsive. The SDK's own tests in `sdks/go/orbit_test.go` run the
worker against a scripted fake runtime built on `net/http/httptest`; run them
with `go test -race ./...`.

## See also

- [How workers work](overview.md)
- [Worker protocol](../reference/protocol.md)
- [Task contracts](../language/tasks.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Security](../runtime/security.md)
- [Polyglot workflows](../guides/polyglot.md)
