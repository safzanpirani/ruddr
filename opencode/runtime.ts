import { randomUUID } from "node:crypto";
import {
  BaseAdapter,
  InvalidParamsError,
  MethodNotFoundError,
  errorMessage,
  isRecord,
  optionalString,
  readLines,
  readTextInput,
  record,
  requiredString,
  type ProtocolMessage,
} from "../adapter/protocol";

interface OpenCodeThread {
  id: string;
  cwd: string;
  model?: string;
  executable: string;
  ephemeral: boolean;
  sandbox?: string;
}

interface OpenCodeTurn {
  id: string;
  interrupted: boolean;
  settling: boolean;
  steerGeneration: number;
  pendingSteers: Set<Promise<void>>;
}

type OpenCodeProcess = Bun.Subprocess<"pipe", "pipe", "inherit">;

const DEFAULT_HTTP_TIMEOUT_MS = 30_000;

export interface OpenCodeBackend {
  open(thread: OpenCodeThread, resumed: boolean): Promise<string>;
  prompt(sessionID: string, text: string, delivery?: "steer"): Promise<string>;
  wait(sessionID: string): Promise<OpenCodeSnapshot>;
  interrupt(sessionID: string): Promise<void>;
  close(sessionID?: string, removeSession?: boolean): Promise<void>;
}

export interface OpenCodeSnapshot {
  outcome?: string;
  messages: Array<Record<string, unknown>>;
  tokens?: { input?: number; output?: number; reasoning?: number; cache?: { read?: number } };
  cost?: number;
  contextWindow?: number;
}

export class OpenCodeRuddrAdapter extends BaseAdapter {
  private thread?: OpenCodeThread;
  private turn?: OpenCodeTurn;
  private completionTask?: Promise<void>;
  private emittedMessages = new Set<string>();

  constructor(
    emit: (message: ProtocolMessage) => void | Promise<void>,
    private readonly backend: OpenCodeBackend = new HTTPBackend(),
  ) {
    super(emit);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await this.backend.close(this.thread?.id, this.thread?.ephemeral);
    await Promise.race([
      this.completionTask ?? Promise.resolve(),
      new Promise<void>((resolve) => setTimeout(resolve, 1_000)),
    ]);
  }

  protected async dispatch(method: string, params: unknown): Promise<unknown> {
    switch (method) {
      case "initialize":
        this.initialized = true;
        return {
          serverInfo: { name: "ruddr-opencode2-adapter", version: "1" },
          capabilities: { experimentalApi: true },
        };
      case "initialized":
        return null;
      case "thread/start":
        return this.acquireThread(params, false);
      case "thread/resume":
        return this.acquireThread(params, true);
      case "turn/start":
        return this.startTurn(params);
      case "turn/steer":
        return this.steerTurn(params);
      case "turn/interrupt":
        return this.interruptTurn(params);
      default:
        throw new MethodNotFoundError(`method ${method} is not supported by the OpenCode 2 adapter`);
    }
  }

  private async acquireThread(params: unknown, resumed: boolean): Promise<unknown> {
    if (!this.initialized) throw new InvalidParamsError("initialize must run first");
    if (this.thread) throw new InvalidParamsError("a thread is already configured");
    const input = record(params, "thread parameters");
    const requestedID = resumed ? requiredString(input.threadId, "threadId") : "";
    const thread: OpenCodeThread = {
      id: requestedID,
      cwd: requiredString(input.cwd, "cwd"),
      executable: optionalString(input.providerPath) ?? "opencode2",
      ephemeral: input.ephemeral === true,
      sandbox: optionalString(input.sandbox),
      ...(optionalString(input.model) ? { model: optionalString(input.model) } : {}),
    };
    thread.id = await this.backend.open(thread, resumed);
    this.thread = thread;
    if (resumed) {
      const snapshot = await this.backend.wait(thread.id);
      for (const message of snapshot.messages) {
        const id = optionalString(message.id);
        if (id) this.emittedMessages.add(id);
      }
    }
    return { thread: { id: thread.id } };
  }

