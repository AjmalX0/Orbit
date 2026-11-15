# Polyglot workflows

This guide runs one workflow whose four steps are served by workers in four
languages: Python normalizes a text, TypeScript tokenizes it, Go counts word
frequencies, and Rust computes a fingerprint. You see how contracts make the
languages interchangeable, how `using` routes each step to the right worker,
and what happens when a worker for one language is missing.

## Before you begin

Clone the ORBIT repository and run every command from its root:

```bash
git clone https://github.com/AjmalX0/Orbit.git
cd Orbit
```

You need `orbit` 0.1.0 or later, Python 3.9+, Node.js 22.18+, Go 1.21+ and
Rust 1.89+. The example workers build against the SDK sources in the
repository, so no packages need to be installed.

| File | Contents |
|---|---|
| `examples/polyglot.orbit` | Contracts and the `DocumentInsights` workflow |
| `examples/workers/polyglot/text_worker.py` | Python worker `text-python`: `text.normalize` |
| `examples/workers/polyglot/text_worker.ts` | TypeScript worker `text-typescript`: `text.tokenize` |
| `examples/workers/polyglot/go/main.go` | Go worker `text-go`: `text.frequencies` |
| `crates/orbit-worker/examples/text_worker.rs` | Rust worker `text-rust`: `text.fingerprint` |

## The contracts and the workflow

```orbit
schema WordCount {
    word: String
    count: Int
}

task "text.normalize" { version "1.0.0" input { text: String } output { text: String } }
task "text.tokenize" { version "1.0.0" input { text: String } output { tokens: List<String> } }
task "text.frequencies" { version "1.0.0" input { tokens: List<String>, top: Int } output { top: List<WordCount>, unique: Int } }
task "text.fingerprint" { version "1.0.0" input { text: String } output { hash: String, bytes: Int } }

workflow DocumentInsights {
    version "1.0.0"
    input {
        text: String
        top: Int = 3
    }
    output {
        words: Int
        unique: Int
        top_word: String?
        top: List<WordCount>
        fingerprint: String
    }

    step normalize { worker "text.normalize" using python with { text: input.text } }
    step tokenize { worker "text.tokenize" using typescript with { text: normalize.text } }
    step frequencies { worker "text.frequencies" using go with { tokens: tokenize.tokens, top: input.top } }
    step fingerprint { worker "text.fingerprint" using rust with { text: normalize.text } timeout 5s }

    return {
        words: len(tokenize.tokens)
        unique: frequencies.unique
        top_word: frequencies.top[0].word
        top: frequencies.top
        fingerprint: fingerprint.hash
    }
}
```

The contracts are the only thing the four languages share. Each one fixes the
JSON shape of a task's input and output, so a worker in any language can
implement it, and the compiler checks every step against it. In the
`return` block, `frequencies.top[0]` is an index into a list and may be
missing, so `top_word` is declared `String?`.

The dependencies come from the data each step reads:

```text
              normalize (python)
               │             │
               ▼             ▼
     tokenize (typescript)   fingerprint (rust)
               │
               ▼
     frequencies (go)
```

`fingerprint` reads only `normalize.text`, so it runs in parallel with
`tokenize` and `frequencies`. `orbit graph examples/polyglot.orbit` prints the
same graph as Mermaid.

```bash
orbit check examples/polyglot.orbit
```

```text
ok DocumentInsights@1.0.0 (4 steps)
```

## The workers

Each worker registers one contract by name and version. The handler receives
the task input as decoded JSON and returns the output.

### Python

```python
"""Python half of examples/polyglot.orbit: text.normalize."""
import re

from orbit_sdk import Worker

worker = Worker("text-python")


@worker.task("text.normalize", version="1.0.0")
def normalize(input, ctx):
    text = re.sub(r"[^\w\s']", " ", input["text"].lower())
    return {"text": re.sub(r"\s+", " ", text).strip()}


if __name__ == "__main__":
    worker.run()
```

### TypeScript

