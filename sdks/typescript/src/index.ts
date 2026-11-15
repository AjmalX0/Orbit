/**
 * ORBIT worker SDK for TypeScript / JavaScript.
 *
 * ```ts
 * import { Worker, TaskError } from "orbit-lang";
 *
 * const worker = new Worker({ workerId: "orders-worker" });
 * worker.task<{ order_id: string }, { valid: boolean }>("orders.validate", { version: "1.0.0" }, async (input, ctx) => {
 *   if (!input.order_id) throw new TaskError("INVALID", "order_id is required", { retryable: false });
 *   return { valid: true };
 * });
 * await worker.run();
 * ```
 *
 * Delivery is at-least-once: a task may be delivered again after a crash
 * (`ctx.redelivery`). Use `ctx.idempotencyKey`, which is stable across retries
 * and redeliveries, to make side effects idempotent.
 */

export const SDK_VERSION = "0.1.0";
const DEFAULT_SERVER = "http://127.0.0.1:7233";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** A task handed to this worker by the runtime (protocol `ExecuteTaskRequest`). */
export interface TaskAssignment {
  token: string;
  execution_id: string;
  workflow: string;
  workflow_version: string;
  step_id: string;
  task_name: string;
  contract_version: string;
  input: Json;
  idempotency_key: string;
  attempt: number;
  redelivery: boolean;
  deadline_ms?: number;
  timeout_ms?: number;
}

/** Throw from a handler to fail the attempt with a structured error. */
export class TaskError extends Error {
  readonly code: string;
  readonly retryable: boolean;
  readonly details?: Json;

  constructor(code: string, message = "", options: { retryable?: boolean; details?: Json } = {}) {
    super(`${code}: ${message}`);
    this.name = "TaskError";
    this.code = code;
    this.retryable = options.retryable ?? true;
    this.details = options.details;
    this.message = message;
  }
}

export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(`${code} (${status}): ${message}`);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
  }

  get isConnectionError(): boolean {
    return this.status === 0;
  }
}

export interface ClientOptions {
  server?: string;
  token?: string;
  /** PEM client certificate and key paths for mutual TLS (or ORBIT_CLIENT_CERT / ORBIT_CLIENT_KEY). */
  clientCert?: string;
  clientKey?: string;
}

/** Minimal client for the ORBIT runtime HTTP API. */
export class Client {
  readonly server: string;
  private readonly token?: string;

  constructor(options: ClientOptions = {}) {
    this.server = (options.server ?? process.env.ORBIT_SERVER ?? DEFAULT_SERVER).replace(/\/+$/, "");
    // Workers should use the restricted worker credential when the runtime has one.
    // For a private CA over HTTPS, Node.js honours NODE_EXTRA_CA_CERTS=/path/ca.pem.
    this.token = options.token ?? (process.env.ORBIT_WORKER_TOKEN || process.env.ORBIT_TOKEN);
    this.clientCert = options.clientCert ?? process.env.ORBIT_CLIENT_CERT;
    this.clientKey = options.clientKey ?? process.env.ORBIT_CLIENT_KEY;
  }

  private readonly clientCert?: string;
  private readonly clientKey?: string;

  /**
   * Mutual-TLS transport. The global `fetch` cannot present client
   * certificates, so requests go through `node:https` when one is configured.
   */
  private async httpsRequest(
    method: string,
    url: string,
    headers: Record<string, string>,
    body: string | undefined,
    signal?: AbortSignal,
  ): Promise<{ status: number; text: string }> {
    const https = await import("node:https");
    const fs = await import("node:fs");
    const cert = fs.readFileSync(this.clientCert as string);
    const key = fs.readFileSync((this.clientKey ?? this.clientCert) as string);
    const ca = process.env.ORBIT_CA_CERT ? fs.readFileSync(process.env.ORBIT_CA_CERT) : undefined;
    return new Promise((resolve, reject) => {
      const req = https.request(url, { method, headers, cert, key, ca, signal }, (res) => {
        let text = "";
        res.setEncoding("utf8");
        res.on("data", (chunk: string) => (text += chunk));
        res.on("end", () => resolve({ status: res.statusCode ?? 0, text }));
        res.on("error", reject);
      });
      req.on("error", reject);
      if (body !== undefined) req.write(body);
      req.end();
    });
  }

