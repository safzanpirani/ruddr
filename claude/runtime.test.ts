import { describe, expect, test } from "bun:test";
import type { Query, SDKMessage, SDKResultMessage } from "@anthropic-ai/claude-agent-sdk";
import {
  AsyncMessageQueue,
  buildQueryOptions,
  ClaudeRuddrAdapter,
  resultStatus,
  userMessage,
  type ProtocolMessage,
  type QueryFactory,
} from "./runtime";

class FakeSDKStream implements AsyncIterable<SDKMessage> {
  private values: SDKMessage[] = [];
  private waiters: Array<(result: IteratorResult<SDKMessage>) => void> = [];
  private closed = false;

  constructor(private readonly ignoreClose = false) {}

  push(value: SDKMessage): void {
    const waiter = this.waiters.shift();
    if (waiter) waiter({ value, done: false });
    else this.values.push(value);
  }

  close(): void {
  if (this.ignoreClose) return;
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) waiter({ value: undefined, done: true });
  }

  query(): Query {
    const iterator = this[Symbol.asyncIterator]();
    const fake = Object.assign(iterator, {
      close: () => this.close(),
      interrupt: async () => undefined,
      setPermissionMode: async () => undefined,
      setMcpPermissionModeOverride: async () => ({}),
      setModel: async () => undefined,
    }) as AsyncIterator<SDKMessage> & { [Symbol.asyncIterator]?: () => Query };
    const typed = fake as unknown as Query;
    fake[Symbol.asyncIterator] = () => typed;
    return typed;
  }

  [Symbol.asyncIterator](): AsyncIterator<SDKMessage> {
    return {
      next: async () => {
        const value = this.values.shift();
        if (value) return { value, done: false };
        if (this.closed) return { value: undefined, done: true };
        return new Promise((resolve) => this.waiters.push(resolve));
      },
    };
  }
}

describe("AsyncMessageQueue", () => {
  test("preserves queued steering order and closes", async () => {
    const queue = new AsyncMessageQueue();
    const iterator = queue[Symbol.asyncIterator]();
    queue.push(userMessage("first"));
    queue.push(userMessage("second"));
    expect(textOf((await iterator.next()).value)).toBe("first");
    expect(textOf((await iterator.next()).value)).toBe("second");
    queue.close();
    expect((await iterator.next()).done).toBe(true);
    expect(() => queue.push(userMessage("late"))).toThrow("closed");
  });
});