  private async startTurn(params: unknown): Promise<unknown> {
    if (!this.thread) throw new InvalidParamsError("thread/start or thread/resume must run first");
    if (this.turn) throw new InvalidParamsError("a turn is already active");
    const input = record(params, "turn parameters");
    this.requireThread(input);
    const id = await this.backend.prompt(this.thread.id, readTextInput(input.input));
    this.turn = {
      id,
      interrupted: false,
      settling: false,
      steerGeneration: 0,
      pendingSteers: new Set(),
    };
    await this.emit({
      method: "turn/started",
      params: { threadId: this.thread.id, turn: { id, status: "inProgress" } },
    });
    this.completionTask = this.completeWhenIdle(this.thread, this.turn);
    return { turn: { id, status: "inProgress" } };
  }

  private async steerTurn(params: unknown): Promise<unknown> {
    const input = record(params, "steer parameters");
    const turn = this.requireTurn(input, "expectedTurnId");
    let releasePending = () => {};
    const pending = new Promise<void>((resolve) => {
      releasePending = resolve;
    });
    turn.pendingSteers.add(pending);
    try {
      await this.backend.prompt(this.thread!.id, readTextInput(input.input), "steer");
      turn.steerGeneration++;
    } finally {
      turn.pendingSteers.delete(pending);
      releasePending();
    }
    return { turnId: turn.id };
  }

  private async interruptTurn(params: unknown): Promise<unknown> {
    const input = record(params, "interrupt parameters");
    const turn = this.requireTurn(input, "turnId");
    turn.interrupted = true;
    await this.backend.interrupt(this.thread!.id);
    return {};
  }

  private requireThread(input: Record<string, unknown>): void {
    if (requiredString(input.threadId, "threadId") !== this.thread?.id) {
      throw new InvalidParamsError("threadId does not match the configured OpenCode session");
    }
  }

  private requireTurn(input: Record<string, unknown>, turnKey: string): OpenCodeTurn {
    this.requireThread(input);
    if (!this.turn) throw new InvalidParamsError("there is no active OpenCode turn");
    if (this.turn.settling) throw new InvalidParamsError("the active OpenCode turn is settling");
    if (requiredString(input[turnKey], turnKey) !== this.turn.id) {
      throw new InvalidParamsError(`${turnKey} does not match the active OpenCode turn`);
    }
    return this.turn;
  }

  private async completeWhenIdle(thread: OpenCodeThread, turn: OpenCodeTurn): Promise<void> {
    try {
      let snapshot: OpenCodeSnapshot;
      while (true) {
        snapshot = await this.backend.wait(thread.id);
        if (this.turn !== turn) return;
        if (turn.pendingSteers.size > 0) {
          const steerGeneration = turn.steerGeneration;
          await Promise.all([...turn.pendingSteers]);
          if (this.turn !== turn) return;
          if (steerGeneration !== turn.steerGeneration) continue;
        }
        turn.settling = true;
        break;
      }
      await this.emitSnapshot(thread.id, snapshot);
      const status = turn.interrupted
        ? "interrupted"
        : snapshot.outcome === "failed"
          ? "failed"
          : snapshot.outcome === "interrupted"
            ? "interrupted"
            : "completed";
      await this.finishTurn(
        thread.id,
        turn,
        status,
        status === "failed" ? "OpenCode session failed" : undefined,
      );
    } catch (error) {
      if (this.turn === turn) {
        await this.finishTurn(thread.id, turn, turn.interrupted ? "interrupted" : "failed", errorMessage(error));
      }
    }
  }

