import { randomUUID } from "node:crypto";
import { hostname } from "node:os";
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

type Sandbox = "read-only" | "workspace-write" | "danger-full-access";

interface DroidThread {
  id: string;
  cwd: string;
  executable: string;
  sandbox: Sandbox;
  model?: string;
  effort?: string;
}

interface DroidTurn {
  id: string;
  interrupted: boolean;
  settling: boolean;
  // add_user_message request IDs that Droid has accepted but not yet turned
  // into a user message. A steer sent near the end of a Droid turn is queued
  // and runs as a new Droid turn, so its agent_turn_completed must not end
  // the Ruddr turn.
  pendingSteers: Set<string>;
  deferredReason?: string;
  deferredTimer?: ReturnType<typeof setTimeout>;
  lastError?: string;
  // The latest text-only assistant message, held back so the last one of the
  // turn is reported as the final answer.
  pendingText?: Record<string, unknown>;
}

interface DroidTool {
  name: string;
  input: Record<string, unknown>;
  startedAt: number;
}

type DroidProcess = Bun.Subprocess<"pipe", "pipe", "inherit">;

const FACTORY_API_VERSION = "1.0.0";
const DEFAULT_RPC_TIMEOUT_MS = 30_000;
// Session start and load read settings, MCP configuration, and session files,
// so they get the same budget the Droid SDK uses.
const DEFAULT_START_TIMEOUT_MS = 60_000;
// How long a completed Droid turn waits for a queued steer to start the next
// Droid turn before Ruddr gives up on it and ends the turn.
const STEER_PICKUP_TIMEOUT_MS = 15_000;

// Ruddr's approval policy is always "never", so Droid rejects every
// permission request instead of waiting for an approver. The sandbox picks how
// much Droid may do without asking.
const AUTONOMY_LEVELS: Record<Sandbox, string> = {
  "read-only": "off",
  "workspace-write": "medium",
  "danger-full-access": "high",
};

export interface DroidClient {
  start(executable: string, cwd: string, onEvent: (params: Record<string, unknown>) => void): Promise<void>;
  request(
    method: string,
    params: Record<string, unknown>,
    options?: { id?: string; timeoutMs?: number },
  ): Promise<Record<string, unknown>>;
  close(): Promise<void>;
}

export class DroidRuddrAdapter extends BaseAdapter {
  private thread?: DroidThread;
  private turn?: DroidTurn;
  private tools = new Map<string, DroidTool>();
  private steerSequence = 0;
  private eventChain = Promise.resolve();
  private latestUsage?: Record<string, unknown>;
  private contextWindow = 0;
  private contextUsed?: number;

