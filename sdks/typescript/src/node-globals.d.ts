// Minimal Node.js declarations so the SDK type-checks without @types/node.
declare const process: {
  env: Record<string, string | undefined>;
  pid: number;
  on(event: "SIGINT" | "SIGTERM", listener: () => void): void;
  off(event: "SIGINT" | "SIGTERM", listener: () => void): void;
};

// The subset of `node:https` / `node:fs` used for mutual TLS.
declare module "node:https" {
  interface IncomingMessage {
    statusCode?: number;
    setEncoding(enc: string): void;
    on(event: "data", cb: (chunk: string) => void): void;
    on(event: "end", cb: () => void): void;
    on(event: "error", cb: (e: Error) => void): void;
  }
  interface ClientRequest {
    on(event: "error", cb: (e: Error) => void): void;
    write(data: string): void;
    end(): void;
  }
  interface RequestOptions {
    method?: string;
    headers?: Record<string, string>;
    cert?: unknown;
    key?: unknown;
    ca?: unknown;
    signal?: AbortSignal;
  }
  export function request(url: string, options: RequestOptions, cb: (res: IncomingMessage) => void): ClientRequest;
}
declare module "node:fs" {
  export function readFileSync(path: string): unknown;
}