  private async emitSnapshot(threadID: string, snapshot: OpenCodeSnapshot): Promise<void> {
    for (const message of snapshot.messages) {
      if (message.type !== "assistant") continue;
      const id = optionalString(message.id) ?? randomUUID();
      if (this.emittedMessages.has(id)) continue;
      this.emittedMessages.add(id);
      const content = Array.isArray(message.content) ? message.content.filter(isRecord) : [];
      for (const part of content) {
        if (part.type === "reasoning" && typeof part.text === "string" && part.text.trim()) {
          await this.emit({
            method: "item/completed",
            params: {
              threadId: threadID,
              item: {
                id: `${id}-reasoning`,
                type: "reasoning",
                status: "completed",
                summary: [{ type: "summary_text", text: part.text.trim() }],
              },
            },
          });
        }
        if (part.type === "tool") await this.emitTool(threadID, part);
      }
      const text = content
        .filter((part) => part.type === "text" && typeof part.text === "string")
        .map((part) => part.text as string)
        .join("\n")
        .trim();
      if (text) {
        await this.emit({
          method: "item/completed",
          params: {
            threadId: threadID,
            item: { id, type: "agentMessage", status: "completed", text, phase: "final_answer" },
          },
        });
      }
    }
    if (snapshot.tokens || snapshot.cost) {
      const input = number(snapshot.tokens?.input) + number(snapshot.tokens?.cache?.read);
      const output = number(snapshot.tokens?.output) + number(snapshot.tokens?.reasoning);
      await this.emit({
        method: "thread/tokenUsage/updated",
        params: {
          threadId: threadID,
          tokenUsage: {
            total: {
              inputTokens: input,
              cachedInputTokens: number(snapshot.tokens?.cache?.read),
              outputTokens: output,
              totalTokens: input + output,
            },
            ...(snapshot.contextWindow ? { modelContextWindow: snapshot.contextWindow } : {}),
          },
          costUsd: number(snapshot.cost),
        },
      });
    }
  }

  private async emitTool(threadID: string, part: Record<string, unknown>): Promise<void> {
    const state = isRecord(part.state) ? part.state : {};
    const toolID = optionalString(part.id) ?? randomUUID();
    const name = optionalString(part.name) ?? "tool";
    const input = isRecord(state.input) ? state.input : {};
    const output = textContent(state.output ?? state.error);
    const failed = state.status === "error";
    await this.emit({
      method: "item/started",
      params: {
        threadId: threadID,
        item: { id: toolID, type: "toolCall", status: "inProgress", toolName: name, command: toolCommand(name, input), input },
      },
    });
    await this.emit({
      method: "item/completed",
      params: {
        threadId: threadID,
        item: { id: toolID, type: "toolCall", status: failed ? "failed" : "completed", toolName: name, command: toolCommand(name, input), input, output },
      },
    });
  }

  private async finishTurn(
    threadID: string,
    turn: OpenCodeTurn,
    status: string,
    message?: string,
  ): Promise<void> {
    this.turn = undefined;
    await this.emit({
      method: "turn/completed",
      params: {
        threadId: threadID,
        turn: {
          id: turn.id,
          status,
          ...(message ? { error: { message } } : {}),
        },
      },
    });
  }
}

export class HTTPBackend implements OpenCodeBackend {
  private process?: OpenCodeProcess;
  private baseURL?: string;
  private password?: string;
  private pendingRequests = new Set<AbortController>();

  constructor(
    private readonly requestTimeoutMs = DEFAULT_HTTP_TIMEOUT_MS,
    private readonly fetcher: typeof fetch = fetch,
  ) {}

  async open(thread: OpenCodeThread, resumed: boolean): Promise<string> {
    const agent = ruddrAgent(thread.sandbox);
    await this.startServer(thread.executable, thread.cwd, thread.sandbox);
    if (resumed) {
      // TODO(review): Decide whether resumed OpenCode sessions should override their persisted model.
      await this.request(`/api/session/${encodeURIComponent(thread.id)}`);
      await this.request(`/api/session/${encodeURIComponent(thread.id)}/agent`, {
        method: "POST",
        body: { agent },
      });
      return thread.id;
    }
    const body: Record<string, unknown> = { location: { directory: thread.cwd }, agent };
    if (thread.model) body.model = parseModel(thread.model);
    const result = record(await this.request("/api/session", { method: "POST", body }), "session response");
    return requiredString(record(result.data, "session data").id, "session id");
  }

  async prompt(sessionID: string, text: string, delivery?: "steer"): Promise<string> {
    const result = record(
      await this.request(`/api/session/${encodeURIComponent(sessionID)}/prompt`, {
        method: "POST",
        body: { text, ...(delivery ? { delivery } : {}) },
      }),
      "prompt response",
    );
    return requiredString(record(result.data, "prompt data").id, "prompt id");
  }