  constructor(
    emit: (message: ProtocolMessage) => void | Promise<void>,
    private readonly client: DroidClient = new SubprocessDroidClient(),
  ) {
    super(emit);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this.turn?.deferredTimer) clearTimeout(this.turn.deferredTimer);
    await this.client.close();
  }

  protected async dispatch(method: string, params: unknown): Promise<unknown> {
    switch (method) {
      case "initialize":
        this.initialized = true;
        return {
          serverInfo: { name: "ruddr-droid-adapter", version: "1" },
          capabilities: { experimentalApi: true },
        };
      case "initialized":
        return null;
      case "thread/start":
        return this.acquireThread(params, "start");
      case "thread/resume":
        return this.acquireThread(params, "resume");
      case "thread/fork":
        return this.acquireThread(params, "fork");
      case "turn/start":
        return this.startTurn(params);
      case "turn/steer":
        return this.steerTurn(params);
      case "turn/interrupt":
        return this.interruptTurn(params);
      default:
        throw new MethodNotFoundError(`method ${method} is not supported by the Droid adapter`);
    }
  }

  private async acquireThread(params: unknown, mode: "start" | "resume" | "fork"): Promise<unknown> {
    if (!this.initialized) throw new InvalidParamsError("initialize must run first");
    if (this.thread) throw new InvalidParamsError("a thread is already configured");
    const input = record(params, "thread parameters");
    if (mode === "start" && input.ephemeral === true) {
      throw new InvalidParamsError("Droid sessions always persist; --ephemeral is not supported");
    }
    if (mode === "fork" && (optionalString(input.beforeTurnId) || optionalString(input.lastTurnId))) {
      throw new InvalidParamsError("Droid forks copy the whole session; --fork-before-turn and --fork-through-turn are not supported");
    }
    const thread: DroidThread = {
      id: "",
      cwd: requiredString(input.cwd, "cwd"),
      executable: optionalString(input.providerPath) ?? "droid",
      sandbox: parseSandbox(input.sandbox),
      ...(optionalString(input.model) ? { model: optionalString(input.model) } : {}),
      ...(optionalString(input.effort) ? { effort: optionalString(input.effort) } : {}),
    };
    await this.client.start(thread.executable, thread.cwd, (event) => this.enqueueEvent(event));
    const settings = {
      ...(thread.model ? { modelId: thread.model } : {}),
      ...(thread.effort ? { reasoningEffort: thread.effort } : {}),
      autonomyLevel: AUTONOMY_LEVELS[thread.sandbox],
    };
    if (mode === "start") {
      const result = await this.client.request(
        "droid.initialize_session",
        { machineId: hostname(), cwd: thread.cwd, autoRejectPermissionRequests: true, ...settings },
        { timeoutMs: DEFAULT_START_TIMEOUT_MS },
      );
      thread.id = requiredString(result.sessionId, "Droid session id");
    } else {
      let sessionID = requiredString(input.threadId, "threadId");
      await this.loadSession(sessionID);
      if (mode === "fork") {
        // fork_session copies the loaded session but leaves this process on
        // the source, so the copy is loaded before any turn runs.
        const forked = await this.client.request("droid.fork_session", {});
        sessionID = requiredString(forked.newSessionId, "Droid fork session id");
        await this.loadSession(sessionID);
      }
      await this.client.request("droid.update_session_settings", settings);
      thread.id = sessionID;
    }
    this.thread = thread;
    return { thread: { id: thread.id } };
  }

  private async loadSession(sessionID: string): Promise<void> {
    await this.client.request(
      "droid.load_session",
      { sessionId: sessionID, autoRejectPermissionRequests: true },
      { timeoutMs: DEFAULT_START_TIMEOUT_MS },
    );
  }

  private async startTurn(params: unknown): Promise<unknown> {
    if (!this.thread) throw new InvalidParamsError("thread/start, thread/resume, or thread/fork must run first");
    if (this.turn) throw new InvalidParamsError("a turn is already active");
    const input = record(params, "turn parameters");
    this.requireThread(input);
    const text = readTextInput(input.input);
    const effort = optionalString(input.effort);
    if (effort && effort !== this.thread.effort) {
      await this.client.request("droid.update_session_settings", { reasoningEffort: effort });
      this.thread.effort = effort;
    }
    const turn: DroidTurn = {
      id: randomUUID(),
      interrupted: false,
      settling: false,
      pendingSteers: new Set(),
    };
    this.turn = turn;
    await this.emit({
      method: "turn/started",
      params: { threadId: this.thread.id, turn: { id: turn.id, status: "inProgress" } },
    });
    try {
      await this.client.request("droid.add_user_message", { text });
    } catch (error) {
      if (this.turn === turn) this.turn = undefined;
      throw error;
    }
    return { turn: { id: turn.id, status: "inProgress" } };
  }

  private async steerTurn(params: unknown): Promise<unknown> {
    const input = record(params, "steer parameters");
    const turn = this.requireTurn(input, "expectedTurnId");
    const text = readTextInput(input.input);
    const id = `ruddr-droid-steer-${++this.steerSequence}`;
    turn.pendingSteers.add(id);
    try {
      await this.client.request("droid.add_user_message", { text }, { id });
    } catch (error) {
      turn.pendingSteers.delete(id);
      this.enqueue(() => this.completeIfSteersSettled(turn));
      throw error;
    }
    await this.emitUserMessage(text);
    return { turnId: turn.id };
  }

  // Codex reports a steer as its own userMessage item, which is what puts the
  // steer in the transcript. Droid's own user-message echo carries no marker
  // that it came from a steer, so the adapter emits it once Droid accepts it.
  private async emitUserMessage(text: string): Promise<void> {
    if (!this.thread) return;
    await this.emit({
      method: "item/completed",
      params: {
        threadId: this.thread.id,
        item: { id: randomUUID(), type: "userMessage", status: "completed", text },
      },
    });
  }

  private async interruptTurn(params: unknown): Promise<unknown> {
    const input = record(params, "interrupt parameters");
    const turn = this.requireTurn(input, "turnId");
    turn.interrupted = true;
    await this.client.request("droid.interrupt_session", {});
    return {};
  }

  private requireThread(input: Record<string, unknown>): void {
    if (requiredString(input.threadId, "threadId") !== this.thread?.id) {
      throw new InvalidParamsError("threadId does not match the configured Droid session");
    }
  }

  private requireTurn(input: Record<string, unknown>, turnKey: string): DroidTurn {
    this.requireThread(input);
    if (!this.turn) throw new InvalidParamsError("there is no active Droid turn");
    if (this.turn.settling) throw new InvalidParamsError("the active Droid turn is settling");
    if (requiredString(input[turnKey], turnKey) !== this.turn.id) {
      throw new InvalidParamsError(`${turnKey} does not match the active Droid turn`);
    }
    return this.turn;
  }

  // Droid notifications are handled one at a time, in arrival order, so a
  // turn completion never overtakes the messages and tool results before it.
  private enqueueEvent(params: Record<string, unknown>): void {
    this.enqueue(() => this.handleEvent(params));
  }

  private enqueue(task: () => Promise<void>): void {
    this.eventChain = this.eventChain.then(task).catch(async (error) => {
      await this.emit({ method: "error", params: { error: { message: `Droid adapter: ${errorMessage(error)}` } } });
    });
  }

  private async handleEvent(params: Record<string, unknown>): Promise<void> {
    const notification = isRecord(params.notification) ? params.notification : undefined;
    if (!notification || !this.thread) return;
    // Subagent sessions report through the same stream; only the Ruddr
    // session's own events belong to the turn.
    const sessionID = optionalString(params.sessionId) ?? optionalString(notification.sessionId);
    if (sessionID && sessionID !== this.thread.id) return;
    if (notification.type === "session_token_usage_changed") {
      this.latestUsage = notification;
      this.contextUsed = undefined;
      await this.emitUsage();
      return;
    }
    const turn = this.turn;
    if (!turn || turn.settling) return;
    switch (notification.type) {
      case "assistant_text_delta": {
        const messageID = optionalString(notification.messageId);
        const delta = optionalString(notification.textDelta);
        if (!messageID || !delta) return;
        await this.emit({
          method: "item/agentMessage/delta",
          params: { threadId: this.thread.id, itemId: textItemID(messageID, notification.blockIndex), delta },
        });
        return;
      }
      case "create_message":
        if (!isRecord(notification.message)) return;
        if (notification.message.role === "assistant") {
          await this.handleAssistantMessage(turn, notification.message);
        } else if (notification.message.role === "user") {
          const requestID = optionalString(notification.requestId);
          if (requestID && turn.pendingSteers.delete(requestID) && turn.pendingSteers.size === 0) {
            // The steer started a new Droid turn; its completion ends the
            // Ruddr turn instead of the one that was deferred.
            clearDeferred(turn);
          }
        }
        return;
      case "tool_call":
        await this.handleToolCall(turn, notification);
        return;
      case "tool_result":
        await this.handleToolResult(notification);
        return;
      case "error": {
        const message = optionalString(notification.message) ?? "Droid reported an error";
        turn.lastError = message;
        await this.emit({ method: "error", params: { error: { message } } });
        return;
      }
      case "queued_messages_discarded":
        turn.pendingSteers.clear();
        await this.completeIfSteersSettled(turn);
        return;
      case "agent_turn_completed": {
        const reason = optionalString(notification.reason) ?? "completed";
        if (!turn.interrupted && turn.pendingSteers.size > 0) {
          clearDeferred(turn);
          turn.deferredReason = reason;
          turn.deferredTimer = setTimeout(() => {
            this.enqueue(async () => {
              turn.pendingSteers.clear();
              await this.completeIfSteersSettled(turn);
            });
          }, STEER_PICKUP_TIMEOUT_MS);
          return;
        }
        await this.completeTurn(turn, reason);
        return;
      }
      case "ruddr_error":
        turn.lastError = optionalString(notification.message) ?? "Droid process failed";
        await this.completeTurn(turn, "process_exit");
        return;
    }
  }

  private async completeIfSteersSettled(turn: DroidTurn): Promise<void> {
    if (this.turn !== turn || turn.pendingSteers.size > 0 || !turn.deferredReason) return;
    await this.completeTurn(turn, turn.deferredReason);
  }

  private async handleAssistantMessage(turn: DroidTurn, message: Record<string, unknown>): Promise<void> {
    if (!this.thread) return;
    const messageID = optionalString(message.id) ?? randomUUID();
    const content = Array.isArray(message.content) ? message.content : [];
    const usesTools = content.some((block) => isRecord(block) && block.type === "tool_use");
    for (let index = 0; index < content.length; index++) {
      const block = content[index];
      if (!isRecord(block)) continue;
      if (block.type === "thinking" && typeof block.thinking === "string" && block.thinking.trim()) {
        await this.flushText(turn, "commentary");
        await this.emit({
          method: "item/completed",
          params: {
            threadId: this.thread.id,
            item: {
              id: `${messageID}-thinking-${index}`,
              type: "reasoning",
              status: "completed",
              summary: [{ type: "summary_text", text: block.thinking.trim() }],
            },
          },
        });
      } else if (block.type === "text" && typeof block.text === "string" && block.text.trim()) {
        await this.flushText(turn, "commentary");
        const item = { id: textItemID(messageID, index), type: "agentMessage", status: "completed", text: block.text.trim() };
        // Text beside a tool call is narration; only a text-only message can
        // be the final answer.
        if (usesTools) await this.emitAgentMessage(item, "commentary");
        else turn.pendingText = item;
      }
    }
  }

  private async flushText(turn: DroidTurn, phase: "commentary" | "final_answer"): Promise<void> {
    const item = turn.pendingText;
    if (!item) return;
    turn.pendingText = undefined;
    await this.emitAgentMessage(item, phase);
  }

  private async emitAgentMessage(item: Record<string, unknown>, phase: "commentary" | "final_answer"): Promise<void> {
    if (!this.thread) return;
    await this.emit({ method: "item/completed", params: { threadId: this.thread.id, item: { ...item, phase } } });
  }

  private async handleToolCall(turn: DroidTurn, notification: Record<string, unknown>): Promise<void> {
    if (!this.thread || !isRecord(notification.toolUse)) return;
    const id = optionalString(notification.toolUse.id);
    if (!id) return;
    const input = isRecord(notification.toolUse.input) ? notification.toolUse.input : {};
    const existing = this.tools.get(id);
    // Droid streams a tool call's input: the same ID repeats as the
    // arguments fill in.
    if (existing) {
      existing.input = input;
      await this.emit({ method: "item/updated", params: { threadId: this.thread.id, item: toolItem(id, existing, "inProgress") } });
      return;
    }
    await this.flushText(turn, "commentary");
    const tool: DroidTool = { name: optionalString(notification.toolUse.name) ?? "tool", input, startedAt: Date.now() };
    this.tools.set(id, tool);
    await this.emit({ method: "item/started", params: { threadId: this.thread.id, item: toolItem(id, tool, "inProgress") } });
  }

  private async handleToolResult(notification: Record<string, unknown>): Promise<void> {
    if (!this.thread) return;
    const id = optionalString(notification.toolUseId);
    if (!id) return;
    const tool = this.tools.get(id);
    if (!tool) return;
    this.tools.delete(id);
    const status = notification.isError === true ? "failed" : "completed";
    await this.emit({
      method: "item/completed",
      params: { threadId: this.thread.id, item: toolItem(id, tool, status, textContent(notification.content)) },
    });
  }

  private async completeTurn(turn: DroidTurn, reason: string): Promise<void> {
    if (this.turn !== turn || !this.thread || turn.settling) return;
    turn.settling = true;
    clearDeferred(turn);
    const threadID = this.thread.id;
    await this.flushText(turn, "final_answer");
    try {
      const stats = await this.client.request("droid.get_context_stats", {});
      this.contextWindow = number(stats.limit) || this.contextWindow;
      if (typeof stats.used === "number" && Number.isFinite(stats.used)) this.contextUsed = stats.used;
      await this.emitUsage();
    } catch {}
    for (const [id, tool] of this.tools) {
      await this.emit({ method: "item/completed", params: { threadId: threadID, item: toolItem(id, tool, "failed") } });
    }
    this.tools.clear();
    const status = turn.interrupted || reason === "cancelled" ? "interrupted" : reason === "completed" ? "completed" : "failed";
    this.turn = undefined;
    await this.emit({
      method: "turn/completed",
      params: {
        threadId: threadID,
        turn: {
          id: turn.id,
          status,
          ...(status === "failed" ? { error: { message: turn.lastError ?? this.describeReason(reason) } } : {}),
        },
      },
    });
  }

  private describeReason(reason: string): string {
    if (reason === "permission_rejected" && this.thread) {
      return `Droid stopped at a tool call that needs approval; the ${this.thread.sandbox} sandbox runs Droid at autonomy ${AUTONOMY_LEVELS[this.thread.sandbox]}`;
    }
    return `Droid turn ended: ${reason}`;
  }

  private async emitUsage(): Promise<void> {
    if (!this.thread || !this.latestUsage) return;
    const tokens = isRecord(this.latestUsage.tokenUsage) ? this.latestUsage.tokenUsage : {};
    const lastCall = isRecord(this.latestUsage.lastCallTokenUsage) ? this.latestUsage.lastCallTokenUsage : undefined;
    const input = number(tokens.inputTokens);
    const output = number(tokens.outputTokens);
    const cacheRead = number(tokens.cacheReadTokens);
    const cacheCreation = number(tokens.cacheCreationTokens);
    const total = input + output + cacheRead + cacheCreation;
    if (total === 0) return;
    // Droid's inputTokens exclude cache reads; Codex counts them as input.
    const last = this.contextUsed ?? (lastCall
      ? number(lastCall.inputTokens) + number(lastCall.cacheReadTokens) + number(lastCall.outputTokens)
      : undefined);
    await this.emit({
      method: "thread/tokenUsage/updated",
      params: {
        threadId: this.thread.id,
        tokenUsage: {
          total: {
            inputTokens: input + cacheRead + cacheCreation,
            cachedInputTokens: cacheRead,
            outputTokens: output,
            totalTokens: total,
          },
          ...(last !== undefined ? { last: { totalTokens: last } } : {}),
          ...(this.contextWindow ? { modelContextWindow: this.contextWindow } : {}),
        },
      },
    });
  }
}

