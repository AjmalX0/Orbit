// Protocol-level tests for the TypeScript SDK against a scripted fake runtime.
//   cd sdks/typescript && npm test
import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { TaskError, Worker } from "../src/index.ts";

type Json = any;

function fakeRuntime(tasks: Json[]) {
  const state = {
    tasks: [...tasks],
    registrations: 0,
    forgetOnce: true,
    failComplete: 2,
    completed: new Map<string, Json>(),
    failed: new Map<string, Json>(),
    cancel: [] as string[],
    auth: "" as string | undefined,
  };
  const server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", async () => {
      const body = raw ? JSON.parse(raw) : {};
      state.auth = req.headers.authorization;
      const reply = (status: number, value?: Json) => {
        res.writeHead(status, { "content-type": "application/json" });
        res.end(value === undefined ? "" : JSON.stringify(value));
      };
      switch (req.url) {
        case "/v1/workers/register":
          state.registrations++;
          return reply(200, { accepted: true, worker_session_id: "s", heartbeat_interval_ms: 500 });
        case "/v1/workers/poll":
          if (state.forgetOnce) {
            state.forgetOnce = false;
            return reply(404, { error: { code: "UNKNOWN_SESSION", message: "gone" } });
          }
          if (state.tasks.length === 0) {
            await new Promise((r) => setTimeout(r, 20));
            return reply(204);
          }
          return reply(200, state.tasks.shift());
        case "/v1/workers/heartbeat":
          return reply(200, { accepted: true, cancel: state.cancel });
        case "/v1/tasks/complete":
          if (state.failComplete > 0) {
            state.failComplete--;
            return reply(503, { error: { code: "UNAVAILABLE", message: "later" } });
          }
          state.completed.set(body.token, body.output);
          return reply(200, { accepted: true });
        case "/v1/tasks/fail":
          state.failed.set(body.token, body);
          return reply(200, { accepted: true });
        default:
          return reply(200, { accepted: true });
      }
    });
  });
  return { state, server };
}

const task = (token: string, name: string, input: Json = {}) => ({
  token, execution_id: "e", step_id: token, task_name: name, contract_version: "1.0.0",
  input, idempotency_key: `e/${token}`, attempt: 1, redelivery: false,
});

test("worker protocol", async () => {
  const { state, server } = fakeRuntime([
    task("ok", "echo", { n: 1 }),
    task("biz", "fail"),
    task("boom", "crash"),
    task("slow", "slow"),
    task("bad", "array"),
  ]);
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
  const port = (server.address() as { port: number }).port;
  const quiet = { info() {}, warn() {}, error() {} };
  const w = new Worker({ server: `http://127.0.0.1:${port}`, token: "wt", workerId: "ts-test", concurrency: 2, pollWaitMs: 100, logger: quiet });
  w.task<{ n: number }, { n: number }>("echo", { version: "1.0.0" }, (input, ctx) => {
    assert.equal(ctx.idempotencyKey, "e/ok");
    return { n: input.n + 1 };
  });
  w.task("fail", { version: "1.0.0" }, () => {
    throw new TaskError("CARD_DECLINED", "no funds", { retryable: false });
  });
  w.task("crash", { version: "1.0.0" }, () => {
    throw new RangeError("kaboom");
  });
  w.task("slow", { version: "1.0.0" }, async (_input, ctx) => {
    state.cancel = [ctx.token];
    await new Promise((resolve) => ctx.signal.addEventListener("abort", resolve, { once: true }));
    throw new TaskError("CANCELLED", "stopped", { retryable: false });
  });
  w.task("array", { version: "1.0.0" }, () => [1, 2] as any);

  const origLog = console.log;
  console.log = () => {};
  const running = w.run({ handleSignals: false });
  const deadline = Date.now() + 15_000;
  while (!(state.completed.size === 1 && state.failed.size === 4) && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 20));
  }
  w.stop();
  await running;
  console.log = origLog;
  server.close();

  assert.deepEqual(state.completed.get("ok"), { n: 2 });
  assert.ok(state.registrations >= 2, "re-registers after UNKNOWN_SESSION");
  assert.equal(state.failed.get("biz").error_code, "CARD_DECLINED");
  assert.equal(state.failed.get("biz").retryable, false);
  assert.equal(state.failed.get("boom").error_code, "UNHANDLED_EXCEPTION");
  assert.equal(state.failed.get("boom").retryable, true);
  assert.match(state.failed.get("boom").error_message, /kaboom/);
  assert.equal(state.failed.get("slow").error_code, "CANCELLED");
  assert.equal(state.failed.get("bad").error_code, "INVALID_RESULT");
  assert.equal(state.auth, "Bearer wt");
});