  async wait(sessionID: string): Promise<OpenCodeSnapshot> {
    await this.request(`/api/session/${encodeURIComponent(sessionID)}/wait`, {
      method: "POST",
      timeoutMs: 0,
    });
    const result = record(
      await this.request(`/api/session/${encodeURIComponent(sessionID)}/export`),
      "export response",
    );
    const data = record(result.data, "export data");
    const info = record(data.info, "session info");
    return {
      outcome: optionalString(info.outcome),
      messages: Array.isArray(data.messages) ? data.messages.filter(isRecord) : [],
      tokens: isRecord(info.tokens) ? (info.tokens as OpenCodeSnapshot["tokens"]) : undefined,
      cost: number(info.cost),
    };
  }

  async interrupt(sessionID: string): Promise<void> {
    await this.request(`/api/session/${encodeURIComponent(sessionID)}/interrupt`, { method: "POST" });
  }

  async close(sessionID?: string, removeSession = false): Promise<void> {
    this.abortPendingRequests("OpenCode backend is closing");
    if (removeSession && sessionID && this.baseURL) {
      try {
        await this.request(`/api/session/${encodeURIComponent(sessionID)}`, {
          method: "DELETE",
          timeoutMs: 1_000,
        });
      } catch {}
    }
    this.abortPendingRequests("OpenCode backend is closing");
    const process = this.process;
    this.process = undefined;
    this.baseURL = undefined;
    this.password = undefined;
    if (!process) return;
    try {
      process.stdin.end();
    } catch {}
    const exited = await Promise.race([
      process.exited.then(() => true),
      new Promise<false>((resolve) => setTimeout(() => resolve(false), 1_000)),
    ]);
    if (!exited) {
      process.kill();
      await Promise.race([
        process.exited,
        new Promise<void>((resolve) => setTimeout(resolve, 1_000)),
      ]);
    }
  }

  private async startServer(executable: string, cwd: string, sandbox?: string): Promise<void> {
    this.password = `${randomUUID()}${randomUUID()}`;
    const childProcess = Bun.spawn([executable, "serve", "--stdio"], {
      cwd,
      stdin: "pipe",
      stdout: "pipe",
      stderr: "inherit",
      env: {
        ...processEnv(),
        OPENCODE_CONFIG_CONTENT: ruddrConfigContent(sandbox, process.env.OPENCODE_CONFIG_CONTENT),
        OPENCODE_SERVER_PASSWORD: this.password,
      },
    });
    this.process = childProcess;
    const iterator = readLines(childProcess.stdout)[Symbol.asyncIterator]();
    let announcementTimer: ReturnType<typeof setTimeout> | undefined;
    const first = await Promise.race([
      iterator.next(),
      new Promise<IteratorResult<string>>((_, reject) => {
        announcementTimer = setTimeout(
          () => reject(new Error("OpenCode 2 server did not report its URL")),
          10_000,
        );
      }),
    ]).finally(() => {
      if (announcementTimer) clearTimeout(announcementTimer);
    });
    if (first.done) throw new Error("OpenCode 2 server exited before reporting its URL");
    const info = record(JSON.parse(first.value), "OpenCode server announcement");
    this.baseURL = validatedLoopbackURL(requiredString(info.url, "OpenCode server URL"));
    void (async () => {
      while (!(await iterator.next()).done) {}
    })();
  }

  private async request(
    pathname: string,
    options: { method?: string; body?: unknown; timeoutMs?: number } = {},
  ): Promise<unknown> {
    if (!this.baseURL) throw new Error("OpenCode 2 server is not running");
    const controller = new AbortController();
    const timeoutMs = options.timeoutMs ?? this.requestTimeoutMs;
    const timeout = timeoutMs > 0
      ? setTimeout(
          () => controller.abort(new Error(`OpenCode API request timed out after ${timeoutMs}ms`)),
          timeoutMs,
        )
      : undefined;
    this.pendingRequests.add(controller);
    try {
      const response = await this.fetcher(`${this.baseURL}${pathname}`, {
        method: options.method ?? "GET",
        headers: {
          authorization: `Basic ${btoa(`opencode:${this.password ?? ""}`)}`,
          ...(options.body === undefined ? {} : { "content-type": "application/json" }),
        },
        body: options.body === undefined ? undefined : JSON.stringify(options.body),
        signal: controller.signal,
      });
      if (!response.ok) {
        const detail = (await response.text()).trim();
        throw new Error(`OpenCode API ${response.status}: ${detail || response.statusText}`);
      }
      if (response.status === 204) return null;
      return await response.json();
    } finally {
      if (timeout) clearTimeout(timeout);
      this.pendingRequests.delete(controller);
    }
  }