  async request<T = any>(method: string, path: string, body?: unknown, signal?: AbortSignal): Promise<{ status: number; body: T }> {
    const headers: Record<string, string> = { "content-type": "application/json", accept: "application/json" };
    if (this.token) headers.authorization = `Bearer ${this.token}`;
    const payload = body === undefined ? undefined : JSON.stringify(body);
    let status: number;
    let text: string;
    try {
      if (this.clientCert && this.server.startsWith("https:")) {
        ({ status, text } = await this.httpsRequest(method, this.server + path, headers, payload, signal));
      } else {
        const res = await fetch(this.server + path, { method, headers, body: payload, signal });
        status = res.status;
        text = await res.text();
      }
    } catch (e) {
      throw new ApiError(0, "CONNECTION", `cannot reach ORBIT runtime at ${this.server}: ${(e as Error).message}`);
    }
    const parsed = text ? JSON.parse(text) : null;
    if (status >= 400) {
      const err = parsed?.error ?? {};
      throw new ApiError(status, err.code ?? "HTTP_ERROR", err.message ?? text);
    }
    return { status, body: parsed as T };
  }

  /** Start a workflow execution and return its id. */
  async start(workflow: string, input: Json = {}, options: { version?: string; executionId?: string } = {}): Promise<string> {
    const r = await this.request<{ execution_id: string }>("POST", "/v1/executions", {
      workflow,
      input,
      version: options.version,
      execution_id: options.executionId,
    });
    return r.body.execution_id;
  }

  /** Long-poll until the execution finishes (or `timeoutMs` elapses). */
  async wait(executionId: string, timeoutMs = 30_000): Promise<any> {
    return (await this.request("GET", `/v1/executions/${encodeURIComponent(executionId)}/wait?timeout_ms=${timeoutMs}`)).body;
  }
}

/** Information about the attempt being executed. */
export class Context {
  readonly token: string;
  readonly executionId: string;
  readonly workflow: string;
  readonly workflowVersion: string;
  readonly stepId: string;
  readonly taskName: string;
  readonly attempt: number;
  readonly idempotencyKey: string;
  readonly redelivery: boolean;
  readonly deadlineMs?: number;
  /** Aborted when the runtime asks this attempt to stop (timeout or cancellation). */
  readonly signal: AbortSignal;
  private readonly controller: AbortController;

  constructor(task: TaskAssignment) {
    this.token = task.token;
    this.executionId = task.execution_id;
    this.workflow = task.workflow;
    this.workflowVersion = task.workflow_version;
    this.stepId = task.step_id;
    this.taskName = task.task_name;
    this.attempt = task.attempt;
    this.idempotencyKey = task.idempotency_key;
    this.redelivery = task.redelivery;
    this.deadlineMs = task.deadline_ms;
    this.controller = new AbortController();
    this.signal = this.controller.signal;
  }

  get cancelled(): boolean {
    return this.signal.aborted;
  }

  /** @internal */
  cancel(): void {
    this.controller.abort(new TaskError("CANCELLED", "attempt cancelled by the runtime", { retryable: false }));
  }

  remainingMs(): number | undefined {
    return this.deadlineMs === undefined ? undefined : Math.max(0, this.deadlineMs - Date.now());
  }

  log(message: string, fields: Record<string, unknown> = {}): void {
    console.log(JSON.stringify({ level: "info", message, execution_id: this.executionId, step: this.stepId, attempt: this.attempt, ...fields }));
  }
}

export type Handler<I = any, O = any> = (input: I, ctx: Context) => O | Promise<O>;

