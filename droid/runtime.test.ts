import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import { DroidRuddrAdapter, SubprocessDroidClient, type DroidClient } from "./runtime";
import type { ProtocolMessage } from "../adapter/protocol";

class FakeDroidClient implements DroidClient {
  requests: Array<{ method: string; params: Record<string, unknown>; id?: string }> = [];
  event?: (params: Record<string, unknown>) => void;
  executable?: string;

  async start(executable: string, _cwd: string, onEvent: (params: Record<string, unknown>) => void): Promise<void> {
    this.executable = executable;
    this.event = onEvent;
  }

  async request(
    method: string,
    params: Record<string, unknown>,
    options: { id?: string } = {},
  ): Promise<Record<string, unknown>> {
    this.requests.push({ method, params, ...(options.id ? { id: options.id } : {}) });
    if (method === "droid.initialize_session") return { sessionId: "droid-session" };
    if (method === "droid.fork_session") return { newSessionId: "droid-fork" };
    if (method === "droid.get_context_stats") return { used: 40, remaining: 960, limit: 1000 };
    return {};
  }

  notify(notification: Record<string, unknown>, sessionId = "droid-session"): void {
    this.event?.({ sessionId, notification });
  }

  async close(): Promise<void> {}
}

async function startThread(
  sandbox = "workspace-write",
): Promise<{ adapter: DroidRuddrAdapter; client: FakeDroidClient; emitted: ProtocolMessage[]; threadId: string; turnId: string }> {
  const emitted: ProtocolMessage[] = [];
  const client = new FakeDroidClient();
  const adapter = new DroidRuddrAdapter((message) => {
    emitted.push(message);
  }, client);
  await adapter.handle({ id: 1, method: "initialize", params: {} });
  await adapter.handle({
    id: 2,
    method: "thread/start",
    params: { cwd: "/tmp/work", sandbox, model: "glm-5.3-flash", providerPath: "/opt/droid", ephemeral: false },
  });
  const threadId = result(emitted, 2).thread.id as string;
  await adapter.handle({ id: 3, method: "turn/start", params: { threadId, input: [{ type: "text", text: "first" }] } });
  const turnId = result(emitted, 3).turn.id as string;
  return { adapter, client, emitted, threadId, turnId };
}

test("Droid adapter starts a session, reports tools and messages, and completes the turn", async () => {
  const { adapter, client, emitted, threadId } = await startThread();
  expect(threadId).toBe("droid-session");
  expect(client.executable).toBe("/opt/droid");
  expect(client.requests[0]).toMatchObject({
    method: "droid.initialize_session",
    params: { cwd: "/tmp/work", modelId: "glm-5.3-flash", autonomyLevel: "medium", autoRejectPermissionRequests: true },
  });
  expect(client.requests[1]).toEqual({ method: "droid.add_user_message", params: { text: "first" } });

  client.notify({ type: "tool_call", toolUse: { id: "tool-1", name: "Execute", input: {} } });
  client.notify({ type: "tool_call", toolUse: { id: "tool-1", name: "Execute", input: { command: "echo one" } } });
  client.notify({ type: "tool_result", toolUseId: "tool-1", content: "one\n", isError: false });
  // A subagent's events carry its own session ID and stay out of the turn.
  client.notify({ type: "tool_call", toolUse: { id: "child-tool", name: "Read", input: {} } }, "child-session");
  client.notify({ type: "assistant_text_delta", messageId: "m-2", blockIndex: 1, textDelta: "DROID" });
  client.notify({
    type: "create_message",
    message: { id: "m-2", role: "assistant", content: [{ type: "thinking", thinking: "checked" }, { type: "text", text: "DROID_OK" }] },
  });
  client.notify({
    type: "session_token_usage_changed",
    sessionId: "droid-session",
    tokenUsage: { inputTokens: 100, outputTokens: 10, cacheReadTokens: 50, cacheCreationTokens: 0 },
    lastCallTokenUsage: { inputTokens: 20, outputTokens: 5, cacheReadTokens: 10 },
  });
  client.notify({ type: "agent_turn_completed", reason: "completed", turnId: "droid-turn" });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);

  const items = completedItems(emitted);
  expect(items.find((item) => item.id === "tool-1")).toMatchObject({
    type: "commandExecution",
    status: "completed",
    command: "echo one",
    aggregatedOutput: "one\n",
  });
  expect(JSON.stringify(emitted)).not.toContain("child-tool");
  expect(notification(emitted, "item/updated")).toMatchObject({ params: { item: { id: "tool-1", command: "echo one" } } });
  expect(notification(emitted, "item/agentMessage/delta")).toMatchObject({ params: { itemId: "m-2-text-1", delta: "DROID" } });
  expect(items.find((item) => item.type === "reasoning")).toMatchObject({ summary: [{ text: "checked" }] });
  expect(items.find((item) => item.type === "agentMessage")).toMatchObject({ id: "m-2-text-1", text: "DROID_OK", phase: "final_answer" });
  const usage = notifications(emitted, "thread/tokenUsage/updated").at(-1);
  expect(usage).toMatchObject({
    params: {
      tokenUsage: {
        total: { inputTokens: 150, cachedInputTokens: 50, outputTokens: 10, totalTokens: 160 },
        last: { totalTokens: 40 },
        modelContextWindow: 1000,
      },
    },
  });
  expect(notification(emitted, "turn/completed")).toMatchObject({ params: { turn: { status: "completed" } } });
  await adapter.close();
});