describe("query options", () => {
  test("maps danger-full-access and read-only with fresh/resume identity", () => {
    const fresh = buildQueryOptions({
      id: "fresh-id",
      cwd: "/tmp/project",
      sandbox: "danger-full-access",
      persistSession: true,
      resumed: false,
    });
    expect(fresh.permissionMode).toBe("bypassPermissions");
    expect(fresh.allowDangerouslySkipPermissions).toBe(true);
    expect(fresh.sessionId).toBe("fresh-id");
    expect(fresh.resume).toBeUndefined();

    const resumed = buildQueryOptions({
      id: "resume-id",
      cwd: "/tmp/project",
      sandbox: "read-only",
      persistSession: false,
      resumed: true,
    });
    expect(resumed.permissionMode).toBe("plan");
    expect(resumed.resume).toBe("resume-id");
    expect(resumed.persistSession).toBe(false);
  });

  test("does not approve Bash requests in read-only mode", async () => {
    const options = buildQueryOptions({
      id: "read-only-id", cwd: "/tmp/project", sandbox: "read-only",
      persistSession: true, resumed: false,
    });
    expect(options.sandbox).toBeUndefined();
    expect((await options.canUseTool!("Bash", { command: "touch probe-file" }, {
      signal: new AbortController().signal, toolUseID: "tool", requestId: "request",
    }))?.behavior).toBe("deny");
  });

  test("runs workspace Bash inside Claude's command sandbox", async () => {
    const workspace = buildQueryOptions({
      id: "workspace-id",
      cwd: "/tmp/project",
      sandbox: "workspace-write",
      persistSession: true,
      resumed: false,
    });

    expect(workspace.permissionMode).toBe("acceptEdits");
    expect(workspace.sandbox).toEqual({
      enabled: true,
      failIfUnavailable: true,
      autoAllowBashIfSandboxed: true,
      allowUnsandboxedCommands: false,
      filesystem: { allowWrite: ["/tmp/project"] },
    });

    const callbackOptions = {
      signal: new AbortController().signal,
      toolUseID: "tool-id",
      requestId: "request-id",
    };
    const ambiguousSandboxedCommand = await workspace.canUseTool?.(
      "Bash",
      { command: "awk '{ print $1 }' inventory.csv" },
      callbackOptions,
    );
    expect(ambiguousSandboxedCommand).toEqual({ behavior: "allow" });

    const sandboxEscape = await workspace.canUseTool?.(
      "Bash",
      { command: "pwd", dangerouslyDisableSandbox: true },
      callbackOptions,
    );
    expect(sandboxEscape).toEqual({
      behavior: "deny",
      message: "Ruddr denied a request to run Bash outside the workspace sandbox.",
    });

    const blockedPath = await workspace.canUseTool?.(
      "Bash",
      { command: "cat ../outside.txt" },
      { ...callbackOptions, blockedPath: "/tmp/outside.txt" },
    );
    expect(blockedPath).toEqual({
      behavior: "deny",
      message: "Ruddr denied Bash access outside the workspace sandbox.",
    });

    const explicitAskRule = await workspace.canUseTool?.(
      "Bash",
      { command: "make verify" },
      {
        ...callbackOptions,
        matchedAskRule: {
          source: "projectSettings",
          toolName: "Bash",
          ruleContent: "Bash(make *)",
        },
      },
    );
    expect(explicitAskRule).toEqual({
      behavior: "deny",
      message: "Ruddr cannot override an explicit interactive approval rule.",
    });
  });
});