export class SubprocessDroidClient implements DroidClient {
  private process?: DroidProcess;
  private pending = new Map<string, {
    resolve: (value: Record<string, unknown>) => void;
    reject: (error: Error) => void;
    timeout: ReturnType<typeof setTimeout>;
  }>();
  private requestSequence = 0;
  private writeChain = Promise.resolve();
  private onEvent?: (params: Record<string, unknown>) => void;
  private closing = false;

  constructor(private readonly rpcTimeoutMs = DEFAULT_RPC_TIMEOUT_MS) {}

  async start(executable: string, cwd: string, onEvent: (params: Record<string, unknown>) => void): Promise<void> {
    this.onEvent = onEvent;
    // In stream JSON-RPC mode Droid takes its session settings from
    // initialize_session and update_session_settings, not from flags.
    const process = Bun.spawn(
      [executable, "exec", "--input-format", "stream-jsonrpc", "--output-format", "stream-jsonrpc"],
      {
        cwd,
        stdin: "pipe",
        stdout: "pipe",
        stderr: "inherit",
        env: { ...globalThis.process.env },
      },
    );
    this.process = process;
    void this.readOutput(process);
  }

  async request(
    method: string,
    params: Record<string, unknown>,
    options: { id?: string; timeoutMs?: number } = {},
  ): Promise<Record<string, unknown>> {
    const process = this.process;
    if (!process) throw new Error("Droid process is not running");
    const id = options.id ?? `ruddr-droid-${++this.requestSequence}`;
    const timeoutMs = options.timeoutMs ?? this.rpcTimeoutMs;
    const response = new Promise<Record<string, unknown>>((resolve, reject) => {
      const timeout = setTimeout(() => {
        const pending = this.pending.get(id);
        if (!pending) return;
        this.pending.delete(id);
        const error = new Error(`Droid ${method} timed out after ${timeoutMs}ms`);
        pending.reject(error);
        this.failProcess(process, error);
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timeout });
    });
    try {
      await this.write(
        process,
        { jsonrpc: "2.0", factoryApiVersion: FACTORY_API_VERSION, type: "request", id, method, params },
        timeoutMs,
      );
    } catch (error) {
      this.rejectPending(id, errorMessage(error));
      this.failProcess(process, error instanceof Error ? error : new Error(errorMessage(error)));
    }
    return await response;
  }