test("Droid narration beside a tool call is commentary", async () => {
  const { adapter, client, emitted } = await startThread();
  client.notify({ type: "create_message", message: { id: "m-1", role: "assistant", content: [{ type: "text", text: "looking" }] } });
  client.notify({ type: "tool_call", toolUse: { id: "tool-1", name: "Create", input: { file_path: "/tmp/work/a.txt" } } });
  client.notify({ type: "tool_result", toolUseId: "tool-1", content: "ok", isError: false });
  client.notify({ type: "create_message", message: { id: "m-2", role: "assistant", content: [{ type: "text", text: "done" }] } });
  client.notify({ type: "agent_turn_completed", reason: "completed" });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  const items = completedItems(emitted);
  expect(items.filter((item) => item.type === "agentMessage").map((item) => [item.text, item.phase])).toEqual([
    ["looking", "commentary"],
    ["done", "final_answer"],
  ]);
  expect(items.find((item) => item.id === "tool-1")).toMatchObject({ type: "fileChange", command: "Create /tmp/work/a.txt" });
  await adapter.close();
});

test("Droid keeps the turn open until a queued steer runs", async () => {
  const { adapter, client, emitted, threadId, turnId } = await startThread();
  await adapter.handle({
    id: 4,
    method: "turn/steer",
    params: { threadId, expectedTurnId: turnId, input: [{ type: "text", text: "correction" }] },
  });
  expect(result(emitted, 4)).toEqual({ turnId });
  const steer = client.requests.find((request) => request.method === "droid.add_user_message" && request.params.text === "correction");
  expect(steer?.id).toBeDefined();
  expect(completedItems(emitted).filter((item) => item.type === "userMessage").map((item) => item.text)).toEqual(["correction"]);

  // Droid finished its turn before it read the steer, then runs the steer as
  // a new Droid turn.
  client.notify({ type: "agent_turn_completed", reason: "completed" });
  await Bun.sleep(20);
  expect(notification(emitted, "turn/completed")).toBeUndefined();
  client.notify({ type: "create_message", requestId: steer!.id, message: { id: "u-2", role: "user", content: [{ type: "text", text: "correction" }] } });
  client.notify({ type: "create_message", message: { id: "m-3", role: "assistant", content: [{ type: "text", text: "corrected" }] } });
  client.notify({ type: "agent_turn_completed", reason: "completed" });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  expect(notifications(emitted, "turn/completed")).toHaveLength(1);
  expect(completedItems(emitted).find((item) => item.type === "agentMessage")).toMatchObject({ text: "corrected", phase: "final_answer" });
  await adapter.close();
});

test("Droid ends a deferred turn when it discards the queued steer", async () => {
  const { adapter, client, emitted, threadId, turnId } = await startThread();
  await adapter.handle({
    id: 4,
    method: "turn/steer",
    params: { threadId, expectedTurnId: turnId, input: [{ type: "text", text: "late" }] },
  });
  client.notify({ type: "agent_turn_completed", reason: "completed" });
  await Bun.sleep(20);
  expect(notification(emitted, "turn/completed")).toBeUndefined();
  client.notify({ type: "queued_messages_discarded" });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  expect(notification(emitted, "turn/completed")).toMatchObject({ params: { turn: { status: "completed" } } });
  await adapter.close();
});

test("Droid interrupts and reports the turn as interrupted", async () => {
  const { adapter, client, emitted, threadId, turnId } = await startThread();
  await adapter.handle({ id: 4, method: "turn/interrupt", params: { threadId, turnId } });
  expect(client.requests.at(-1)).toEqual({ method: "droid.interrupt_session", params: {} });
  client.notify({ type: "agent_turn_completed", reason: "cancelled" });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  expect(notification(emitted, "turn/completed")).toMatchObject({ params: { turn: { id: turnId, status: "interrupted" } } });
  await adapter.close();
});

test("Droid reports a rejected permission as a failed turn", async () => {
  const { adapter, client, emitted } = await startThread("read-only");
  expect(client.requests[0]).toMatchObject({ params: { autonomyLevel: "off", autoRejectPermissionRequests: true } });
  client.notify({ type: "tool_call", toolUse: { id: "tool-1", name: "Execute", input: { command: "echo hi" } } });
  client.notify({ type: "agent_turn_completed", reason: "permission_rejected" });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  expect(notification(emitted, "turn/completed")).toMatchObject({
    params: { turn: { status: "failed", error: { message: expect.stringContaining("autonomy off") } } },
  });
  // A tool left open by the turn's end gets a terminal status.
  expect(completedItems(emitted).find((item) => item.id === "tool-1")).toMatchObject({ status: "failed" });
  await adapter.close();
});

