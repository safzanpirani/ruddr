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

interface PiThread {
  id: string;
  cwd: string;
  model?: string;
  effort?: string;
  executable: string;
  sandbox: string;
  ephemeral: boolean;
  resumed: boolean;
}

interface PiTurn {
  id: string;
  interrupted: boolean;
  settling: boolean;
  steerGeneration: number;
  pendingSteers: Set<Promise<void>>;
  assistantMessages: Array<Record<string, unknown>>;
}

type PiProcess = Bun.Subprocess<"pipe", "pipe", "inherit">;

const DEFAULT_RPC_TIMEOUT_MS = 30_000;
// Launching the Pi binary is far slower and more variable than a steady-state
// RPC, so the startup handshake gets its own budget.
const DEFAULT_START_TIMEOUT_MS = 60_000;
const FIRE_AND_FORGET_UI_METHODS = new Set([
  "notify",
  "setStatus",
  "setWidget",
  "setTitle",
  "set_editor_text",
]);

export interface PiClient {
  start(config: PiThread, onEvent: (event: Record<string, unknown>) => void): Promise<string>;
  send(command: Record<string, unknown>, timeoutMs?: number): Promise<Record<string, unknown>>;
  close(): Promise<void>;
}

export class PiRuddrAdapter extends BaseAdapter {
  private thread?: PiThread;
  private turn?: PiTurn;
  private tools = new Map<string, Record<string, unknown>>();

  constructor(
    emit: (message: ProtocolMessage) => void | Promise<void>,
    private readonly client: PiClient = new SubprocessPiClient(),
  ) {
    super(emit);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await this.client.close();
  }

  protected async dispatch(method: string, params: unknown): Promise<unknown> {
    switch (method) {
      case "initialize":
        this.initialized = true;
        return {
          serverInfo: { name: "ruddr-pi-adapter", version: "1" },
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
        throw new MethodNotFoundError(`method ${method} is not supported by the Pi adapter`);
    }
  }

  private async acquireThread(params: unknown, resumed: boolean): Promise<unknown> {
    if (!this.initialized) throw new InvalidParamsError("initialize must run first");
    if (this.thread) throw new InvalidParamsError("a thread is already configured");
    const input = record(params, "thread parameters");
    const thread: PiThread = {
      id: resumed ? requiredString(input.threadId, "threadId") : randomUUID(),
      cwd: requiredString(input.cwd, "cwd"),
      executable: optionalString(input.providerPath) ?? "pi",
      sandbox: requiredString(input.sandbox, "sandbox"),
      ephemeral: input.ephemeral === true,
      resumed,
      ...(optionalString(input.model) ? { model: optionalString(input.model) } : {}),
      ...(optionalString(input.effort) ? { effort: optionalString(input.effort) } : {}),
    };
    thread.id = await this.client.start(thread, (event) => this.handleEvent(event));
    this.thread = thread;
    return { thread: { id: thread.id } };
  }

  private async startTurn(params: unknown): Promise<unknown> {
    if (!this.thread) throw new InvalidParamsError("thread/start or thread/resume must run first");
    if (this.turn) throw new InvalidParamsError("a turn is already active");
    const input = record(params, "turn parameters");
    this.requireThread(input);
    const effort = optionalString(input.effort);
    if (effort) await this.client.send({ type: "set_thinking_level", level: effort });
    const turn: PiTurn = {
      id: randomUUID(),
      interrupted: false,
      settling: false,
      steerGeneration: 0,
      pendingSteers: new Set(),
      assistantMessages: [],
    };
    this.turn = turn;
    await this.emit({
      method: "turn/started",
      params: { threadId: this.thread.id, turn: { id: turn.id, status: "inProgress" } },
    });
    try {
      await this.client.send({ type: "prompt", message: readTextInput(input.input) });
    } catch (error) {
      if (this.turn === turn) this.turn = undefined;
      throw error;
    }
    return { turn: { id: turn.id, status: "inProgress" } };
  }

  private async steerTurn(params: unknown): Promise<unknown> {
    const input = record(params, "steer parameters");
    const turn = this.requireTurn(input, "expectedTurnId");
    let releasePending = () => {};
    const pending = new Promise<void>((resolve) => {
      releasePending = resolve;
    });
    turn.pendingSteers.add(pending);
    const text = readTextInput(input.input);
    try {
      await this.client.send({ type: "steer", message: text });
      turn.steerGeneration++;
    } finally {
      turn.pendingSteers.delete(pending);
      releasePending();
    }
    await this.emitUserMessage(text);
    return { turnId: turn.id };
  }

  // Codex reports a steer as its own userMessage item, which is what puts the
  // steer in the transcript. Pi echoes nothing back, so the
  // adapter emits it once the steer has been accepted.
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
    await this.client.send({ type: "abort" });
    return {};
  }