describe("ClaudeRuddrAdapter", () => {
  test("returns JSON-RPC method-not-found for unsupported methods", async () => {
    const emitted: ProtocolMessage[] = [];
    const adapter = new ClaudeRuddrAdapter((message) => {
      emitted.push(message);
    });
    await adapter.handle({ id: "unknown", method: "thread/archive", params: {} });
    expect(emitted).toEqual([
      {
        id: "unknown",
        error: {
          code: -32601,
          message: "method thread/archive is not supported by the Claude adapter",
        },
      },
    ]);
  });

  test("allows a retry after query construction fails", async () => {
    const emitted: ProtocolMessage[] = [];
    const stream = new FakeSDKStream();
    let attempts = 0;
    const adapter = new ClaudeRuddrAdapter((message) => { emitted.push(message); }, () => {
      if (++attempts === 1) throw new Error("query construction failed");
      return stream.query();
    });
    try {
      await adapter.handle({ id: "init", method: "initialize", params: {} });
      await adapter.handle({ id: "thread", method: "thread/start", params: { cwd: "/tmp/project", sandbox: "read-only" } });
      const threadId = responseResult(emitted, "thread", "thread.id");
      const params = { threadId, input: [{ type: "text", text: "hello" }] };
      await adapter.handle({ id: "first", method: "turn/start", params });
      expect(emitted.find((message) => "id" in message && message.id === "first")).toMatchObject({ error: { message: "query construction failed" } });
      await adapter.handle({ id: "retry", method: "turn/start", params });
      expect(responseResult(emitted, "retry", "turn.id")).toBeTruthy();
      expect(attempts).toBe(2);
    } finally {
      await adapter.close();
    }
  });

  test("preserves distinct identical messages while suppressing a repeated result fallback", async () => {
    const emitted: ProtocolMessage[] = [];
    const stream = new FakeSDKStream();
    const adapter = new ClaudeRuddrAdapter((message) => { emitted.push(message); }, () => stream.query());
    try {
      await adapter.handle({ id: "init", method: "initialize", params: {} });
      await adapter.handle({ id: "thread", method: "thread/start", params: { cwd: "/tmp/project", sandbox: "read-only" } });
      const threadId = responseResult(emitted, "thread", "thread.id");
      await adapter.handle({ id: "turn", method: "turn/start", params: { threadId, input: [{ type: "text", text: "hello" }] } });
      for (const index of [0, 1]) {
        stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, event: { type: "content_block_start", index, content_block: { type: "text", text: "Done." } } }));
        stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, event: { type: "content_block_stop", index } }));
      }
      stream.push(sdk({ type: "result", subtype: "success", result: "Done.", queued_turn_count: 0 }));
      await waitFor(() => emitted.some((message) => notification(message, "turn/completed")));
      const messages = emitted.filter((message) => notification(message, "item/completed") && JSON.stringify(message).includes('"agentMessage"'));
      expect(messages).toHaveLength(2);
      expect(messages[0]).toMatchObject({ params: { item: { phase: "commentary", text: "Done." } } });
      expect(messages[1]).toMatchObject({ params: { item: { phase: "final_answer", text: "Done." } } });
      expect(new Set(messages.map((message) => (message as any).params.item.id)).size).toBe(2);
    } finally {
      await adapter.close();
    }
  });

  test("normalizes thinking, tools, commentary, final output, and completion", async () => {
    const emitted: ProtocolMessage[] = [];
    const stream = new FakeSDKStream();
    let prompt: AsyncIterable<ReturnType<typeof userMessage>> | undefined;
    const factory: QueryFactory = (input) => {
      prompt = input.prompt;
      return stream.query();
    };
    const adapter = new ClaudeRuddrAdapter((message) => {
      emitted.push(message);
    }, factory);

    await adapter.handle({ id: "init", method: "initialize", params: {} });
    await adapter.handle({
      id: "thread",
      method: "thread/start",
      params: { cwd: "/tmp/project", sandbox: "workspace-write", provider: "claude" },
    });
    const threadID = responseResult(emitted, "thread", "thread.id");
    await adapter.handle({
      id: "turn",
      method: "turn/start",
      params: { threadId: threadID, input: [{ type: "text", text: "initial" }] },
    });
    const turnID = responseResult(emitted, "turn", "turn.id");
    const promptIterator = prompt![Symbol.asyncIterator]();
    expect(textOf((await promptIterator.next()).value)).toBe("initial");

    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "a", session_id: threadID, event: { type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "", signature: "" } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "b", session_id: threadID, event: { type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "Inspecting the failure" } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "c", session_id: threadID, event: { type: "content_block_stop", index: 0 } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "d", session_id: threadID, event: { type: "content_block_start", index: 1, content_block: { type: "text", text: "" } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "e", session_id: threadID, event: { type: "content_block_delta", index: 1, delta: { type: "text_delta", text: "I found the issue." } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "f", session_id: threadID, event: { type: "content_block_stop", index: 1 } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "g", session_id: threadID, event: { type: "content_block_start", index: 2, content_block: { type: "tool_use", id: "tool-1", name: "Bash", input: {} } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "h", session_id: threadID, event: { type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: '{"command":"bun test"}' } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, uuid: "i", session_id: threadID, event: { type: "content_block_stop", index: 2 } }));
    stream.push(sdk({ type: "user", parent_tool_use_id: null, session_id: threadID, message: { role: "user", content: [{ type: "tool_result", tool_use_id: "tool-1", content: "pass" }] } }));
    stream.push(sdk({ type: "result", subtype: "success", is_error: false, result: "Done.", queued_turn_count: 0, session_id: threadID, uuid: "r", duration_ms: 1, duration_api_ms: 1, num_turns: 1, stop_reason: "end_turn", total_cost_usd: 0, usage: {}, modelUsage: {}, permission_denials: [] }));

    await waitFor(() => emitted.some((message) => notification(message, "turn/completed")));
    const notifications = emitted.filter((message): message is Extract<ProtocolMessage, { method: string }> => "method" in message);
    expect(notifications.some((message) => message.method === "item/completed" && JSON.stringify(message).includes("Inspecting the failure"))).toBe(true);
    expect(notifications.some((message) => message.method === "item/started" && JSON.stringify(message).includes("tool-1"))).toBe(true);
    expect(notifications.some((message) => message.method === "item/completed" && JSON.stringify(message).includes("I found the issue") && JSON.stringify(message).includes("commentary"))).toBe(true);
    expect(notifications.some((message) => message.method === "item/completed" && JSON.stringify(message).includes("Done.") && JSON.stringify(message).includes("final_answer"))).toBe(true);
    expect(JSON.stringify(notifications.find((message) => message.method === "turn/completed"))).toContain(turnID);

    // Assistant text streams as it arrives, in the same shape Codex emits, and
    // the completed item that follows shares the streamed item's id.
    const deltas = notifications.filter((message) => message.method === "item/agentMessage/delta");
    expect(deltas.map((message) => (message.params as { delta: string }).delta)).toEqual([
      "I found the issue.",
    ]);
    const streamedID = (deltas[0].params as { itemId: string }).itemId;
    expect(
      notifications.some(
        (message) =>
          message.method === "item/completed" &&
          (message.params as { item: { id: string; text: string } }).item.id === streamedID &&
          (message.params as { item: { id: string; text: string } }).item.text ===
            "I found the issue.",
      ),
    ).toBe(true);
    await adapter.close();
  });

  test("queues steer text on the same turn", async () => {
    const emitted: ProtocolMessage[] = [];
    const stream = new FakeSDKStream();
    let prompt: AsyncIterable<ReturnType<typeof userMessage>> | undefined;
    const adapter = new ClaudeRuddrAdapter(
      (message) => {
        emitted.push(message);
      },
      (input) => {
        prompt = input.prompt;
        return stream.query();
      },
    );
    await adapter.handle({ id: 1, method: "initialize", params: {} });
    await adapter.handle({ id: 2, method: "thread/start", params: { cwd: "/tmp", sandbox: "workspace-write" } });
    const threadID = responseResult(emitted, 2, "thread.id");
    await adapter.handle({ id: 3, method: "turn/start", params: { threadId: threadID, input: [{ type: "text", text: "first" }] } });
    const turnID = responseResult(emitted, 3, "turn.id");
    await adapter.handle({ id: 4, method: "turn/steer", params: { threadId: threadID, expectedTurnId: turnID, input: [{ type: "text", text: "steer" }] } });
    const iterator = prompt![Symbol.asyncIterator]();
    expect(textOf((await iterator.next()).value)).toBe("first");
    expect(textOf((await iterator.next()).value)).toBe("steer");
    expect(responseResult(emitted, 4, "turnId")).toBe(turnID);
    // The steer also reaches the transcript as its own user message; nothing
    // in the Claude stream echoes it back.
    const userItems = emitted
      .filter((message): message is Extract<ProtocolMessage, { method: string }> =>
        "method" in message && message.method === "item/completed")
      .map((message) => (message.params as { item: { type: string; text: string } }).item)
      .filter((item) => item.type === "userMessage");
    expect(userItems.map((item) => item.text)).toEqual(["steer"]);
    await adapter.close();
  });
});

