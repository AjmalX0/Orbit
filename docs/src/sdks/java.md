# Java

The Java SDK, `io.github.ajmalx0:orbit-sdk`, runs ORBIT task handlers on the
JVM. It is built on `java.net.http` and Jackson. Handlers are typed with
Jackson, or work on raw `JsonNode` values. Each poll loop runs on its own
thread, cancellation interrupts the handler's thread, and the SDK handles
registration, long polling, heartbeats, local deadlines, result delivery
through runtime outages, and graceful shutdown. Read
[How workers work](overview.md) first for the concepts this page relies on.

## Install

The SDK requires Java 17 or later and depends on `jackson-databind`.

Maven:

```xml
<dependency>
  <groupId>io.github.ajmalx0</groupId>
  <artifactId>orbit-sdk</artifactId>
  <version>0.1.0</version>
</dependency>
```

Gradle:

```kotlin
implementation("io.github.ajmalx0:orbit-sdk:0.1.0")
```

## A minimal worker

This worker implements `greeting.java` from
[`examples/java_hello.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/java_hello.orbit):

```orbit
task "greeting.java" {
    version "1.0.0"
    input { name: String, excited: Bool }
    output { message: String, letters: Int }
}
```

```java
import io.github.ajmalx0.orbit.TaskException;
import io.github.ajmalx0.orbit.Worker;

public final class HelloWorker {
    record Input(String name, boolean excited) {}

    record Output(String message, int letters) {}

    public static void main(String[] args) {
        Worker worker = Worker.builder().workerId("hello-java").build();

        worker.task("greeting.java", "1.0.0", Input.class, (in, ctx) -> {
            String name = in.name() == null ? "" : in.name().strip();
            if (name.isEmpty()) {
                throw TaskException.permanent("INVALID_NAME", "name must not be blank");
            }
            int letters = (int) name.codePoints().filter(Character::isLetter).count();
            String message = "Hello, " + name + ", from Java" + (in.excited() ? "!" : ".");
            return new Output(message, letters);
        });

        Runtime.getRuntime().addShutdownHook(new Thread(worker::stop));
        worker.run();
    }
}
```

Run it against a local runtime. The repository's
[`examples/workers/java`](https://github.com/AjmalX0/Orbit/tree/main/examples/workers/java)
project contains this worker with a Maven build:

```bash
orbit server &
(cd examples/workers/java && mvn -q compile exec:java) &
orbit run examples/java_hello.orbit --input '{"name": "Ada", "excited": true}'
```

```json
{
  "message": "Hello, Ada, from Java!",
  "letters": 3
}
```

## Registering tasks

```java
<I, O> Worker task(String name, String version, Class<I> inputType, TaskHandler<I, O> handler)
<I, O> Worker task(String name, String version, TypeReference<I> inputType, TaskHandler<I, O> handler)
Worker rawTask(String name, String version, RawTaskHandler handler)
```

Each method registers a handler for `name@version` and returns the worker, so
calls can be chained. The name and version must match the `task` declaration
in your `.orbit` files exactly. Registering the same name twice throws
`IllegalArgumentException`, and registering after the worker started throws
`IllegalStateException`.

Use the `TypeReference` overload for generic input types:

```java
worker.task("orders.tally", "1.0.0", new TypeReference<Map<String, List<String>>>() {},
        (input, ctx) -> Map.of("count", input.size()));
```

## Handlers and results

`TaskHandler<I, O>` receives the decoded input and a [`TaskContext`](#taskcontext):

```java
@FunctionalInterface
public interface TaskHandler<I, O> {
    O handle(I input, TaskContext ctx) throws Exception;
}
```

`RawTaskHandler` receives the input as a Jackson `JsonNode` (`{}` when the
runtime sent none) and returns any value that serializes to a JSON object,
such as a `Map`, a record, or an `ObjectNode`:

```java
worker.rawTask("inventory.release", "1.0.0", (input, ctx) ->
        Map.of("released", input.path("reservation_id").asText()));
```

- The SDK decodes input with the worker's `ObjectMapper`. The default mapper
  ignores unknown properties. Input that cannot be decoded fails the attempt
  with `INVALID_INPUT`, which is not retried.
- Returning `null` reports `{}`.
- A result that does not serialize to a JSON object fails the attempt with
  `INVALID_RESULT`, which is not retried.
- The runtime validates the output against the contract's output schema; a
  mismatch fails the step with `OUTPUT_CONTRACT_VIOLATION`.

Jackson maps record components and fields by name, so name them as in the
contract (`order_id`), or configure the mapper with
`PropertyNamingStrategies.SNAKE_CASE` and pass it to `objectMapper(...)`.

## Throwing task errors

Throw `TaskException` to fail an attempt with a structured error:

```java
throw TaskException.permanent("CARD_DECLINED", "limit exceeded");
throw TaskException.retryable("BANK_UNAVAILABLE", "upstream timed out");
throw new TaskException("CARD_DECLINED", "limit exceeded", false, Map.of("limit", 5000));
```

| Constructor or factory | Retryable |
|---|---|
| `TaskException.retryable(String code, String message)` | yes |
| `TaskException.permanent(String code, String message)` | no, fails the step whatever its retry policy |
| `new TaskException(String code, String message, boolean retryable)` | as given |
| `new TaskException(String code, String message, boolean retryable, Object details)` | as given; `details` is serialized with the mapper |

`TaskException` is unchecked. Any other exception is reported as a retryable
`UNHANDLED_EXCEPTION` whose message is the exception's `toString()`, with
`details.exception` (the class name) and `details.stack_trace`. The SDK also
logs it at `ERROR`.

## TaskContext

| Method | Returns | Meaning |
|---|---|---|
| `token()` | `String` | Identifies this delivery. The SDK echoes it in the result. |
| `executionId()` | `String` | The workflow execution. |
| `workflow()` | `String` | Workflow name. |
| `workflowVersion()` | `String` | Workflow version. |
| `stepId()` | `String` | The step, or `compensate:<step>` for a compensation; `step[i]` for a fan-out element. |
| `taskName()` | `String` | The task name. |
| `contractVersion()` | `String` | The task's contract version. |
| `input()` | `JsonNode` | The raw input, `{}` when the runtime sent none. |
| `idempotencyKey()` | `String` | Stable across retries and redeliveries. Use it for side effects. |
| `attempt()` | `int` | 1-based attempt number. Redeliveries keep the number. |
| `redelivery()` | `boolean` | `true` when the outcome of an earlier delivery of this attempt is unknown. |
| `deadlineMs()` | `OptionalLong` | Absolute deadline in Unix epoch milliseconds, when the step has a `timeout`. |
| `timeoutMs()` | `OptionalLong` | The step's `timeout` in milliseconds. |
| `remainingMs()` | `OptionalLong` | Milliseconds left before the deadline, never negative. |
| `isCancelled()` | `boolean` | `true` once the attempt was cancelled. |
| `cancellation()` | `CompletableFuture<Void>` | Completes when the attempt is cancelled. |
| `throwIfCancelled()` | `void` | Throws a retryable `TaskException` with code `CANCELLED` if the attempt was cancelled. |

## Cancellation and timeouts

An attempt is cancelled when the runtime lists it in a heartbeat's `cancel`
list (the attempt timed out, or its workflow was cancelled or failed), or when
its `deadlineMs()` passes. Cancellation does three things:

- `isCancelled()` starts returning `true`;
- the `cancellation()` future completes;
- the thread running the handler is interrupted, so blocking calls such as
  `Thread.sleep`, `BlockingQueue.take` or `HttpClient.send` throw
  `InterruptedException`.

An `InterruptedException` that escapes the handler of a cancelled attempt is
reported as a retryable `CANCELLED`. Use `throwIfCancelled()` between units of
work, and `cancellation()` to abort a resource that does not respond to
interrupts:

```java
worker.task("reports.render", "1.0.0", Report.class, (report, ctx) -> {
    ctx.cancellation().thenRun(renderer::abort);
    for (Section s : report.sections()) {
        ctx.throwIfCancelled();
        renderer.render(s);
    }
    return Map.of("pages", report.sections().size());
});
```

Heartbeats run on their own thread and continue until every in-flight task
has finished, including during shutdown.

## Concurrency

`concurrency(n)` (default 4) sets the number of poll threads, named
`orbit-poll-<i>`, and therefore the maximum number of tasks this worker runs
at once. Handlers run on those threads, so make them thread-safe.

## Credentials and TLS

`Worker.builder()` falls back to the environment for every unset option.

| Builder method | Environment | Default | Meaning |
|---|---|---|---|
| `server(String)` | `ORBIT_SERVER` | `http://127.0.0.1:7233` | Runtime URL. |
| `token(String)` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | none | Bearer token. |
| `caCertFile(Path)` | `ORBIT_CA_CERT` | none | PEM CA certificates, trusted in addition to the system trust store. |
| `clientCert(Path certPem, Path keyPem)` | `ORBIT_CLIENT_CERT`, `ORBIT_CLIENT_KEY` | none | Client certificate chain (leaf first) and private key for mutual TLS. |
| `sslContext(SSLContext)` | | none | Your own TLS context. Replaces the CA and client certificate options. |

```java
Worker worker = Worker.builder()
        .workerId("payments-1")
        .server("https://orbit.internal:7233")
        .caCertFile(Path.of("/etc/orbit/ca.pem"))
        .clientCert(Path.of("/etc/orbit/payments-1.pem"), Path.of("/etc/orbit/payments-1.key"))
        .build();
```

The private key must be an unencrypted PKCS#8 PEM file (`BEGIN PRIVATE KEY`,
which OpenSSL 3 writes by default), RSA or EC. Convert other formats with
`openssl pkcs8 -topk8 -nocrypt -in old.key -out worker.key`. Setting only one
of the certificate and the key throws `IllegalArgumentException` from
`build()`, and unreadable TLS files throw `UncheckedIOException`. With a
per-worker identity from a workers file, set `workerId` to the identity's
`id` and the token to the identity's token.