  private requireThread(input: Record<string, unknown>): void {
    if (requiredString(input.threadId, "threadId") !== this.thread?.id) {
      throw new InvalidParamsError("threadId does not match the configured Pi session");
    }
  }

  private requireTurn(input: Record<string, unknown>, turnKey: string): PiTurn {
    this.requireThread(input);
    if (!this.turn) throw new InvalidParamsError("there is no active Pi turn");
    if (this.turn.settling) throw new InvalidParamsError("the active Pi turn is settling");
    if (requiredString(input[turnKey], turnKey) !== this.turn.id) {
      throw new InvalidParamsError(`${turnKey} does not match the active Pi turn`);
    }
    return this.turn;
  }

  private handleEvent(event: Record<string, unknown>): void {
    const turn = this.turn;
    if (!turn || !this.thread) return;
    if (event.type === "message_end" && isRecord(event.message) && event.message.role === "assistant") {
      turn.assistantMessages.push(event.message);
      return;
    }
    if (event.type === "tool_execution_start") {
      this.tools.set(requiredString(event.toolCallId, "toolCallId"), event);
      void this.emitTool("item/started", event, "inProgress");
      return;
    }
    if (event.type === "tool_execution_update") {
      const merged = this.mergeTool(event);
      void this.emitTool("item/updated", merged, "inProgress");
      return;
    }
    if (event.type === "tool_execution_end") {
      const merged = this.mergeTool(event);
      this.tools.delete(requiredString(event.toolCallId, "toolCallId"));
      void this.emitTool("item/completed", merged, event.isError === true ? "failed" : "completed");
      return;
    }
    if (event.type === "agent_settled") {
      void this.completeTurn(turn, undefined, undefined, turn.steerGeneration);
      return;
    }
    if (event.type === "ruddr_error") {
      void this.completeTurn(turn, "failed", optionalString(event.error) ?? "Pi RPC process failed");
    }
  }

  private mergeTool(event: Record<string, unknown>): Record<string, unknown> {
    const id = requiredString(event.toolCallId, "toolCallId");
    const merged = { ...(this.tools.get(id) ?? {}), ...event };
    this.tools.set(id, merged);
    return merged;
  }

  private async emitTool(method: string, event: Record<string, unknown>, status: string): Promise<void> {
    if (!this.thread) return;
    const id = requiredString(event.toolCallId, "toolCallId");
    const name = optionalString(event.toolName) ?? "tool";
    const input = isRecord(event.args) ? event.args : {};
    const result = isRecord(event.result)
      ? event.result
      : isRecord(event.partialResult)
        ? event.partialResult
        : {};
    await this.emit({
      method,
      params: {
        threadId: this.thread.id,
        item: {
          id,
          type: "toolCall",
          status,
          toolName: name,
          command: optionalString(input.command) ?? `${name} ${JSON.stringify(input)}`,
          input,
          output: textContent(result.content),
        },
      },
    });
  }