  async close(): Promise<void> {
    const process = this.process;
    this.closing = true;
    this.process = undefined;
    this.rejectAllPending("Droid client is closing");
    if (!process) return;
    try {
      process.stdin.end();
    } catch {}
    const exited = await Promise.race([
      process.exited.then(() => true),
      new Promise<false>((resolve) => setTimeout(() => resolve(false), 2_000)),
    ]);
    if (!exited) {
      process.kill();
      await Promise.race([
        process.exited,
        new Promise<void>((resolve) => setTimeout(resolve, 1_000)),
      ]);
    }
  }

  private async readOutput(process: DroidProcess): Promise<void> {
    try {
      for await (const line of readLines(process.stdout)) {
        if (!line.trim()) continue;
        const message = record(JSON.parse(line), "Droid JSON-RPC message");
        if (message.type === "response") {
          this.resolveResponse(message);
          continue;
        }
        if (message.type === "request") {
          void this.answerServerRequest(process, message);
          continue;
        }
        if (message.method === "droid.session_notification" && isRecord(message.params)) {
          this.onEvent?.(message.params);
        }
      }
      throw new Error("Droid output closed");
    } catch (error) {
      this.failProcess(process, error instanceof Error ? error : new Error(errorMessage(error)));
    }
  }

  private resolveResponse(message: Record<string, unknown>): void {
    const id = typeof message.id === "string" || typeof message.id === "number" ? String(message.id) : undefined;
    const pending = id === undefined ? undefined : this.pending.get(id);
    if (!id || !pending) return;
    this.pending.delete(id);
    clearTimeout(pending.timeout);
    if (isRecord(message.error)) {
      pending.reject(new Error(optionalString(message.error.message) ?? "Droid request failed"));
    } else {
      pending.resolve(isRecord(message.result) ? message.result : {});
    }
  }

