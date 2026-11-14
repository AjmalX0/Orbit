# @orbit/sdk (TypeScript / JavaScript)

Worker SDK for the [ORBIT](../../README.md) runtime. No dependencies, built on
`fetch` and `AbortController`.

```ts
import { Worker, TaskError } from "@orbit/sdk";

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
