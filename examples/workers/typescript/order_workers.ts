// TypeScript workers for examples/order_processing.orbit: validation,
// inventory and notifications.
//
// Run:  node examples/workers/typescript/order_workers.ts
import { TaskError, Worker } from "../../../sdks/typescript/src/index.ts";

interface LineItem { sku: string; quantity: number }

const worker = new Worker({ workerId: process.env.ORBIT_WORKER_ID ?? "orders-typescript", concurrency: 8 });
const reservations = new Map<string, LineItem[]>();

worker.task<{ order_id: string; amount: number; items: LineItem[] }, { valid: boolean }>(
  "orders.validate",
  { version: "1.0.0" },
  async (input) => {
    if (input.items.length === 0) throw new TaskError("EMPTY_ORDER", "order has no items", { retryable: false });
    if (input.amount <= 0) throw new TaskError("INVALID_AMOUNT", "amount must be positive", { retryable: false });
    return { valid: true };
  },
);

worker.task<{ order_id: string; items: LineItem[] }, { reservation_id: string }>(
  "inventory.reserve",
  { version: "1.0.0" },
  async (input, ctx) => {
    const id = `res-${input.order_id}`;
    reservations.set(id, input.items);
    ctx.log("reserved", { reservation_id: id, skus: input.items.map((i) => i.sku) });
    return { reservation_id: id };
  },
);

worker.task<{ reservation_id: string }, { released: boolean }>("inventory.release", { version: "1.0.0" }, async (input, ctx) => {
  reservations.delete(input.reservation_id);
  ctx.log("released", { reservation_id: input.reservation_id });
  return { released: true };
});

worker.task<{ customer_id: string; message: string }, { delivered: boolean }>(
  "notifications.send",
  { version: "1.0.0" },
  async (input, ctx) => {
    ctx.log("notify", { customer_id: input.customer_id, message: input.message });
    return { delivered: true };
  },
);

await worker.run();
