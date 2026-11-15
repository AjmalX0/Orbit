# orbit-lang (TypeScript / JavaScript)

Worker SDK for the [ORBIT](../../README.md) runtime. No dependencies, built on
`fetch` and `AbortController`.

```bash
npm install orbit-lang
```

```ts
import { Worker, TaskError } from "orbit-lang";

const worker = new Worker({ workerId: "orders-worker", concurrency: 8 });

worker.task<{ order_id: string; items: { sku: string }[] }, { reservation_id: string }>(
  "inventory.reserve",
  { version: "1.0.0" },
  async (input, ctx) => {
    if (input.items.length === 0) throw new TaskError("EMPTY_ORDER", "no items", { retryable: false });
    // ctx.signal aborts when the runtime cancels the attempt (timeout / cancellation)
    return { reservation_id: `res-${input.order_id}` };
  },
);

await worker.run(); // SIGINT/SIGTERM stop polling, finish in-flight work, unregister
```

* `ctx`: `executionId`, `stepId`, `attempt`, `idempotencyKey`, `redelivery`,
  `deadlineMs`, `remainingMs()`, `signal`, `cancelled`, `log()`.
* Start workflows with `new Client().start("OrderProcessing", input)` and
  `client.wait(id)`.
* Development: Node.js 22.18+ runs `src/index.ts` directly. To publish, run
  `npm run build`, which emits `dist/` with type declarations.

## Credentials and TLS

| Variable | Purpose |
|---|---|
| `ORBIT_SERVER` | runtime URL (default `http://127.0.0.1:7233`) |
| `ORBIT_WORKER_TOKEN` (or `ORBIT_TOKEN`) | bearer token: a per-worker token from `orbit token`, or the shared worker token |
| `ORBIT_CA_CERT` | PEM CA to trust when the runtime uses a private certificate (Node.js: use `NODE_EXTRA_CA_CERTS`) |
| `ORBIT_CLIENT_CERT`, `ORBIT_CLIENT_KEY` | PEM client certificate and key for mutual TLS |
