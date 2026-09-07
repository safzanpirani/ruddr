import { expect, test } from "bun:test";
import {
  HTTPBackend,
  OpenCodeRuddrAdapter,
  ruddrConfigContent,
  validatedLoopbackURL,
  type OpenCodeBackend,
  type OpenCodeSnapshot,
} from "./runtime";
import type { ProtocolMessage } from "../adapter/protocol";

class FakeBackend implements OpenCodeBackend {
  prompts: Array<{ text: string; delivery?: "steer" }> = [];
  interrupted = false;
  steerGate?: Promise<void>;
  private snapshots: OpenCodeSnapshot[] = [];
  private waiters: Array<(snapshot: OpenCodeSnapshot) => void> = [];

  async open(thread: { id: string }, resumed: boolean): Promise<string> {
    return resumed ? thread.id : "ses_test";
  }

  async prompt(_sessionID: string, text: string, delivery?: "steer"): Promise<string> {
    this.prompts.push({ text, delivery });
    if (delivery === "steer" && this.steerGate) await this.steerGate;
    return `msg_${this.prompts.length}`;
  }

  wait(): Promise<OpenCodeSnapshot> {
    const snapshot = this.snapshots.shift();
    if (snapshot) return Promise.resolve(snapshot);
    return new Promise((resolve) => {
      this.waiters.push(resolve);
    });
  }

  finish(snapshot: OpenCodeSnapshot): void {
    const waiter = this.waiters.shift();
    if (waiter) waiter(snapshot);
    else this.snapshots.push(snapshot);
  }

  async interrupt(): Promise<void> {
    this.interrupted = true;
  }

  async close(): Promise<void> {}
}

test("OpenCode adapter preserves same-turn steering and normalizes final output", async () => {
  const emitted: ProtocolMessage[] = [];
  const backend = new FakeBackend();
  const adapter = new OpenCodeRuddrAdapter((message) => {
    emitted.push(message);
  }, backend);
  await adapter.handle({ id: 1, method: "initialize", params: {} });
  await adapter.handle({
    id: 2,
    method: "thread/start",
    params: { cwd: "/tmp/work", sandbox: "workspace-write", model: "openrouter/deepseek/model" },
  });
  await adapter.handle({
    id: 3,
    method: "turn/start",
    params: { threadId: "ses_test", input: [{ type: "text", text: "first" }] },
  });
  const turnID = result(emitted, 3).turn.id as string;
  await adapter.handle({
    id: 4,
    method: "turn/steer",
    params: {
      threadId: "ses_test",
      expectedTurnId: turnID,
      input: [{ type: "text", text: "correction" }],
    },
  });
  expect(backend.prompts).toEqual([
    { text: "first", delivery: undefined },
    { text: "correction", delivery: "steer" },
  ]);
  expect(result(emitted, 4).turnId).toBe(turnID);

  backend.finish({
    outcome: "succeeded",
    messages: [
      {
        id: "msg_assistant",
        type: "assistant",
        content: [
          { type: "reasoning", text: "checked" },
          { type: "text", text: "OPENCODE_OK" },
        ],
      },
    ],
    tokens: { input: 10, output: 4, cache: { read: 2 } },
    cost: 0.01,
  });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  expect(JSON.stringify(notification(emitted, "item/completed"))).toContain("checked");
  expect(JSON.stringify(emitted)).toContain("OPENCODE_OK");
  expect(JSON.stringify(notification(emitted, "thread/tokenUsage/updated"))).toContain('"totalTokens":16');
  await adapter.close();
});