test("Claude context tracks the latest main request and preserves cached input across deltas", async () => {
  const emitted: ProtocolMessage[] = [];
  const stream = new FakeSDKStream();
  const adapter = new ClaudeRuddrAdapter((message) => { emitted.push(message); }, () => stream.query());
  const usageEvents = () => emitted.filter((message) => notification(message, "thread/tokenUsage/updated")) as Array<{ params: { tokenUsage: { last: { totalTokens: number }; total: { totalTokens: number }; modelContextWindow?: number } } }>;
  try {
    await adapter.handle({ id: 1, method: "initialize", params: {} });
    await adapter.handle({ id: 2, method: "thread/start", params: { cwd: "/tmp", sandbox: "read-only" } });
    const threadId = responseResult(emitted, 2, "thread.id");
    await adapter.handle({ id: 3, method: "turn/start", params: { threadId, input: [{ type: "text", text: "hello" }] } });
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, event: { type: "message_start", message: { model: "main", usage: { input_tokens: 1000, cache_read_input_tokens: 2000, cache_creation_input_tokens: 300, output_tokens: 10 } } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, event: { type: "message_delta", usage: { input_tokens: null, cache_read_input_tokens: null, output_tokens: 20 } } }));
    await waitFor(() => usageEvents().length === 2);
    expect(usageEvents()[1].params.tokenUsage.last.totalTokens).toBe(3320);
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: "tool-sub", event: { type: "message_start", message: { model: "sub", usage: { input_tokens: 999999 } } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: "tool-sub", event: { type: "message_delta", usage: { output_tokens: 9999 } } }));
    stream.push(sdk({ type: "stream_event", parent_tool_use_id: null, event: { type: "message_start", message: { model: "main", usage: { input_tokens: 500, output_tokens: 3 } } } }));
    stream.push(sdk({ type: "result", subtype: "success", result: "done", queued_turn_count: 0, modelUsage: {
      main: { inputTokens: 2400000, outputTokens: 100000, contextWindow: 200000 },
      sub: { inputTokens: 0, outputTokens: 0, contextWindow: 1000000 },
    } }));
    await waitFor(() => emitted.some((message) => notification(message, "turn/completed")));
    expect(usageEvents().map((message) => message.params.tokenUsage.last.totalTokens)).toEqual([3310, 3320, 503, 503]);
    expect(usageEvents().at(-1)?.params.tokenUsage).toMatchObject({ total: { totalTokens: 2500000 }, last: { totalTokens: 503 }, modelContextWindow: 200000 });
  } finally {
    await adapter.close();
  }
});

