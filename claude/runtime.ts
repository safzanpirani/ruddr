import {
  query,
  type Options,
  type PermissionMode,
  type Query,
  type SDKMessage,
  type SDKResultMessage,
  type SDKUserMessage,
} from "@anthropic-ai/claude-agent-sdk";
import { randomUUID } from "node:crypto";

import {
  BaseAdapter, InvalidParamsError, MethodNotFoundError,
  errorMessage, isRecord, record, readTextInput,
  type ProtocolMessage,
} from "../adapter/protocol";
// Preserve the adapter's existing type imports for consumers.
export type { RPCID, RPCRequest, RPCError, RPCResponse, RPCNotification, ProtocolMessage } from "../adapter/protocol";

export type QueryFactory = (input: {
  prompt: AsyncIterable<SDKUserMessage>;
  options: Options;
}) => Query;

export interface ThreadConfig {
  id: string;
  cwd: string;
  model?: string;
  sandbox: "read-only" | "workspace-write" | "danger-full-access";
  effort?: Options["effort"];
  claudePath?: string;
  persistSession: boolean;
  resumed: boolean;
}

interface TextBlock {
  index: number;
  id: string;
  text: string;
}

interface ThinkingBlock {
  index: number;
  id: string;
  text: string;
}

interface ToolBlock {
  index: number;
  id: string;
  name: string;
  input: Record<string, unknown>;
  partialInput: string;
  startedAt: number;
  parentToolUseId?: string;
}

interface TurnState {
  id: string;
  textBlocks: Map<number, TextBlock>;
  pendingText: TextBlock[];
  thinkingBlocks: Map<number, ThinkingBlock>;
  toolsByIndex: Map<number, ToolBlock>;
  toolsByID: Map<string, ToolBlock>;
  emittedTexts: Set<string>;
  emittedBlockIDs: Set<string>;
  interruptRequested: boolean;
}

const effortValues = new Set(["low", "medium", "high", "xhigh", "max"]);

export class AsyncMessageQueue implements AsyncIterable<SDKUserMessage> {
  private values: SDKUserMessage[] = [];
  private waiters: Array<(result: IteratorResult<SDKUserMessage>) => void> = [];
  private closed = false;

  push(value: SDKUserMessage): void {
    if (this.closed) throw new Error("Claude prompt queue is closed");
    const waiter = this.waiters.shift();
    if (waiter) waiter({ value, done: false });
    else this.values.push(value);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) {
      waiter({ value: undefined, done: true });
    }
  }

  [Symbol.asyncIterator](): AsyncIterator<SDKUserMessage> {
    return {
      next: async () => {
        const value = this.values.shift();
        if (value) return { value, done: false };
        if (this.closed) return { value: undefined, done: true };
        return new Promise<IteratorResult<SDKUserMessage>>((resolve) => {
          this.waiters.push(resolve);
        });
      },
    };
  }
}

export class ClaudeRuddrAdapter extends BaseAdapter {
  private thread?: ThreadConfig;
  private turn?: TurnState;
  private queue?: AsyncMessageQueue;
  private runtime?: Query;
  private streamTask?: Promise<void>;
  private turnsCompleted = 0;
  private usageTotals = { inputTokens: 0, cachedInputTokens: 0, outputTokens: 0, totalTokens: 0 };
  private costTotal = 0;
  private contextWindow = 0;
  private contextModel?: string;
  private lastUsage?: {
    input_tokens: number; cache_creation_input_tokens: number;
    cache_read_input_tokens: number; output_tokens: number;
  };

  constructor(
    emit: (message: ProtocolMessage) => void | Promise<void>,
    private readonly createQuery: QueryFactory = ({ prompt, options }) =>
      query({ prompt, options }),
    private readonly streamSettleTimeoutMs = 1_000,
  ) {
    super(emit);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.queue?.close();
    this.runtime?.close();
    await Promise.race([
      this.streamTask ?? Promise.resolve(),
      new Promise<void>((resolve) => setTimeout(resolve, this.streamSettleTimeoutMs)),
    ]);
  }