  // Ruddr runs unattended, so Droid's interactive requests are declined at
  // once instead of being left to hang the turn.
  private async answerServerRequest(process: DroidProcess, message: Record<string, unknown>): Promise<void> {
    const reply: Record<string, unknown> = { jsonrpc: "2.0", factoryApiVersion: FACTORY_API_VERSION, type: "response", id: message.id };
    if (message.method === "droid.request_permission") {
      reply.result = { selectedOption: "cancel" };
    } else if (message.method === "droid.ask_user") {
      reply.result = { cancelled: true, answers: [] };
    } else {
      reply.error = { code: -32601, message: `Ruddr does not answer ${String(message.method)}` };
    }
    try {
      await this.write(process, reply);
    } catch (error) {
      this.failProcess(process, error instanceof Error ? error : new Error(errorMessage(error)));
    }
  }

  private async write(
    process: DroidProcess,
    message: Record<string, unknown>,
    timeoutMs = this.rpcTimeoutMs,
  ): Promise<void> {
    const operation = this.writeChain.then(async () => {
      if (this.process !== process) throw new Error("Droid process is not running");
      process.stdin.write(`${JSON.stringify(message)}\n`);
      await process.stdin.flush();
    });
    this.writeChain = operation.catch(() => {});
    let timeout: ReturnType<typeof setTimeout> | undefined;
    await Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timeout = setTimeout(
          () => reject(new Error(`Droid write timed out after ${timeoutMs}ms`)),
          timeoutMs,
        );
      }),
    ]).finally(() => {
      if (timeout) clearTimeout(timeout);
    });
  }

  private rejectPending(id: string, message: string): void {
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);
    clearTimeout(pending.timeout);
    pending.reject(new Error(message));
  }

  private rejectAllPending(message: string): void {
    for (const [id] of this.pending) this.rejectPending(id, message);
  }

  private failProcess(process: DroidProcess, error: Error): void {
    if (this.process !== process) return;
    this.process = undefined;
    process.kill();
    this.rejectAllPending(error.message);
    if (!this.closing) this.onEvent?.({ notification: { type: "ruddr_error", message: error.message } });
  }
}