  private async completeTurn(
    turn: PiTurn,
    forcedStatus?: string,
    forcedError?: string,
    settledGeneration = turn.steerGeneration,
  ): Promise<void> {
    if (this.turn !== turn || !this.thread || turn.settling) return;
    if (!forcedStatus && turn.pendingSteers.size > 0) {
      await Promise.all([...turn.pendingSteers]);
      if (this.turn !== turn || turn.settling || settledGeneration !== turn.steerGeneration) return;
    }
    if (!forcedStatus && settledGeneration !== turn.steerGeneration) return;
    turn.settling = true;
    // TODO(review): Define terminal statuses for unfinished Pi tool calls before clearing them at turn completion.
    const threadID = this.thread.id;
    const messages = turn.assistantMessages.splice(0);
    for (let index = 0; index < messages.length; index++) {
      const message = messages[index]!;
      const content = Array.isArray(message.content) ? message.content.filter(isRecord) : [];
      const reasoning = content
        .filter((part) => part.type === "thinking" && typeof part.thinking === "string")
        .map((part) => part.thinking as string)
        .join("\n")
        .trim();
      if (reasoning) {
        await this.emit({
          method: "item/completed",
          params: {
            threadId: threadID,
            item: {
              id: `${turn.id}-reasoning-${index}`,
              type: "reasoning",
              status: "completed",
              summary: [{ type: "summary_text", text: reasoning }],
            },
          },
        });
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
            item: {
              id: `${turn.id}-message-${index}`,
              type: "agentMessage",
              status: "completed",
              text,
              phase: index === messages.length - 1 ? "final_answer" : "commentary",
            },
          },
        });
      }
    }

    let stats: Record<string, unknown> = {};
    try {
      stats = record((await this.client.send({ type: "get_session_stats" })).data, "session stats");
      await this.emitUsage(threadID, stats);
    } catch {}

    const last = messages[messages.length - 1];
    const stopReason = optionalString(last?.stopReason);
    const status = forcedStatus ?? (turn.interrupted || stopReason === "aborted" ? "interrupted" : stopReason === "error" ? "failed" : "completed");
    this.turn = undefined;
    await this.emit({
      method: "turn/completed",
      params: {
        threadId: threadID,
        turn: {
          id: turn.id,
          status,
          ...(status === "failed" ? { error: { message: forcedError ?? "Pi model returned an error" } } : {}),
        },
      },
    });
  }

  private async emitUsage(threadID: string, stats: Record<string, unknown>): Promise<void> {
    const tokens = isRecord(stats.tokens) ? stats.tokens : {};
    const context = isRecord(stats.contextUsage) ? stats.contextUsage : {};
    const input = number(tokens.input);
    const output = number(tokens.output);
    const cached = number(tokens.cacheRead);
    const total = number(tokens.total) || number(tokens.totalTokens) || input + output + cached + number(tokens.cacheWrite);
    if (total === 0 && number(stats.cost) === 0) return;
    await this.emit({
      method: "thread/tokenUsage/updated",
      params: {
        threadId: threadID,
        tokenUsage: {
          total: {
            inputTokens: input + cached,
            cachedInputTokens: cached,
            outputTokens: output,
            totalTokens: total,
          },
          ...(typeof context.tokens === "number" && Number.isFinite(context.tokens) && context.tokens >= 0
            ? { last: { totalTokens: context.tokens } } : {}),
          ...(number(context.contextWindow) ? { modelContextWindow: number(context.contextWindow) } : {}),
        },
        costUsd: number(stats.cost),
      },
    });
  }
}

export class SubprocessPiClient implements PiClient {
  private process?: PiProcess;
  private pending = new Map<string, {
    resolve: (value: Record<string, unknown>) => void;
    reject: (error: Error) => void;
    timeout: ReturnType<typeof setTimeout>;
  }>();
  private requestSequence = 0;
  private writeChain = Promise.resolve();
  private onEvent?: (event: Record<string, unknown>) => void;
  private closing = false;

  constructor(
    private readonly rpcTimeoutMs = DEFAULT_RPC_TIMEOUT_MS,
    private readonly startTimeoutMs = Math.max(rpcTimeoutMs, DEFAULT_START_TIMEOUT_MS),
  ) {}