## Logging

The SDK logs to `System.getLogger("orbit")`, which is `java.util.logging`
unless you bridge it, for example with SLF4J's `jdk-platform-logging`
binding. Pass `logger(System.Logger)` to use another logger. Registration,
runtime warnings, failures, cancellations and ignored results are logged at
`INFO` and above; each execution is logged at `DEBUG`. Messages logged from a
JVM shutdown hook can be dropped by `java.util.logging`'s own shutdown hook.

## Shutdown

`run()` blocks the calling thread until `stop()` is called. `start()` runs the
worker on a new thread named `orbit-worker` and returns it. `stop()` stops
polling (interrupting open long polls), lets running handlers finish and
deliver their results, unregisters, and blocks until all of that is done. It
is safe to call from a shutdown hook and more than once. `stop(Duration)`
waits at most the given time and returns whether the worker stopped.

```java
Runtime.getRuntime().addShutdownHook(new Thread(worker::stop));
worker.run();
```

The JVM runs shutdown hooks on `SIGINT` and `SIGTERM`, so this drains the
worker when the process is asked to stop.

## API reference

### `Worker`

| Member | Meaning |
|---|---|
| `static Builder builder()` | A builder; unset options fall back to the environment and defaults. |
| `static Worker create()` | A worker configured entirely from the environment and defaults. |
| `task(...)`, `rawTask(...)` | Register handlers. See [Registering tasks](#registering-tasks). |
| `void run()` | Registers, polls and blocks until stopped. Throws `IllegalStateException` when no task is registered or the worker already started, and `ApiException` when the runtime refuses registration. Connection errors and 5xx responses are retried. |
| `Thread start()` | Runs `run()` on a new thread. |
| `void stop()` | Stops gracefully and blocks until done. |
| `boolean stop(Duration timeout)` | Like `stop()`, waiting at most `timeout`. Throws `InterruptedException`. |
| `String workerId()` | The resolved worker id. |
| `String server()` | The resolved runtime URL. |
| `ObjectMapper objectMapper()` | The mapper used for input and output. |
| `VERSION` | The SDK version, `"0.1.0"`. |
| `DEFAULT_SERVER` | `"http://127.0.0.1:7233"`. |

### `Worker.Builder`

| Method | Default | Meaning |
|---|---|---|
| `workerId(String)` | `java-<host>-<pid>` (`HOSTNAME`, then the local host name) | Stable identity sent at registration. |
| `server(String)` | `ORBIT_SERVER`, then `http://127.0.0.1:7233` | Runtime URL. |
| `token(String)` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | Bearer token. |
| `caCertFile(Path)` | `ORBIT_CA_CERT` | PEM CA certificates. |
| `clientCert(Path, Path)` | `ORBIT_CLIENT_CERT`, `ORBIT_CLIENT_KEY` | Mutual TLS identity. |
| `sslContext(SSLContext)` | none | Custom TLS context. |
| `concurrency(int)` | `4` | Number of poll threads. |
| `runtime(String)` | `"java"` | Announced runtime, matched against `using <language>`. |
| `pollWait(Duration)` | 20 seconds | `wait_ms` of each long poll. The runtime caps it at 60 seconds. |
| `resultRetryTimeout(Duration)` | 10 minutes | How long to keep retrying a result through outages. |
| `objectMapper(ObjectMapper)` | an `ObjectMapper` that ignores unknown properties | Mapper for input and output. |
| `logger(System.Logger)` | `System.getLogger("orbit")` | Destination of SDK messages. |
| `build()` | | Builds the worker. |

### `TaskException`

Methods: `code()`, `errorMessage()` (the message without the code prefix),
`retryable()`, `details()`. `getMessage()` returns `"<code>: <message>"`. See
[Throwing task errors](#throwing-task-errors).

### `ApiException`

An unchecked exception for an error response or a connection failure.
Methods: `status()` (`0` for a connection failure), `code()` (the runtime's
error code, `CONNECTION`, or `HTTP_ERROR`), `isConnectionError()`.

### `TaskHandler<I, O>` and `RawTaskHandler`

Functional interfaces. See [Handlers and results](#handlers-and-results).

## Testing

Handlers are lambdas or classes, so you can test their logic directly. The
SDK constructs `TaskContext`, so keep the business logic in a method that
does not need it:

```java
static Output greet(Input in) { ... }

@Test
void greetsWithLetters() {
    assertEquals(3, HelloWorker.greet(new Input("Ada", true)).letters());
}
```

For an integration test, start `orbit server` with a temporary database,
deploy your workflow, call `worker.start()`, drive executions through the
HTTP API, and call `worker.stop()` in teardown. A short `pollWait` keeps polls
responsive. The SDK's own tests in `sdks/java/src/test` run the worker
against a scripted fake runtime built on the JDK's `HttpServer`, including a
mutual TLS test; run them with `mvn verify`.

## See also

- [How workers work](overview.md)
- [Worker protocol](../reference/protocol.md)
- [Task contracts](../language/tasks.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Security](../runtime/security.md)
- [Polyglot workflows](../guides/polyglot.md)