function toolItem(id: string, tool: DroidTool, status: string, output?: string): Record<string, unknown> {
  const lower = tool.name.toLowerCase();
  const common = {
    id,
    status,
    toolName: tool.name,
    input: tool.input,
    durationMs: Math.max(0, Date.now() - tool.startedAt),
    ...(output ? { aggregatedOutput: output } : {}),
  };
  if (lower === "execute") {
    return { ...common, type: "commandExecution", command: optionalString(tool.input.command) ?? tool.name };
  }
  if (["create", "edit", "multiedit", "applypatch"].includes(lower)) {
    return { ...common, type: "fileChange", command: summarizeTool(tool.name, tool.input) };
  }
  if (lower === "websearch" || lower === "fetchurl") {
    return {
      ...common,
      type: "webSearch",
      query: optionalString(tool.input.query) ?? optionalString(tool.input.url),
      command: summarizeTool(tool.name, tool.input),
    };
  }
  return { ...common, type: "toolCall", command: summarizeTool(tool.name, tool.input) };
}

function summarizeTool(name: string, input: Record<string, unknown>): string {
  for (const key of ["command", "file_path", "path", "query", "url", "pattern", "description"]) {
    const value = optionalString(input[key]);
    if (value) return `${name} ${value}`;
  }
  const serialized = JSON.stringify(input);
  return serialized === "{}" ? name : `${name} ${serialized}`;
}

function textItemID(messageID: string, blockIndex: unknown): string {
  return `${messageID}-text-${typeof blockIndex === "number" ? blockIndex : 0}`;
}

function clearDeferred(turn: DroidTurn): void {
  if (turn.deferredTimer) clearTimeout(turn.deferredTimer);
  turn.deferredTimer = undefined;
  turn.deferredReason = undefined;
}

function parseSandbox(value: unknown): Sandbox {
  if (value === "read-only" || value === "workspace-write" || value === "danger-full-access") return value;
  throw new InvalidParamsError("sandbox must be read-only, workspace-write, or danger-full-access");
}

function textContent(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map(textContent).filter(Boolean).join("\n");
  if (isRecord(value)) return textContent(value.text ?? value.content);
  return "";
}

function number(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