test("result status recognizes Claude abort terminal reasons", () => {
  expect(resultStatus({ subtype: "success", is_error: false } as SDKResultMessage)).toBe("completed");
  expect(resultStatus({ subtype: "error_during_execution", terminal_reason: "aborted_tools", errors: [] } as unknown as SDKResultMessage)).toBe("interrupted");
  expect(resultStatus({ subtype: "error_during_execution", terminal_reason: "model_error", errors: ["boom"] } as unknown as SDKResultMessage)).toBe("failed");
});

describe("multi-turn sessions", () => {
  test("second turn resumes the session and usage accumulates across turns", async () => {
    const emitted: ProtocolMessage[] = [];
    const streams: FakeSDKStream[] = [];
    const capturedOptions: Array<Record<string, unknown>> = [];
    const adapter = new ClaudeRuddrAdapter(
      (message) => {
        emitted.push(message);
      },
      (input) => {
        capturedOptions.push(input.options as unknown as Record<string, unknown>);
        const stream = new FakeSDKStream();
        streams.push(stream);
        return stream.query();
      },
    );
    await adapter.handle({ id: 1, method: "initialize", params: {} });
    await adapter.handle({ id: 2, method: "thread/start", params: { cwd: "/tmp", sandbox: "workspace-write" } });
    const threadID = responseResult(emitted, 2, "thread.id");

    const finishTurn = async (id: number, cost: number) => {
      await adapter.handle({ id, method: "turn/start", params: { threadId: threadID, input: [{ type: "text", text: `task ${id}` }] } });
      streams[streams.length - 1]!.push(
        sdk({ type: "result", subtype: "success", is_error: false, result: `done ${id}`, queued_turn_count: 0, session_id: threadID, uuid: `r${id}`, duration_ms: 1, duration_api_ms: 1, num_turns: 1, stop_reason: "end_turn", total_cost_usd: cost, usage: { input_tokens: 100, cache_read_input_tokens: 40, cache_creation_input_tokens: 10, output_tokens: 25 }, modelUsage: {}, permission_denials: [] }),
      );
      await waitFor(() => emitted.filter((message) => notification(message, "turn/completed")).length >= id - 2);
    };

    await finishTurn(3, 0.05);
    expect(capturedOptions[0]!.sessionId).toBe(threadID);
    expect(capturedOptions[0]!.resume).toBeUndefined();

    await finishTurn(4, 0.07);
    expect(capturedOptions[1]!.resume).toBe(threadID);
    expect(capturedOptions[1]!.sessionId).toBeUndefined();

    const usageNotifications = emitted.filter((message) => notification(message, "thread/tokenUsage/updated"));
    expect(usageNotifications.length).toBe(2);
    const last = usageNotifications[usageNotifications.length - 1] as { params: { threadId: string; tokenUsage: { total: Record<string, number> }; costUsd: number } };
    expect(last.params.threadId).toBe(threadID);
    expect(last.params.tokenUsage.total.inputTokens).toBe(300);
    expect(last.params.tokenUsage.total.cachedInputTokens).toBe(80);
    expect(last.params.tokenUsage.total.outputTokens).toBe(50);
    expect(last.params.tokenUsage.total.totalTokens).toBe(350);
    expect(last.params.costUsd).toBeCloseTo(0.12);
    const indexOfUsage = emitted.indexOf(usageNotifications[0]!);
    const indexOfCompleted = emitted.findIndex((message) => notification(message, "turn/completed"));
    expect(indexOfUsage).toBeLessThan(indexOfCompleted);
    await adapter.close();
  });

  test("ephemeral sessions refuse a second turn", async () => {
    const emitted: ProtocolMessage[] = [];
    const streams: FakeSDKStream[] = [];
    const adapter = new ClaudeRuddrAdapter(
      (message) => {
        emitted.push(message);
      },
      () => {
        const stream = new FakeSDKStream();
        streams.push(stream);
        return stream.query();
      },
    );
    await adapter.handle({ id: 1, method: "initialize", params: {} });
    await adapter.handle({ id: 2, method: "thread/start", params: { cwd: "/tmp", sandbox: "workspace-write", persistSession: false } });
    const threadID = responseResult(emitted, 2, "thread.id");
    await adapter.handle({ id: 3, method: "turn/start", params: { threadId: threadID, input: [{ type: "text", text: "one" }] } });
    streams[0]!.push(
      sdk({ type: "result", subtype: "success", is_error: false, result: "done", queued_turn_count: 0, session_id: threadID, uuid: "r", duration_ms: 1, duration_api_ms: 1, num_turns: 1, stop_reason: "end_turn", total_cost_usd: 0, usage: {}, modelUsage: {}, permission_denials: [] }),
    );
    await waitFor(() => emitted.some((message) => notification(message, "turn/completed")));
    await adapter.handle({ id: 4, method: "turn/start", params: { threadId: threadID, input: [{ type: "text", text: "two" }] } });
    const error = emitted.find((message) => "id" in message && message.id === 4 && "error" in message) as { error?: { message: string } } | undefined;
    expect(error?.error?.message).toContain("single turn");
    await adapter.close();
  });

  test("refuses a new query while the previous stream is still emitting", async () => {
  const emitted: ProtocolMessage[] = [];
  const streams: FakeSDKStream[] = [];
  const adapter = new ClaudeRuddrAdapter(
    (message) => {
    emitted.push(message);
    },
    () => {
    const stream = new FakeSDKStream(true);
    streams.push(stream);
    return stream.query();
    },
    5,
  );
  await adapter.handle({ id: 1, method: "initialize", params: {} });
  await adapter.handle({ id: 2, method: "thread/start", params: { cwd: "/tmp", sandbox: "workspace-write" } });
  const threadID = responseResult(emitted, 2, "thread.id");
  await adapter.handle({ id: 3, method: "turn/start", params: { threadId: threadID, input: [{ type: "text", text: "one" }] } });
  streams[0]!.push(
    sdk({ type: "result", subtype: "success", is_error: false, result: "done", queued_turn_count: 0, session_id: threadID, uuid: "r1", duration_ms: 1, duration_api_ms: 1, num_turns: 1, stop_reason: "end_turn", total_cost_usd: 0, usage: {}, modelUsage: {}, permission_denials: [] }),
  );
  await waitFor(() => emitted.some((message) => notification(message, "turn/completed")));
  await adapter.handle({ id: 4, method: "turn/start", params: { threadId: threadID, input: [{ type: "text", text: "two" }] } });
  const response = emitted.find((message) => "id" in message && message.id === 4);
  expect(response && "error" in response ? response.error?.message : "").toContain("did not settle");
  expect(streams).toHaveLength(1);
  streams[0]!.push(
    sdk({ type: "result", subtype: "success", is_error: false, result: "late", queued_turn_count: 0, session_id: threadID, uuid: "late", duration_ms: 1, duration_api_ms: 1, num_turns: 1, stop_reason: "end_turn", total_cost_usd: 0, usage: {}, modelUsage: {}, permission_denials: [] }),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(emitted.filter((message) => notification(message, "turn/completed"))).toHaveLength(1);
  await adapter.close();
  });

  test("uses cumulative modelUsage once for queued and error results", async () => {
  const emitted: ProtocolMessage[] = [];
  const stream = new FakeSDKStream();
  const adapter = new ClaudeRuddrAdapter(
    (message) => {
    emitted.push(message);
    },
    () => stream.query(),
  );
  await adapter.handle({ id: 1, method: "initialize", params: {} });
  await adapter.handle({ id: 2, method: "thread/start", params: { cwd: "/tmp", sandbox: "workspace-write" } });
  const threadID = responseResult(emitted, 2, "thread.id");
  await adapter.handle({ id: 3, method: "turn/start", params: { threadId: threadID, input: [{ type: "text", text: "one" }] } });
  const modelUsage = {
    "claude-main": { inputTokens: 10, cacheCreationInputTokens: 2, cacheReadInputTokens: 3, outputTokens: 4, webSearchRequests: 0, costUSD: 0.01, contextWindow: 200_000, maxOutputTokens: 8_000 },
    "claude-subagent": { inputTokens: 20, cacheCreationInputTokens: 5, cacheReadInputTokens: 7, outputTokens: 6, webSearchRequests: 0, costUSD: 0.02, contextWindow: 100_000, maxOutputTokens: 8_000 },
  };
  stream.push(
    sdk({ type: "result", subtype: "success", is_error: false, result: "queued", queued_turn_count: 1, session_id: threadID, uuid: "queued", duration_ms: 1, duration_api_ms: 1, num_turns: 1, stop_reason: "end_turn", total_cost_usd: 0.01, usage: { input_tokens: 999, output_tokens: 999 }, modelUsage: { "claude-main": modelUsage["claude-main"] }, permission_denials: [] }),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(emitted.filter((message) => notification(message, "thread/tokenUsage/updated"))).toHaveLength(0);
  stream.push(
    sdk({ type: "result", subtype: "error_during_execution", is_error: true, errors: ["model error"], terminal_reason: "model_error", queued_turn_count: 0, session_id: threadID, uuid: "final", duration_ms: 2, duration_api_ms: 2, num_turns: 2, stop_reason: null, total_cost_usd: 0.03, usage: { input_tokens: 999, output_tokens: 999 }, modelUsage, permission_denials: [] }),
  );
  await waitFor(() => emitted.some((message) => notification(message, "turn/completed")));
  const usage = emitted.find((message) => notification(message, "thread/tokenUsage/updated")) as { params: { tokenUsage: { total: Record<string, number>; modelContextWindow: number }; costUsd: number } };
  expect(usage.params.tokenUsage.total).toEqual({ inputTokens: 47, cachedInputTokens: 10, outputTokens: 10, totalTokens: 57 });
  expect(usage.params.tokenUsage.modelContextWindow).toBeUndefined();
  expect(usage.params.costUsd).toBeCloseTo(0.03);
  await adapter.close();
  });
});

function sdk(value: unknown): SDKMessage {
  return value as SDKMessage;
}

function textOf(message: ReturnType<typeof userMessage> | undefined): string {
  if (!message || !Array.isArray(message.message.content)) return "";
  const block = message.message.content[0];
  return typeof block === "object" && block !== null && "text" in block ? String(block.text) : "";
}

function responseResult(messages: ProtocolMessage[], id: string | number, path: string): string {
  const response = messages.find((message) => "id" in message && message.id === id);
  let value: unknown = response && "result" in response ? response.result : undefined;
  for (const key of path.split(".")) {
    value = typeof value === "object" && value !== null ? Reflect.get(value, key) : undefined;
  }
  if (typeof value !== "string")
    throw new Error(`missing ${path} for response ${id}: ${JSON.stringify(messages)}`);
  return value;
}

function notification(message: ProtocolMessage, method: string): boolean {
  return "method" in message && message.method === method;
}

async function waitFor(predicate: () => boolean): Promise<void> {
  const deadline = Date.now() + 1_000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("timed out waiting for adapter event");
    await Bun.sleep(1);
  }
}