```ts
// TypeScript part of examples/polyglot.orbit: text.tokenize.
import { Worker } from "../../../sdks/typescript/src/index.ts";

const worker = new Worker({ workerId: "text-typescript" });
worker.task<{ text: string }, { tokens: string[] }>("text.tokenize", { version: "1.0.0" }, ({ text }) => ({
  tokens: text.split(" ").filter((t) => t.length > 0),
}));
await worker.run();
```

In your own project, import from the `orbit-lang` package instead of the
repository path.

### Go

```go
w := orbit.NewWorker(orbit.Options{WorkerID: "text-go"})
w.Task("text.frequencies", "1.0.0", func(ctx context.Context, t *orbit.Task) (any, error) {
	var in input
	if err := t.Decode(&in); err != nil {
		return nil, orbit.Permanent("INVALID_INPUT", err.Error())
	}
	counts := map[string]int{}
	for _, tok := range in.Tokens {
		counts[tok]++
	}
	// ... sort by count, then word, and keep the first in.Top entries ...
	return map[string]any{"top": top, "unique": len(counts)}, nil
})
ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
defer stop()
if err := w.Run(ctx); err != nil {
	log.Fatal(err)
}
```

`t.Decode` unmarshals the input into a struct with `json` tags. A decoding
error is returned as `orbit.Permanent`, so the runtime does not retry a task
whose input can never be decoded. The example's `go.mod` uses a `replace`
directive to build against `sdks/go` in the repository; in your own module,
`go get github.com/AjmalX0/Orbit/sdks/go@v0.1.0`.

### Rust

```rust
use orbit_worker::Worker;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Input {
    text: String,
}

/// 64-bit FNV-1a, a stable non-cryptographic fingerprint.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

fn main() {
    let mut worker = Worker::builder("text-rust").build();
    worker.task("text.fingerprint", "1.0.0", |input: Input, _ctx| {
        Ok(json!({ "hash": format!("{:016x}", fnv1a(input.text.as_bytes())), "bytes": input.text.len() }))
    });
    worker.run().expect("worker failed");
}
```

The handler's input type implements `Deserialize`. Input that does not
deserialize fails the attempt with a permanent `INVALID_INPUT` error.

## Run it

Start the runtime:

```bash
orbit server
```

Start each worker in its own terminal:

```bash
PYTHONPATH=sdks/python python3 examples/workers/polyglot/text_worker.py
```

```bash
node examples/workers/polyglot/text_worker.ts
```

```bash
(cd examples/workers/polyglot/go && go run .)
```

```bash
cargo run -p orbit-worker --example text_worker
```

Each worker prints a line such as `worker text-go registered (1 tasks)`.
Confirm that all four are online, each with its own runtime:

```bash
orbit workers list
```

```text
WORKER                       RUNTIME      STATE    INFLIGHT  TASKS
text-go                      go           online   0  text.frequencies@1.0.0
text-python                  python       online   0  text.normalize@1.0.0
text-rust                    rust         online   0  text.fingerprint@1.0.0
text-typescript              typescript   online   0  text.tokenize@1.0.0
```

Run the workflow:

```bash
orbit run examples/polyglot.orbit --id doc-1 --input '{"text": "The quick brown fox jumps over the lazy dog. The dog sleeps."}'
```

```text
deployed DocumentInsights@1.0.0 (sha256:f99d73185b54)
started execution doc-1
  ✓ normalize                      succeeded 9ms
  ✓ fingerprint                    succeeded 13ms
  ✓ tokenize                       succeeded 15ms
  ✓ frequencies                    succeeded 8ms

COMPLETED doc-1  DocumentInsights@1.0.0
  elapsed: 44ms
{
  "words": 12,
  "unique": 9,
  "top_word": "the",
  "top": [
    {
      "word": "the",
      "count": 3
    },
    {
      "word": "dog",
      "count": 2
    },
    {
      "word": "brown",
      "count": 1
    }
  ],
  "fingerprint": "be451c7eaa8fcee3"
}
```

`orbit status doc-1` shows which worker ran each step:

```text
  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  normalize                    SUCCEEDED          1      9ms  worker text-python
  fingerprint                  SUCCEEDED          1     13ms  worker text-rust
  tokenize                     SUCCEEDED          1     15ms  worker text-typescript
  frequencies                  SUCCEEDED          1      8ms  worker text-go
```

## How routing works

The runtime gives a task to a worker only when all of these match:

| Step declares | Worker announced |
|---|---|
| `worker "text.frequencies"` | the task name `text.frequencies` |
| the contract's `version "1.0.0"` | the contract version `1.0.0` |
| `using go` (optional) | the runtime `go` |

Every SDK announces its own language as the worker's runtime (`python`,
`typescript`, `go`, `rust`, `java`, `csharp`). Without `using`, any worker
that implements the contract can take the task, whatever its language. Use
`using` when a step depends on a language-specific implementation, or to keep
a migration between implementations explicit.

## When a worker is missing

Stop the Go worker and start another execution without waiting for it:

```bash
orbit run examples/polyglot.orbit --id doc-2 --no-wait --input '{"text": "To be, or not to be: that is the question."}'
orbit status doc-2
```

```text
RUNNING doc-2  DocumentInsights@1.0.0
  elapsed: 30ms

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  normalize                    SUCCEEDED          1      9ms  worker text-python
  fingerprint                  SUCCEEDED          1      7ms  worker text-rust
  tokenize                     SUCCEEDED          1     11ms  worker text-typescript
  frequencies                  SCHEDULED          1
```

The other branches finished. `frequencies` is `SCHEDULED`: it is queued and
waits for a worker that implements `text.frequencies@1.0.0` with runtime `go`.
The execution is durable, so it can wait indefinitely, across runtime
restarts. Start the Go worker again and follow the execution by running the
same command with the same id:

```bash
orbit run examples/polyglot.orbit --id doc-2 --input '{"text": "To be, or not to be: that is the question."}'
```

```text
up to date DocumentInsights@1.0.0 (sha256:f99d73185b54)
attached to existing execution doc-2
  ✓ normalize                      succeeded 9ms
  ✓ fingerprint                    succeeded 7ms
  ✓ tokenize                       succeeded 11ms
  ✓ frequencies                    succeeded 11ms

COMPLETED doc-2  DocumentInsights@1.0.0
  elapsed: 7.4s
{
  "words": 10,
  "unique": 8,
  "top_word": "be",
  "top": [
    {
      "word": "be",
      "count": 2
    },
    {
      "word": "to",
      "count": 2
    },
    {
      "word": "is",
      "count": 1
    }
  ],
  "fingerprint": "c9e05e87c1546d07"
}
```

If a step must not wait indefinitely, give it a `queue_timeout`. When no
worker picks the task up in time, the step fails with `QUEUE_TIMEOUT`, which
is never retried:

```orbit
step frequencies {
    worker "text.frequencies"
    using go
    with { tokens: tokenize.tokens, top: input.top }
    queue_timeout 2m
}
```

## Adding Java and C# workers

The same pattern works with the Java and C# SDKs. The repository contains
one-step examples for each:

| Workflow | Worker | Run the worker |
|---|---|---|
| `examples/java_hello.orbit` | `examples/workers/java/` | see `examples/workers/java/README.md` |
| `examples/csharp_hello.orbit` | `examples/workers/csharp/` | `dotnet run --project examples/workers/csharp` |

```bash
orbit run examples/csharp_hello.orbit --input '{"name": "Ada", "excitement": 2}'
orbit run examples/java_hello.orbit --input '{"name": "Ada"}'
```

## See also

- [How workers work](../sdks/overview.md)
- [Python](../sdks/python.md), [TypeScript and JavaScript](../sdks/typescript.md),
  [Go](../sdks/go.md), [Rust](../sdks/rust.md), [Java](../sdks/java.md),
  [C# and .NET](../sdks/csharp.md)
- [Task contracts](../language/tasks.md)
- [Worker protocol](../reference/protocol.md)
- [Fan-out and child workflows](fulfilment.md)