  private abortPendingRequests(message: string): void {
    for (const controller of this.pendingRequests) controller.abort(new Error(message));
  }
}

export function validatedLoopbackURL(value: string): string {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new InvalidParamsError("OpenCode server URL must be a valid URL");
  }
  const loopback =
    url.hostname === "127.0.0.1" ||
    url.hostname === "::1" ||
    url.hostname === "[::1]" ||
    url.hostname === "localhost";
  if ((url.protocol !== "http:" && url.protocol !== "https:") || !loopback) {
    throw new InvalidParamsError("OpenCode server URL must use HTTP on a loopback host");
  }
  if (url.username || url.password || url.search || url.hash || (url.pathname && url.pathname !== "/")) {
    throw new InvalidParamsError("OpenCode server URL must not contain credentials, a path, query, or fragment");
  }
  return url.origin;
}

const RUDDR_AGENTS: Record<string, string> = {
  "read-only": "ruddr-read-only",
  "workspace-write": "ruddr-workspace-write",
  "danger-full-access": "ruddr-danger-full-access",
};

function ruddrAgent(sandbox?: string): string {
  return RUDDR_AGENTS[sandbox ?? "workspace-write"] ?? RUDDR_AGENTS["workspace-write"]!;
}

export function ruddrConfigContent(sandbox?: string, existing?: string): string {
  // TODO(review): Define how Ruddr should isolate inherited OpenCode plugins and agents before changing config precedence.
  const configured = existing ? record(JSON.parse(existing), "OPENCODE_CONFIG_CONTENT") : {};
  const agents = isRecord(configured.agents) ? configured.agents : {};
  const readRules = ["read", "glob", "grep", "lsp", "webfetch", "websearch"].map((action) => ({
    action,
    resource: "*",
    effect: "allow",
  }));
  return JSON.stringify({
    ...configured,
    default_agent: ruddrAgent(sandbox),
    agents: {
      ...agents,
      "ruddr-read-only": {
        description: "Ruddr read-only agent",
        mode: "primary",
        permissions: [{ action: "*", resource: "*", effect: "deny" }, ...readRules],
      },
      "ruddr-workspace-write": {
        description: "Ruddr workspace-write agent",
        mode: "primary",
        permissions: [
          { action: "*", resource: "*", effect: "allow" },
          { action: "external_directory", resource: "*", effect: "deny" },
          { action: "read", resource: "*.env", effect: "deny" },
          { action: "read", resource: "*.env.*", effect: "deny" },
          { action: "read", resource: "*.env.example", effect: "allow" },
        ],
      },
      "ruddr-danger-full-access": {
        description: "Ruddr unrestricted agent",
        mode: "primary",
        permissions: [{ action: "*", resource: "*", effect: "allow" }],
      },
    },
  });
}

function parseModel(model: string): { providerID: string; id: string } {
  const slash = model.indexOf("/");
  if (slash <= 0 || slash === model.length - 1) {
    throw new InvalidParamsError("OpenCode models must use provider/model syntax");
  }
  return { providerID: model.slice(0, slash), id: model.slice(slash + 1) };
}

function processEnv(): Record<string, string | undefined> {
  return { ...process.env };
}

function number(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function textContent(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map(textContent).filter(Boolean).join("\n");
  if (isRecord(value)) return textContent(value.text ?? value.content ?? value.message);
  return "";
}

function toolCommand(name: string, input: Record<string, unknown>): string {
  const command = optionalString(input.command);
  return command ?? `${name} ${JSON.stringify(input)}`;
}