  protected async dispatch(method: string, params: unknown): Promise<unknown> {
    switch (method) {
      case "initialize":
        this.initialized = true;
        return {
          serverInfo: { name: "ruddr-claude-adapter", version: "1" },
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
        throw new MethodNotFoundError(`method ${method} is not supported by the Claude adapter`);
    }
  }

  private acquireThread(params: unknown, resumed: boolean): unknown {
    if (!this.initialized) throw new InvalidParamsError("initialize must run first");
    if (this.thread) throw new InvalidParamsError("a thread is already configured");
    const input = record(params, "thread parameters");
    const id = resumed ? requiredString(input.threadId, "threadId") : randomUUID();
    const cwd = requiredString(input.cwd, "cwd");
    const sandbox = parseSandbox(input.sandbox);
    const effort = parseEffort(input.effort);
    this.thread = {
      id,
      cwd,
      sandbox,
      resumed,
      persistSession: input.persistSession !== false,
      ...(optionalString(input.model) ? { model: optionalString(input.model) } : {}),
      ...(effort ? { effort } : {}),
      ...(optionalString(input.claudePath)
        ? { claudePath: optionalString(input.claudePath) }
        : {}),
    };
    return { thread: { id } };
  }

  private async startTurn(params: unknown): Promise<unknown> {
    if (!this.thread) throw new InvalidParamsError("thread/start or thread/resume must run first");
    if (this.turn) throw new InvalidParamsError("a turn is already active");
    const input = record(params, "turn parameters");
    if (requiredString(input.threadId, "threadId") !== this.thread.id) {
      throw new InvalidParamsError("threadId does not match the configured Claude session");
    }
    const text = readTextInput(input.input);
    const turnEffort = parseEffort(input.effort);
    if (turnEffort) this.thread.effort = turnEffort;
    if (this.turnsCompleted > 0) {
      if (!this.thread.persistSession) {
        throw new InvalidParamsError("ephemeral Claude sessions support a single turn");
      }
      // Let the previous turn's stream settle before starting a fresh query.
      this.runtime?.close();
      const settled = await Promise.race([
        (this.streamTask ?? Promise.resolve()).then(() => true),
        new Promise<false>((resolve) =>
          setTimeout(() => resolve(false), this.streamSettleTimeoutMs),
        ),
      ]);
      if (!settled) {
        throw new InvalidParamsError(
          "the previous Claude runtime did not settle; retry the turn after it closes",
        );
      }
      this.runtime = undefined;
      this.queue = undefined;
      this.streamTask = undefined;
    }
    const turn: TurnState = {
      id: randomUUID(),
      textBlocks: new Map(),
      pendingText: [],
      thinkingBlocks: new Map(),
      toolsByIndex: new Map(),
      toolsByID: new Map(),
      emittedTexts: new Set(),
      emittedBlockIDs: new Set(),
      interruptRequested: false,
    };
    const queue = new AsyncMessageQueue();
    const options = buildQueryOptions(this.thread);
    const runtime = this.createQuery({ prompt: queue, options });
    this.turn = turn;
    this.queue = queue;
    this.runtime = runtime;
    this.streamTask = this.consumeRuntime(this.runtime);
    this.queue.push(userMessage(text));
    await this.emit({
      method: "turn/started",
      params: { turn: { id: turn.id, status: "inProgress" } },
    });
    return { turn: { id: turn.id, status: "inProgress" } };
  }

  private steerTurn(params: unknown): unknown {
    const input = record(params, "steer parameters");
    const turn = this.requireActiveTurn(input);
    const text = readTextInput(input.input);
    this.queue?.push(userMessage(text));
    return { turnId: turn.id };
  }

  private async interruptTurn(params: unknown): Promise<unknown> {
    const input = record(params, "interrupt parameters");
    const turn = this.requireActiveTurn(input, "turnId");
    turn.interruptRequested = true;
    this.queue?.close();
    await this.completeTurn("interrupted");
    this.runtime?.close();
    return {};
  }

  private requireActiveTurn(input: Record<string, unknown>, turnKey = "expectedTurnId"): TurnState {
    if (!this.thread || !this.turn) throw new InvalidParamsError("there is no active Claude turn");
    if (requiredString(input.threadId, "threadId") !== this.thread.id) {
      throw new InvalidParamsError("threadId does not match the active Claude session");
    }
    if (requiredString(input[turnKey], turnKey) !== this.turn.id) {
      throw new InvalidParamsError(`${turnKey} does not match the active Claude turn`);
    }
    return this.turn;
  }

  private async consumeRuntime(runtime: Query): Promise<void> {
    try {
      for await (const message of runtime) {
        await this.handleSDKMessage(message);
      }
      if (this.turn) {
        await this.completeTurn(this.turn.interruptRequested ? "interrupted" : "failed", "Claude runtime stream ended before a terminal result");
      }
    } catch (error) {
      if (this.turn) {
        await this.completeTurn(this.turn.interruptRequested ? "interrupted" : "failed", errorMessage(error));
      }
    }
  }

  private async handleSDKMessage(message: SDKMessage): Promise<void> {
    if (message.type === "stream_event") {
      await this.handleStreamEvent(message);
      return;
    }
    if (message.type === "user") {
      await this.handleToolResults(message);
      return;
    }
    if (message.type === "result") {
      await this.handleResult(message);
    }
  }

  private async handleStreamEvent(message: Extract<SDKMessage, { type: "stream_event" }>): Promise<void> {
    const turn = this.turn;
    if (!turn) return;
    const event = message.event;
    if (!message.parent_tool_use_id && (event.type === "message_start" || event.type === "message_delta")) {
      if (event.type === "message_start") {
        if (this.contextModel && this.contextModel !== event.message.model) this.contextWindow = 0;
        this.contextModel = event.message.model;
        this.lastUsage = { input_tokens: 0, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 0 };
      }
      const usage = event.type === "message_start" ? event.message.usage : event.usage;
      if (this.lastUsage) {
        for (const key of Object.keys(this.lastUsage) as Array<keyof NonNullable<typeof this.lastUsage>>) {
          const value = usage[key];
          if (typeof value === "number" && Number.isFinite(value) && value >= 0) this.lastUsage[key] = value;
        }
        await this.emitUsageSnapshot();
      }
      return;
    }
    if (message.parent_tool_use_id && event.type === "content_block_delta") {
      if (event.delta.type === "text_delta" || event.delta.type === "thinking_delta") return;
    }
    if (event.type === "content_block_start") {
      const block = event.content_block;
      if (block.type === "text") {
        turn.textBlocks.set(event.index, { index: event.index, id: internalID(), text: block.text });
      } else if (block.type === "thinking") {
        turn.thinkingBlocks.set(event.index, { index: event.index, id: internalID(), text: block.thinking });
      } else if (block.type === "tool_use" || block.type === "server_tool_use" || block.type === "mcp_tool_use") {
        await this.flushPendingText("commentary");
        const input = isRecord(block.input) ? block.input : {};
        const tool: ToolBlock = {
          index: event.index,
          id: block.id,
          name: block.name,
          input,
          partialInput: "",
          startedAt: Date.now(),
          ...(message.parent_tool_use_id ? { parentToolUseId: message.parent_tool_use_id } : {}),
        };
        turn.toolsByIndex.set(event.index, tool);
        turn.toolsByID.set(tool.id, tool);
        await this.emitTool("item/started", tool, "inProgress");
      }
      return;
    }
    if (event.type === "content_block_delta") {
      if (event.delta.type === "text_delta") {
        const block = turn.textBlocks.get(event.index) ?? { index: event.index, id: internalID(), text: "" };
        block.text += event.delta.text;
        turn.textBlocks.set(event.index, block);
      } else if (event.delta.type === "thinking_delta") {
        const block = turn.thinkingBlocks.get(event.index) ?? { index: event.index, id: internalID(), text: "" };
        block.text += event.delta.thinking;
        turn.thinkingBlocks.set(event.index, block);
      } else if (event.delta.type === "input_json_delta") {
        const tool = turn.toolsByIndex.get(event.index);
        if (!tool) return;
        tool.partialInput += event.delta.partial_json;
        const parsed = parseJSONRecord(tool.partialInput);
        if (parsed) {
          tool.input = parsed;
          await this.emitTool("item/updated", tool, "inProgress");
        }
      }
      return;
    }
    if (event.type === "content_block_stop") {
      const text = turn.textBlocks.get(event.index);
      if (text) {
        turn.textBlocks.delete(event.index);
        if (text.text.trim()) turn.pendingText.push(text);
      }
      const thinking = turn.thinkingBlocks.get(event.index);
      if (thinking) {
        turn.thinkingBlocks.delete(event.index);
        if (thinking.text.trim()) {
          await this.emit({
            method: "item/completed",
            params: {
              item: {
                id: thinking.id,
                type: "reasoning",
                status: "completed",
                summary: [{ type: "summary_text", text: thinking.text.trim() }],
              },
            },
          });
        }
      }
      const tool = turn.toolsByIndex.get(event.index);
      if (tool && tool.partialInput) {
        tool.input = parseJSONRecord(tool.partialInput) ?? tool.input;
        await this.emitTool("item/updated", tool, "inProgress");
      }
    }
  }

  private async handleToolResults(message: Extract<SDKMessage, { type: "user" }>): Promise<void> {
    const turn = this.turn;
    if (!turn || message.parent_tool_use_id) return;
    const content = Array.isArray(message.message.content) ? message.message.content : [];
    for (const block of content) {
      if (!isRecord(block) || block.type !== "tool_result" || typeof block.tool_use_id !== "string") continue;
      const tool = turn.toolsByID.get(block.tool_use_id);
      if (!tool) continue;
      const output = extractText(block.content);
      const failed = block.is_error === true;
      await this.emitTool("item/completed", tool, failed ? "failed" : "completed", output);
      turn.toolsByID.delete(tool.id);
      turn.toolsByIndex.delete(tool.index);
    }
  }

  private async handleResult(result: SDKResultMessage): Promise<void> {
    if (!this.turn) return;
    const queued = typeof result.queued_turn_count === "number" ? result.queued_turn_count : 0;
    if (queued > 0) {
      await this.flushPendingText("commentary");
      return;
    }
    const status = resultStatus(result);
    if (status === "completed") {
      if (this.turn.pendingText.length === 0 && result.subtype === "success" && result.result.trim() && !this.turn.emittedTexts.has(result.result.trim())) {
        this.turn.pendingText.push({ index: -1, id: internalID(), text: result.result });
      }
      const finalBlock = this.turn.pendingText.pop();
      await this.flushPendingText("commentary");
      if (finalBlock) await this.emitAgentMessage(finalBlock, "final_answer");
    } else {
      await this.flushPendingText("commentary");
    }
    await this.emitTokenUsage(result);
    await this.completeTurn(status, resultError(result));
  }

  // Accumulates SDK usage/cost across turns and forwards it in the same
  // thread/tokenUsage/updated shape Codex emits, plus a costUsd extension.
  private async emitTokenUsage(result: SDKResultMessage): Promise<void> {
    if (!this.thread) return;
    const count = (value: unknown): number => (typeof value === "number" && Number.isFinite(value) ? value : 0);
    const perModel = Object.values(result.modelUsage ?? {});
    let input = 0;
    let cached = 0;
    let output = 0;
    if (perModel.length > 0) {
      for (const usage of perModel) {
        input +=
          count(usage.inputTokens) +
          count(usage.cacheCreationInputTokens) +
          count(usage.cacheReadInputTokens);
        cached += count(usage.cacheReadInputTokens);
        output += count(usage.outputTokens);
      }
    } else if (isRecord(result.usage)) {
      input =
        count(result.usage.input_tokens) +
        count(result.usage.cache_creation_input_tokens) +
        count(result.usage.cache_read_input_tokens);
      cached = count(result.usage.cache_read_input_tokens);
      output = count(result.usage.output_tokens);
    }
    this.usageTotals.inputTokens += input;
    this.usageTotals.cachedInputTokens += cached;
    this.usageTotals.outputTokens += output;
    this.usageTotals.totalTokens += input + output;
    if (typeof result.total_cost_usd === "number" && Number.isFinite(result.total_cost_usd)) {
      this.costTotal += result.total_cost_usd;
    }
    const mainModel = this.contextModel ? result.modelUsage?.[this.contextModel] : undefined;
    const window = count((mainModel ?? (perModel.length === 1 ? perModel[0] : undefined))?.contextWindow);
    if (window > 0) this.contextWindow = window;
    await this.emitUsageSnapshot();
  }

  private async emitUsageSnapshot(): Promise<void> {
    if (!this.thread || (this.usageTotals.totalTokens === 0 && this.costTotal === 0 && !this.lastUsage)) return;
    const last = this.lastUsage;
    const input = last ? last.input_tokens + last.cache_creation_input_tokens + last.cache_read_input_tokens : 0;
    await this.emit({
      method: "thread/tokenUsage/updated",
      params: {
        threadId: this.thread.id,
        tokenUsage: {
          total: { ...this.usageTotals },
          ...(last ? { last: {
            inputTokens: input, cachedInputTokens: last.cache_read_input_tokens,
            outputTokens: last.output_tokens, totalTokens: input + last.output_tokens,
          } } : {}),
          ...(this.contextWindow > 0
            ? { modelContextWindow: this.contextWindow }
            : {}),
        },
        costUsd: this.costTotal,
      },
    });
  }

  private async flushPendingText(phase: "commentary" | "final_answer"): Promise<void> {
    const pending = this.turn?.pendingText.splice(0) ?? [];
    for (const block of pending) await this.emitAgentMessage(block, phase);
  }

  private async emitAgentMessage(block: TextBlock, phase: "commentary" | "final_answer"): Promise<void> {
    const turn = this.turn;
    const text = block.text.trim();
    if (!turn || !text || turn.emittedBlockIDs.has(block.id)) return;
    turn.emittedBlockIDs.add(block.id);
    turn.emittedTexts.add(text);
    await this.emit({
      method: "item/completed",
      params: { item: { id: block.id, type: "agentMessage", status: "completed", phase, text } },
    });
  }

  private async emitTool(method: "item/started" | "item/updated" | "item/completed", tool: ToolBlock, status: "inProgress" | "completed" | "failed", output?: string): Promise<void> {
    const item = normalizeTool(tool, status, output);
    await this.emit({ method, params: { item } });
  }

  private async completeTurn(status: "completed" | "failed" | "interrupted", error?: string): Promise<void> {
    const turn = this.turn;
    if (!turn) return;
    this.turn = undefined;
    this.queue?.close();
    this.turnsCompleted += 1;
    if (this.thread?.persistSession) {
      // The next turn must resume the persisted session instead of reusing
      // the sessionId, which the SDK rejects for an existing session.
      this.thread.resumed = true;
    }
    await this.emit({
      method: "turn/completed",
      params: {
        turn: {
          id: turn.id,
          status,
          ...(error ? { error: { code: -32000, message: error } } : {}),
        },
      },
    });
  }
}

export function buildQueryOptions(thread: ThreadConfig): Options {
  const permissionMode: PermissionMode =
    thread.sandbox === "read-only"
      ? "plan"
      : thread.sandbox === "danger-full-access"
        ? "bypassPermissions"
        : "acceptEdits";
  const sandbox =
    thread.sandbox === "workspace-write"
      ? {
          enabled: true,
          failIfUnavailable: true,
          autoAllowBashIfSandboxed: true,
          allowUnsandboxedCommands: false,
          filesystem: { allowWrite: [thread.cwd] },
        }
      : undefined;
  return {
    cwd: thread.cwd,
    systemPrompt: { type: "preset", preset: "claude_code" },
    settingSources: ["user", "project", "local"],
    includePartialMessages: true,
    forwardSubagentText: false,
    thinking: { type: "adaptive" },
    permissionMode,
    ...(sandbox ? { sandbox } : {}),
    persistSession: thread.persistSession,
    ...(thread.resumed ? { resume: thread.id } : { sessionId: thread.id }),
    ...(thread.model ? { model: thread.model } : {}),
    ...(thread.effort ? { effort: thread.effort } : {}),
    ...(thread.claudePath ? { pathToClaudeCodeExecutable: thread.claudePath } : {}),
    ...(permissionMode === "bypassPermissions"
      ? { allowDangerouslySkipPermissions: true }
      : {
          canUseTool: async (toolName, input, options) => {
            if (toolName === "AskUserQuestion") {
              return {
                behavior: "deny" as const,
                message: "Ruddr cannot answer interactive questions; proceed with best judgment.",
              };
            }
            if (toolName === "Bash" && thread.sandbox === "workspace-write") {
              if (input.dangerouslyDisableSandbox === true) {
                return {
                  behavior: "deny" as const,
                  message: "Ruddr denied a request to run Bash outside the workspace sandbox.",
                };
              }
              if (options.blockedPath) {
                return {
                  behavior: "deny" as const,
                  message: "Ruddr denied Bash access outside the workspace sandbox.",
                };
              }
              if (options.matchedAskRule) {
                return {
                  behavior: "deny" as const,
                  message: "Ruddr cannot override an explicit interactive approval rule.",
                };
              }

              // The SDK can request approval for commands that its classifier
              // cannot auto-allow. The active sandbox still confines them.
              return { behavior: "allow" as const };
            }
            return {
              behavior: "deny" as const,
              message: "Ruddr has no interactive approval surface for this operation.",
            };
          },
        }),
  };
}

export function userMessage(text: string): SDKUserMessage {
  return {
    type: "user",
    session_id: "",
    parent_tool_use_id: null,
    origin: { kind: "human" },
    message: { role: "user", content: [{ type: "text", text }] },
  };
}

export function resultStatus(result: SDKResultMessage): "completed" | "failed" | "interrupted" {
  if (result.subtype === "success" && !result.is_error) return "completed";
  if (result.terminal_reason === "aborted_tools" || result.terminal_reason === "aborted_streaming") return "interrupted";
  const errors = result.subtype === "success" ? result.result : result.errors.join(" ");
  return /interrupt|cancel|abort/i.test(errors) ? "interrupted" : "failed";
}

function resultError(result: SDKResultMessage): string | undefined {
  if (result.subtype === "success") return result.is_error ? result.result : undefined;
  return result.errors.find((message) => !message.startsWith("[ede_diagnostic]"));
}

function normalizeTool(tool: ToolBlock, status: string, output?: string): Record<string, unknown> {
  const lower = tool.name.toLowerCase();
  const durationMs = Math.max(0, Date.now() - tool.startedAt);
  const common = {
    id: tool.id,
    status,
    toolName: tool.name,
    input: tool.input,
    durationMs,
    ...(output ? { aggregatedOutput: output } : {}),
    ...(tool.parentToolUseId ? { parentToolUseId: tool.parentToolUseId } : {}),
  };
  if (lower === "bash" || lower === "shell") {
    return { ...common, type: "commandExecution", command: optionalString(tool.input.command) ?? tool.name, cwd: optionalString(tool.input.cwd) };
  }
  if (["edit", "write", "notebookedit"].includes(lower)) {
    return { ...common, type: "fileChange", command: summarizeTool(tool.name, tool.input) };
  }
  if (lower === "websearch" || lower === "webfetch") {
    return { ...common, type: "webSearch", query: optionalString(tool.input.query) ?? optionalString(tool.input.url), command: summarizeTool(tool.name, tool.input) };
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

function parseSandbox(value: unknown): ThreadConfig["sandbox"] {
  if (value === "read-only" || value === "workspace-write" || value === "danger-full-access") return value;
  throw new InvalidParamsError("sandbox must be read-only, workspace-write, or danger-full-access");
}

function parseEffort(value: unknown): Options["effort"] | undefined {
  if (value === undefined || value === "") return undefined;
  if (typeof value === "string" && effortValues.has(value)) return value as Options["effort"];
  throw new InvalidParamsError("Claude effort must be low, medium, high, xhigh, or max");
}

function extractText(value: unknown): string {
  if (typeof value === "string") return value;
  if (!Array.isArray(value)) return "";
  return value
    .map((entry) => (isRecord(entry) && typeof entry.text === "string" ? entry.text : ""))
    .filter(Boolean)
    .join("\n");
}

function parseJSONRecord(value: string): Record<string, unknown> | undefined {
  try {
    const parsed: unknown = JSON.parse(value);
    return isRecord(parsed) ? parsed : undefined;
  } catch {
    return undefined;
  }
}

// Claude configuration strings historically trim whitespace.
function requiredString(value: unknown, label: string): string {
  const parsed = optionalString(value);
  if (!parsed) throw new InvalidParamsError(`${label} is required`);
  return parsed;
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value.trim() : undefined;
}

function internalID(): string {
  return typeof Bun.randomUUIDv7 === "function" ? Bun.randomUUIDv7() : randomUUID();
}
