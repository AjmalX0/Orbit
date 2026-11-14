// TypeScript part of examples/polyglot.orbit: text.tokenize.
import { Worker } from "../../../sdks/typescript/src/index.ts";

const worker = new Worker({ workerId: "text-typescript" });
worker.task<{ text: string }, { tokens: string[] }>("text.tokenize", { version: "1.0.0" }, ({ text }) => ({
  tokens: text.split(" ").filter((t) => t.length > 0),
}));
await worker.run();