export interface WorkerOptions extends ClientOptions {
  workerId?: string;
  /** Maximum tasks executed at the same time. */
  concurrency?: number;
  /** Announced runtime, matched against `using <language>` in workflows. */
  runtime?: string;
  pollWaitMs?: number;
  logger?: Pick<Console, "info" | "warn" | "error">;
}

interface RegisteredTask {
  name: string;
  version: string;
  handler: Handler;
}

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

export class Worker {
  readonly workerId: string;
  readonly client: Client;
  private readonly concurrency: number;
  private readonly runtime: string;
  private readonly pollWaitMs: number;
  private readonly logger: Pick<Console, "info" | "warn" | "error">;
  private readonly tasks = new Map<string, RegisteredTask>();
  private readonly active = new Map<string, Context>();
  private session?: string;
  private registering?: Promise<string>;
  private heartbeatMs = 5_000;
  private stopped = false;
  private readonly stopController = new AbortController();

  constructor(options: WorkerOptions = {}) {
    this.workerId = options.workerId ?? `typescript-${process.pid}`;
    this.client = new Client(options);
    this.concurrency = Math.max(1, options.concurrency ?? 4);
    this.runtime = options.runtime ?? "typescript";
    this.pollWaitMs = options.pollWaitMs ?? 20_000;
    this.logger = options.logger ?? console;
  }

  /** Register `handler` as the implementation of `name@version`. */
  task<I = any, O = any>(name: string, options: { version?: string }, handler: Handler<I, O>): this {
    if (this.tasks.has(name)) throw new Error(`task ${name} is already registered`);
    this.tasks.set(name, { name, version: options.version ?? "1.0.0", handler: handler as Handler });
    return this;
  }

  private async register(): Promise<string> {
    if (!this.registering) {
      this.registering = (async () => {
        const r = await this.client.request("POST", "/v1/workers/register", {
          worker_id: this.workerId,
          runtime: this.runtime,
          sdk: `orbit-typescript/${SDK_VERSION}`,
          contracts: [...this.tasks.values()].map((t) => ({ task_name: t.name, contract_version: t.version })),
        });
        this.session = r.body.worker_session_id as string;
        this.heartbeatMs = Math.max(500, r.body.heartbeat_interval_ms ?? 5_000);
        for (const w of r.body.warnings ?? []) this.logger.warn(`[orbit] runtime: ${w}`);
        this.logger.info(`[orbit] worker ${this.workerId} registered (${this.tasks.size} tasks)`);
        return this.session;
      })().finally(() => {
        this.registering = undefined;
      });
    }
    return this.registering;
  }

  /** Call `fn(session)`, re-registering if the runtime forgot the session. */
  private async withSession<T>(fn: (session: string) => Promise<T>): Promise<T> {
    const session = this.session ?? (await this.register());
    try {
      return await fn(session);
    } catch (e) {
      if (e instanceof ApiError && e.status === 404 && e.code === "UNKNOWN_SESSION") {
        if (this.session === session) this.session = undefined;
        return fn(this.session ?? (await this.register()));
      }
      throw e;
    }
  }

  private async retrying<T>(fn: () => Promise<T>): Promise<T | undefined> {
    let delay = 200;
    for (;;) {
      try {
        return await fn();
      } catch (e) {
        if (!(e instanceof ApiError) || (!e.isConnectionError && e.status < 500)) throw e;
        if (this.stopped) {
          this.logger.warn(`[orbit] ${e.message}; giving up because the worker is stopping`);
          return undefined;
        }
        this.logger.warn(`[orbit] ${e.message}; retrying in ${delay}ms`);
        await sleep(delay);
        delay = Math.min(delay * 2, 5_000) * (0.8 + Math.random() * 0.4);
      }
    }
  }