test("OpenCode waits for an accepted steer and the following idle snapshot", async () => {
  const emitted: ProtocolMessage[] = [];
  const backend = new FakeBackend();
  let releaseSteer = () => {};
  backend.steerGate = new Promise<void>((resolve) => {
    releaseSteer = resolve;
  });
  const adapter = new OpenCodeRuddrAdapter((message) => {
    emitted.push(message);
  }, backend);
  await adapter.handle({ id: 1, method: "initialize", params: {} });
  await adapter.handle({
    id: 2,
    method: "thread/start",
    params: { cwd: "/tmp/work", sandbox: "workspace-write" },
  });
  await adapter.handle({
    id: 3,
    method: "turn/start",
    params: { threadId: "ses_test", input: [{ type: "text", text: "first" }] },
  });
  const turnID = result(emitted, 3).turn.id as string;
  const steering = adapter.handle({
    id: 4,
    method: "turn/steer",
    params: {
      threadId: "ses_test",
      expectedTurnId: turnID,
      input: [{ type: "text", text: "correction" }],
    },
  });
  await waitFor(() => backend.prompts.length === 2);
  backend.finish({ outcome: "succeeded", messages: [{ id: "early", type: "assistant", content: [{ type: "text", text: "EARLY" }] }] });
  await Bun.sleep(10);
  expect(notification(emitted, "turn/completed")).toBeUndefined();

  releaseSteer();
  await steering;
  backend.finish({ outcome: "succeeded", messages: [{ id: "final", type: "assistant", content: [{ type: "text", text: "FINAL" }] }] });
  await waitFor(() => notification(emitted, "turn/completed") !== undefined);
  expect(JSON.stringify(emitted)).not.toContain("EARLY");
  expect(JSON.stringify(emitted)).toContain("FINAL");
  await adapter.close();
});

test("OpenCode HTTP requests time out and server announcements stay on loopback", async () => {
  const fetcher = ((_input: string | URL | Request, init?: RequestInit) =>
    new Promise<Response>((_resolve, reject) => {
      init?.signal?.addEventListener("abort", () => reject(init.signal?.reason), { once: true });
    })) as typeof fetch;
  const backend = new HTTPBackend(10, fetcher);
  Object.assign(backend, { baseURL: "http://127.0.0.1:4096", password: "private" });
  await expect(backend.prompt("ses_test", "hello")).rejects.toThrow("timed out after 10ms");

  expect(validatedLoopbackURL("http://localhost:4096/")).toBe("http://localhost:4096");
  expect(validatedLoopbackURL("http://[::1]:4096")).toBe("http://[::1]:4096");
  expect(() => validatedLoopbackURL("http://192.0.2.1:4096")).toThrow("loopback host");
  expect(() => validatedLoopbackURL("http://user:secret@127.0.0.1:4096")).toThrow("credentials");
  expect(() => validatedLoopbackURL("http://127.0.0.1:4096/api")).toThrow("must not contain");
  await backend.close();
});

test("OpenCode keeps response bodies bounded by timeout and shutdown", async () => {
  for (const closeEarly of [false, true]) {
    let bodyStarted!: () => void;
    const started = new Promise<void>((resolve) => { bodyStarted = resolve; });
    const fetcher = (async (_input: unknown, init?: RequestInit) => new Response(
      new ReadableStream({
        start(controller) {
          init?.signal?.addEventListener("abort", () => controller.error(init.signal!.reason), { once: true });
          controller.enqueue(new TextEncoder().encode('{"id":'));
          bodyStarted();
        },
      }),
    )) as typeof fetch;
    const backend = new HTTPBackend(closeEarly ? 10_000 : 20, fetcher);
    Object.assign(backend, { baseURL: "http://127.0.0.1:4096", password: "test" });
    const pending = backend.prompt("ses_test", "hello");
    const outcome = pending.catch((error: Error) => error);
    await started;
    // Let fetch resolve so the request is consuming its response body.
    await Promise.resolve();
    if (closeEarly) await backend.close();
    try {
      const error = await outcome;
      expect(error).toBeInstanceOf(Error);
      expect((error as Error).message).toContain(closeEarly ? "closing" : "timed out");
    } finally {
      await backend.close();
    }
  }
});

test("OpenCode adapter installs distinct Ruddr agents without discarding inline config", () => {
  const config = JSON.parse(ruddrConfigContent("read-only", JSON.stringify({ theme: "ruddr", agents: { existing: { mode: "primary" } } })));
  expect(config.theme).toBe("ruddr");
  expect(config.agents.existing.mode).toBe("primary");
  expect(config.agents["ruddr-read-only"].permissions).toContainEqual({ action: "*", resource: "*", effect: "deny" });
  expect(config.agents["ruddr-workspace-write"].permissions).toContainEqual({ action: "external_directory", resource: "*", effect: "deny" });
  expect(config.agents["ruddr-danger-full-access"].permissions).toEqual([{ action: "*", resource: "*", effect: "allow" }]);
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
