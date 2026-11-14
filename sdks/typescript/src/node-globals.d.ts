// Minimal Node.js declarations so the SDK type-checks without @types/node.
declare const process: {
  env: Record<string, string | undefined>;
  pid: number;
  on(event: "SIGINT" | "SIGTERM", listener: () => void): void;
  off(event: "SIGINT" | "SIGTERM", listener: () => void): void;
};