  /** Run until `stop()` is called or the process receives SIGINT/SIGTERM. */
  async run(options: { handleSignals?: boolean } = {}): Promise<void> {
    if (this.tasks.size === 0) throw new Error("no tasks registered");
    const onSignal = () => this.stop();
    if (options.handleSignals ?? true) {
      process.on("SIGINT", onSignal);
      process.on("SIGTERM", onSignal);
    }
    await this.retrying(() => this.register());
    const loops = [this.heartbeatLoop(), ...Array.from({ length: this.concurrency }, () => this.pollLoop())];
    await Promise.all(loops);
    if (options.handleSignals ?? true) {
      process.off("SIGINT", onSignal);
      process.off("SIGTERM", onSignal);
    }
    if (this.session) {
      await this.client.request("POST", "/v1/workers/unregister", { worker_session_id: this.session }).catch(() => undefined);
    }
    this.logger.info(`[orbit] worker ${this.workerId} stopped`);
  }

  stop(): void {
    this.stopped = true;
    this.stopController.abort();
  }

  private async heartbeatLoop(): Promise<void> {
    while (!this.stopped) {
      await Promise.race([sleep(this.heartbeatMs), new Promise((r) => this.stopController.signal.addEventListener("abort", r, { once: true }))]);
      if (this.stopped) return;
      try {
        const r = await this.withSession((s) => this.client.request("POST", "/v1/workers/heartbeat", { worker_session_id: s }));
        for (const token of r.body.cancel ?? []) {
          const ctx = this.active.get(token);
          if (ctx) {
            ctx.log("cancellation requested by runtime");
            ctx.cancel();
          }
        }
      } catch {
        /* the poll loops report connectivity problems */
      }
    }
  }

  private async pollLoop(): Promise<void> {
    let backoff = 200;
    while (!this.stopped) {
      let res: { status: number; body: TaskAssignment | null };
      try {
        res = await this.withSession((s) =>
          this.client.request<TaskAssignment | null>(
            "POST",
            "/v1/workers/poll",
            { worker_session_id: s, wait_ms: this.pollWaitMs },
            this.stopController.signal,
          ),
        );
        backoff = 200;
      } catch (e) {
        if (this.stopped) return;
        this.logger.warn(`[orbit] poll failed: ${(e as Error).message}; retrying in ${backoff}ms`);
        await sleep(backoff);
        backoff = Math.min(backoff * 2, 5_000);
        continue;
      }
      if (res.status === 204 || !res.body) continue;
      await this.execute(res.body);
    }
  }

  private async execute(task: TaskAssignment): Promise<void> {
    const ctx = new Context(task);
    this.active.set(ctx.token, ctx);
    try {
      const impl = this.tasks.get(task.task_name);
      if (!impl) throw new TaskError("UNKNOWN_TASK", `worker does not implement ${task.task_name}`);
      ctx.log("executing", { task: ctx.taskName, redelivery: ctx.redelivery });
      let output = await impl.handler(task.input ?? {}, ctx);
      if (output === undefined || output === null) output = {};
      if (typeof output !== "object" || Array.isArray(output)) {
        throw new TaskError("INVALID_RESULT", "tasks must return an object", { retryable: false });
      }
      await this.report("/v1/tasks/complete", { token: ctx.token, output });
    } catch (e) {
      const err =
        e instanceof TaskError
          ? e
          : new TaskError("UNHANDLED_EXCEPTION", e instanceof Error ? `${e.name}: ${e.message}` : String(e), {
              retryable: true,
              details: e instanceof Error && e.stack ? { stack: e.stack.split("\n").slice(0, 8).join("\n") } : undefined,
            });
      if (!(e instanceof TaskError)) this.logger.error(`[orbit] task ${task.task_name} threw`, e);
      await this.report("/v1/tasks/fail", {
        token: ctx.token,
        error_code: err.code,
        error_message: err.message,
        retryable: err.retryable,
        details: err.details ?? null,
      });
    } finally {
      this.active.delete(ctx.token);
    }
  }

  /** Deliver a result, retrying through outages so it survives a runtime restart. */
  private async report(path: string, body: Record<string, unknown>): Promise<void> {
    const r = await this.retrying(() => this.client.request("POST", path, body));
    if (r && r.body && r.body.accepted === false) {
      this.logger.info(`[orbit] runtime ignored result: ${r.body.reason}`);
    }
  }
}