test("Droid resumes and forks sessions and rejects unsupported thread options", async () => {
  const resumed: ProtocolMessage[] = [];
  const resumeClient = new FakeDroidClient();
  const resumeAdapter = new DroidRuddrAdapter((message) => { resumed.push(message); }, resumeClient);
  await resumeAdapter.handle({ id: 1, method: "initialize", params: {} });
  await resumeAdapter.handle({
    id: 2,
    method: "thread/resume",
    params: { threadId: "old-session", cwd: "/tmp", sandbox: "danger-full-access", model: "glm-5.3-flash", excludeTurns: true },
  });
  expect(result(resumed, 2)).toEqual({ thread: { id: "old-session" } });
  expect(resumeClient.requests.map((request) => request.method)).toEqual(["droid.load_session", "droid.update_session_settings"]);
  expect(resumeClient.requests[0]!.params).toEqual({ sessionId: "old-session", autoRejectPermissionRequests: true });
  expect(resumeClient.requests[1]!.params).toEqual({ modelId: "glm-5.3-flash", autonomyLevel: "high" });
  await resumeAdapter.close();

  const forked: ProtocolMessage[] = [];
  const forkClient = new FakeDroidClient();
  const forkAdapter = new DroidRuddrAdapter((message) => { forked.push(message); }, forkClient);
  await forkAdapter.handle({ id: 1, method: "initialize", params: {} });
  await forkAdapter.handle({ id: 2, method: "thread/fork", params: { threadId: "old-session", cwd: "/tmp", sandbox: "workspace-write", lastTurnId: "t-1" } });
  expect(JSON.stringify(forked)).toContain("--fork-through-turn");
  await forkAdapter.handle({ id: 3, method: "thread/fork", params: { threadId: "old-session", cwd: "/tmp", sandbox: "workspace-write" } });
  expect(result(forked, 3)).toEqual({ thread: { id: "droid-fork" } });
  expect(forkClient.requests.map((request) => [request.method, request.params.sessionId])).toEqual([
    ["droid.load_session", "old-session"],
    ["droid.fork_session", undefined],
    ["droid.load_session", "droid-fork"],
    ["droid.update_session_settings", undefined],
  ]);
  await forkAdapter.close();

  const ephemeral: ProtocolMessage[] = [];
  const ephemeralAdapter = new DroidRuddrAdapter((message) => { ephemeral.push(message); }, new FakeDroidClient());
  await ephemeralAdapter.handle({ id: 1, method: "initialize", params: {} });
  await ephemeralAdapter.handle({ id: 2, method: "thread/start", params: { cwd: "/tmp", sandbox: "read-only", ephemeral: true } });
  expect(JSON.stringify(ephemeral)).toContain("--ephemeral is not supported");
  await ephemeralAdapter.close();
});

test("Droid client declines interactive requests and times out unanswered calls", async () => {
  const client = new SubprocessDroidClient(500);
  const events: Array<Record<string, unknown>> = [];
  const executable = fileURLToPath(new URL("testdata/fake-droid.ts", import.meta.url));
  await client.start(executable, "/tmp", (event) => events.push(event));
  const started = await client.request("droid.initialize_session", { machineId: "test", cwd: "/tmp" }, { timeoutMs: 30_000 });
  expect(started).toEqual({ sessionId: "droid_test_session" });
  const responses = () => events
    .map((event) => event.notification as Record<string, unknown>)
    .filter((notification) => notification.type === "test_server_response")
    .map((notification) => notification.response as Record<string, unknown>);
  await waitFor(() => responses().length === 3);
  expect(responses()).toContainEqual(expect.objectContaining({ id: "perm-1", type: "response", result: { selectedOption: "cancel" } }));
  expect(responses()).toContainEqual(expect.objectContaining({ id: "ask-1", result: { cancelled: true, answers: [] } }));
  expect(responses()).toContainEqual(expect.objectContaining({ id: "future-1", error: expect.objectContaining({ code: -32601 }) }));

  await expect(client.request("droid.fail", {})).rejects.toThrow("session not found");
  await expect(client.request("droid.never_respond", {})).rejects.toThrow("timed out after 500ms");
  await waitFor(() => events.some((event) => (event.notification as Record<string, unknown>)?.type === "ruddr_error"));
  await client.close();
});

function result(messages: ProtocolMessage[], id: string | number): Record<string, any> {
  return (messages.find((message) => "id" in message && message.id === id) as { result: Record<string, any> }).result;
}

function notification(messages: ProtocolMessage[], method: string): ProtocolMessage | undefined {
  return messages.find((message) => "method" in message && message.method === method);
}

function notifications(messages: ProtocolMessage[], method: string): ProtocolMessage[] {
  return messages.filter((message) => "method" in message && message.method === method);
}

function completedItems(messages: ProtocolMessage[]): Array<Record<string, any>> {
  return notifications(messages, "item/completed").map((message) => (message as { params: { item: Record<string, any> } }).params.item);
}

async function waitFor(predicate: () => boolean, timeoutMs = 15_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await Bun.sleep(5);
  }
  throw new Error("condition was not met");
}