  async start(config: PiThread, onEvent: (event: Record<string, unknown>) => void): Promise<string> {
    this.onEvent = onEvent;
    // TODO(review): Define Pi approval behavior for inherited project resources before changing --approve.
    const args = [config.executable, "--mode", "rpc", "--approve"];
    if (config.model) args.push("--model", config.model);
    if (config.effort) args.push("--thinking", config.effort);
    if (config.ephemeral) args.push("--no-session");
    else if (config.resumed) args.push("--session", config.id);
    else args.push("--session-id", config.id);
    if (config.sandbox === "read-only") {
      args.push("--no-extensions", "--tools", "read,grep,find,ls");
    }
    const process = Bun.spawn(args, {
      cwd: config.cwd,
      stdin: "pipe",
      stdout: "pipe",
      stderr: "inherit",
      env: { ...globalThis.process.env },
    });
    this.process = process;
    void this.readOutput(process);
    const state = record(
      (await this.send({ type: "get_state" }, this.startTimeoutMs)).data,
      "Pi state",
    );
    return requiredString(state.sessionId, "Pi session id");
  }

  async send(
    command: Record<string, unknown>,
    timeoutMs = this.rpcTimeoutMs,
  ): Promise<Record<string, unknown>> {
    const process = this.process;
    if (!process) throw new Error("Pi RPC process is not running");
    const id = `ruddr-pi-${++this.requestSequence}`;
    const response = new Promise<Record<string, unknown>>((resolve, reject) => {
      const timeout = setTimeout(() => {
        const pending = this.pending.get(id);
        if (!pending) return;
        this.pending.delete(id);
        const error = new Error(`Pi RPC ${String(command.type ?? "command")} timed out after ${timeoutMs}ms`);
        pending.reject(error);
        this.failProcess(process, error);
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timeout });
    });
    try {
      await this.write(process, { ...command, id }, timeoutMs);
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
    this.rejectAllPending("Pi RPC client is closing");
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

  private async readOutput(process: PiProcess): Promise<void> {
    try {
      for await (const line of readLines(process.stdout)) {
        if (!line.trim()) continue;
        const message = record(JSON.parse(line), "Pi RPC message");
        if (message.type === "response" && typeof message.id === "string") {
          const pending = this.pending.get(message.id);
          if (!pending) continue;
          this.pending.delete(message.id);
          clearTimeout(pending.timeout);
          if (message.success !== true) pending.reject(new Error(optionalString(message.error) ?? "Pi RPC command returned a malformed response"));
          else pending.resolve(message);
          continue;
        }
        if (message.type === "extension_ui_request" && typeof message.id === "string") {
          const method = optionalString(message.method);
          if (!method || !FIRE_AND_FORGET_UI_METHODS.has(method)) {
            void this.writeRaw({ type: "extension_ui_response", id: message.id, cancelled: true })
              .catch((error) => this.failProcess(process, error instanceof Error ? error : new Error(errorMessage(error))));
          }
          continue;
        }
        this.onEvent?.(message);
      }
      throw new Error("Pi RPC output closed");
    } catch (error) {
      this.failProcess(process, error instanceof Error ? error : new Error(errorMessage(error)));
    }
  }

  private async writeRaw(message: Record<string, unknown>): Promise<void> {
    const process = this.process;
    if (!process) throw new Error("Pi RPC process is not running");
    await this.write(process, message);
  }

  private async write(
    process: PiProcess,
    message: Record<string, unknown>,
    timeoutMs = this.rpcTimeoutMs,
  ): Promise<void> {
    const operation = this.writeChain.then(async () => {
      if (this.process !== process) throw new Error("Pi RPC process is not running");
      process.stdin.write(`${JSON.stringify(message)}\n`);
      await process.stdin.flush();
    });
    this.writeChain = operation.catch(() => {});
    let timeout: ReturnType<typeof setTimeout> | undefined;
    await Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timeout = setTimeout(
          () => reject(new Error(`Pi RPC write timed out after ${timeoutMs}ms`)),
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

  private failProcess(process: PiProcess, error: Error): void {
    if (this.process !== process) return;
    this.process = undefined;
    process.kill();
    this.rejectAllPending(error.message);
    if (!this.closing) this.onEvent?.({ type: "ruddr_error", error: error.message });
  }
}

function textContent(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map(textContent).filter(Boolean).join("\n");
  if (isRecord(value)) return textContent(value.text ?? value.content ?? value.message);
  return "";
}

function number(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
