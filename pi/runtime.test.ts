import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import { PiRuddrAdapter, SubprocessPiClient, type PiClient } from "./runtime";
import type { ProtocolMessage } from "../adapter/protocol";

class FakePiClient implements PiClient {
  commands: Record<string, unknown>[] = [];
  event?: (event: Record<string, unknown>) => void;
  contextTokens: number | null = 12;

  async start(config: { id: string }, onEvent: (event: Record<string, unknown>) => void): Promise<string> {
    this.event = onEvent;
    return config.id;
  }

  async send(command: Record<string, unknown>): Promise<Record<string, unknown>> {
    this.commands.push(command);
    if (command.type === "get_session_stats") {
      return {
        data: {
          tokens: { input: 20, output: 5, cacheRead: 3, cacheWrite: 0, totalTokens: 28 },
          cost: 0.02,
          contextUsage: { contextWindow: 1_000_000, tokens: this.contextTokens },
        },
      };
    }
    return { success: true };
  }

  async close(): Promise<void> {}
}

test("Pi adapter steers, reports tools, and completes after agent_settled", async () => {
  const emitted: ProtocolMessage[] = [];
  const client = new FakePiClient();
  const adapter = new PiRuddrAdapter((message) => {
    emitted.push(message);
  }, client);
  await adapter.handle({ id: 1, method: "initialize", params: {} });
  await adapter.handle({
    id: 2,
    method: "thread/start",
    params: { cwd: "/tmp/work", sandbox: "workspace-write", model: "openrouter/deepseek/model" },
  });
  const threadID = result(emitted, 2).thread.id as string;
  await adapter.handle({
    id: 3,
    method: "turn/start",
    params: { threadId: threadID, input: [{ type: "text", text: "first" }] },
  });
  const turnID = result(emitted, 3).turn.id as string;
  await adapter.handle({
    id: 4,
    method: "turn/steer",
    params: { threadId: threadID, expectedTurnId: turnID, input: [{ type: "text", text: "correction" }] },
  });
  expect(client.commands.slice(0, 2)).toEqual([
    { type: "prompt", message: "first" },
    { type: "steer", message: "correction" },
  ]);

  client.event?.({ type: "tool_execution_start", toolCallId: "tool-1", toolName: "read", args: { path: "README.md" } });
  client.event?.({ type: "tool_execution_end", toolCallId: "tool-1", toolName: "read", args: { path: "README.md" }, result: { content: [{ type: "text", text: "ok" }] }, isError: false });
  client.event?.({
    type: "message_end",
    message: {
      role: "assistant",
      content: [
        { type: "thinking", thinking: "verified" },
        { type: "text", text: "PI_OK" },
      ],
      stopReason: "stop",
    },
  });
  client.event?.({ type: "agent_settled" });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  expect(JSON.stringify(emitted)).toContain("PI_OK");
  expect(JSON.stringify(emitted)).toContain("tool-1");
  expect(JSON.stringify(notification(emitted, "thread/tokenUsage/updated"))).toContain('"totalTokens":28');
  expect(notification(emitted, "thread/tokenUsage/updated")).toMatchObject({ params: { tokenUsage: { last: { totalTokens: 12 } } } });
  await adapter.close();
});

test("Pi clears unknown context after compaction instead of reusing session totals", async () => {
  const emitted: ProtocolMessage[] = [];
  const client = new FakePiClient();
  client.contextTokens = null;
  const adapter = new PiRuddrAdapter((message) => { emitted.push(message); }, client);
  try {
    await adapter.handle({ id: 1, method: "initialize", params: {} });
    await adapter.handle({ id: 2, method: "thread/start", params: { cwd: "/tmp", sandbox: "read-only" } });
    const threadId = result(emitted, 2).thread.id;
    await adapter.handle({ id: 3, method: "turn/start", params: { threadId, input: [{ type: "text", text: "hello" }] } });
    client.event?.({ type: "agent_settled" });
    await waitFor(() => notification(emitted, "turn/completed") !== undefined);
    const usage = notification(emitted, "thread/tokenUsage/updated") as { params: { tokenUsage: { last?: unknown } } };
    expect(usage.params.tokenUsage.last).toBeUndefined();
  } finally {
    await adapter.close();
  }
});

test("Pi waits for an accepted steer and the following settled event", async () => {
  const emitted: ProtocolMessage[] = [];
  const client = new FakePiClient();
  let releaseSteer = () => {};
  const steerGate = new Promise<void>((resolve) => {
    releaseSteer = resolve;
  });
  const originalSend = client.send.bind(client);
  client.send = async (command) => {
    if (command.type === "steer") await steerGate;
    return originalSend(command);
  };
  const adapter = new PiRuddrAdapter((message) => {
    emitted.push(message);
  }, client);
  await adapter.handle({ id: 1, method: "initialize", params: {} });
  await adapter.handle({
    id: 2,
    method: "thread/start",
    params: { cwd: "/tmp/work", sandbox: "workspace-write", model: "openrouter/deepseek/model" },
  });
  const threadID = result(emitted, 2).thread.id as string;
  await adapter.handle({
    id: 3,
    method: "turn/start",
    params: { threadId: threadID, input: [{ type: "text", text: "first" }] },
  });
  const turnID = result(emitted, 3).turn.id as string;
  const steering = adapter.handle({
    id: 4,
    method: "turn/steer",
    params: { threadId: threadID, expectedTurnId: turnID, input: [{ type: "text", text: "correction" }] },
  });
  await Bun.sleep(5);
  client.event?.({ type: "agent_settled" });
  await Bun.sleep(10);
  expect(notification(emitted, "turn/completed")).toBeUndefined();

  releaseSteer();
  await steering;
  await Bun.sleep(10);
  expect(notification(emitted, "turn/completed")).toBeUndefined();
  client.event?.({ type: "agent_settled" });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  await adapter.close();
});

test("Pi RPC rejects interactive extension UI and times out unanswered commands", async () => {
  const client = new SubprocessPiClient(500);
  const events: Array<Record<string, unknown>> = [];
  const executable = fileURLToPath(new URL("testdata/fake-pi.ts", import.meta.url));
  const sessionID = await client.start(
    { id: "", cwd: "/tmp", executable, sandbox: "workspace-write", ephemeral: false, resumed: false },
    (event) => events.push(event),
  );
  expect(sessionID).toBe("pi_test_session");
  await waitFor(() => events.filter((event) => event.type === "test_ui_response").length === 2);
  const responses = events
    .filter((event) => event.type === "test_ui_response")
    .map((event) => event.response);
  expect(responses).toContainEqual({ type: "extension_ui_response", id: "ui-select", cancelled: true });
  expect(responses).toContainEqual({ type: "extension_ui_response", id: "ui-unknown", cancelled: true });
  expect(JSON.stringify(responses)).not.toContain("ui-notify");

  await expect(client.send({ type: "never_respond" })).rejects.toThrow("timed out after 500ms");
  await waitFor(() => events.some((event) => event.type === "ruddr_error"));
  await client.close();
});

function result(messages: ProtocolMessage[], id: string | number): Record<string, any> {
  return (messages.find((message) => "id" in message && message.id === id) as { result: Record<string, any> }).result;
}

function notification(messages: ProtocolMessage[], method: string): ProtocolMessage | undefined {
  return messages.find((message) => "method" in message && message.method === method);
}

async function waitFor(predicate: () => boolean): Promise<void> {
  for (let index = 0; index < 100; index++) {
    if (predicate()) return;
    await Bun.sleep(5);
  }
  throw new Error("condition was not met");
}
