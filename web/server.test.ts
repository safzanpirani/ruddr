import { afterAll, beforeAll, describe, expect, spyOn, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, rm, stat, writeFile, appendFile, symlink, rename } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { eventStream, isSameOriginMutation, loadToken, parseWebArguments, readAlignedTail, WebApp } from "./server";

const TOKEN = "t".repeat(40);
let root: string;
let stateDir: string;
let argvLog: string;
let app: WebApp;

async function writeState(status: string, extra: Record<string, unknown> = {}) {
  await writeFile(
    join(stateDir, "state.json"),
    JSON.stringify({ version: 2, provider: "codex", pid: process.pid, status, threadId: "thread-1", turnId: "turn-1", cwd: root, stateDir, eventsPath: join(stateDir, "events.jsonl"), ...extra }),
  );
}

beforeAll(async () => {
  root = await mkdtemp(join(tmpdir(), "ruddr-web-test-"));
  stateDir = join(root, ".scratch", "run");
  await mkdir(stateDir, { recursive: true });
  argvLog = join(root, "argv.log");
  const fake = join(root, "fake-ruddr");
  // Records each call's argv and the message file it was given.
  await writeFile(fake, `#!/bin/sh\nprintf '%s\\n' "$*" >> '${argvLog}'\nfor a in "$@"; do if [ -f "$a" ]; then cat "$a" >> '${argvLog}'; fi; done\necho accepted\n`);
  await chmod(fake, 0o755);
  await writeState("active");
  await writeFile(join(stateDir, "events.jsonl"), "");
  const args = parseWebArguments(["--ruddr", fake, "--root", join(root, ".scratch")], {});
  app = new WebApp(args, TOKEN);
  await app.refreshSessions();
});

afterAll(async () => {
  await rm(root, { recursive: true, force: true });
});

const request = (path: string, init: RequestInit & { auth?: boolean; mutation?: boolean } = {}) => {
  const headers = new Headers(init.headers);
  headers.set("host", "127.0.0.1:4519");
  if (init.auth !== false) headers.set("cookie", `ruddr_web=${TOKEN}`);
  if (init.mutation) headers.set("x-ruddr-request", "1");
  return app.fetch(new Request(`http://127.0.0.1:4519${path}`, { ...init, headers }));
};

describe("auth", () => {
  test("rejects API calls without the token", async () => {
    expect((await request("/api/meta", { auth: false })).status).toBe(401);
    expect((await request("/api/meta")).status).toBe(200);
    const bearer = await app.fetch(new Request("http://x/api/meta", { headers: { authorization: `Bearer ${TOKEN}` } }));
    expect(bearer.status).toBe(200);
  });

  test("trades a valid query token for an HttpOnly cookie", async () => {
    const response = await request(`/?token=${TOKEN}`, { auth: false });
    expect(response.status).toBe(303);
    expect(response.headers.get("set-cookie")).toContain("HttpOnly; SameSite=Strict");
    expect((await request("/?token=wrong", { auth: false })).status).toBe(401);
  });

  test("requires the custom header and a matching origin on mutations", async () => {
    expect((await request("/api/stop", { method: "POST", body: "{}" })).status).toBe(403);
    const crossOrigin = new Request("http://127.0.0.1:4519/api/stop", { method: "POST", headers: { host: "127.0.0.1:4519", origin: "https://evil.example", "x-ruddr-request": "1" } });
    expect(isSameOriginMutation(crossOrigin)).toBe(false);
    const sameOrigin = new Request("http://127.0.0.1:4519/api/stop", { method: "POST", headers: { host: "127.0.0.1:4519", origin: "http://127.0.0.1:4519", "x-ruddr-request": "1" } });
    expect(isSameOriginMutation(sameOrigin)).toBe(true);
  });

  test("creates a private token file once", async () => {
    const file = join(root, "config", "web-token");
    const first = await loadToken(file);
    expect(await loadToken(file)).toBe(first);
    expect((await stat(file)).mode & 0o777).toBe(0o600);
  });
});

describe("sessions", () => {
  test("refuses to read files of unknown directories", async () => {
    expect((await request(`/api/run/output?dir=${encodeURIComponent("/etc")}`)).status).toBe(404);
    expect((await request(`/api/run/events?dir=${encodeURIComponent(root)}`)).status).toBe(404);
  });

  test("steers with the expected turn and never converts the route", async () => {
    const stale = await request("/api/prompt", { method: "POST", mutation: true, body: JSON.stringify({ stateDir, route: "prompt", message: "hi" }) });
    expect(stale.status).toBe(409);
    const moved = await request("/api/prompt", { method: "POST", mutation: true, body: JSON.stringify({ stateDir, route: "steer", turnId: "old", message: "hi" }) });
    expect(moved.status).toBe(409);
    const ok = await request("/api/prompt", { method: "POST", mutation: true, body: JSON.stringify({ stateDir, route: "steer", turnId: "turn-1", message: "go left" }) });
    expect(ok.status).toBe(200);
    const log = await readFile(argvLog, "utf8");
    expect(log).toContain(`steer --state-dir ${stateDir} --expected-turn-id turn-1 --message-file`);
    expect(log).toContain("go left");
  });

  test("interrupts active turns and stops idle sessions", async () => {
    await writeFile(argvLog, "");
    expect((await request("/api/stop", { method: "POST", mutation: true, body: JSON.stringify({ stateDir }) })).status).toBe(200);
    await writeState("idle");
    expect((await request("/api/stop", { method: "POST", mutation: true, body: JSON.stringify({ stateDir }) })).status).toBe(200);
    await writeState("completed");
    expect((await request("/api/stop", { method: "POST", mutation: true, body: JSON.stringify({ stateDir }) })).status).toBe(409);
    const log = await readFile(argvLog, "utf8");
    expect(log).toContain(`interrupt --state-dir ${stateDir}`);
    expect(log).toContain(`stop --state-dir ${stateDir}`);
    await writeState("active");
  });

  test("streams the event log as a reset and then appends whole lines", async () => {
    await writeFile(join(stateDir, "events.jsonl"), '{"a":1}\n{"b":');
    const controller = new AbortController();
    const response = await app.fetch(
      new Request(`http://x/api/run/events?dir=${encodeURIComponent(stateDir)}`, { headers: { cookie: `ruddr_web=${TOKEN}` }, signal: controller.signal }),
    );
    const reader = response.body!.getReader();
    const decoder = new TextDecoder();
    let text = "";
    const readUntil = async (needle: string) => {
      const deadline = Date.now() + 3000;
      while (!text.includes(needle) && Date.now() < deadline) text += decoder.decode((await reader.read()).value);
    };
    await readUntil("event: reset");
    expect(text).toContain('"text":"{\\"a\\":1}\\n"');
    await appendFile(join(stateDir, "events.jsonl"), '2}\n{"c":3}\n');
    await readUntil("event: append");
    expect(text).toContain('"text":"{\\"b\\":2}\\n{\\"c\\":3}\\n"');
    controller.abort();
    await reader.cancel().catch(() => {});
  });
});

describe("arguments", () => {
  test("parses flags and rejects bad values", () => {
    const args = parseWebArguments(["--host", "100.64.0.1", "--port", "0", "--interval", "2s", "--state-dir", "/tmp/x"], {});
    expect(args).toMatchObject({ host: "100.64.0.1", port: 0, interval: 2000, stateDirs: ["/tmp/x"] });
    expect(() => parseWebArguments(["--interval", "5"], {})).toThrow();
    expect(() => parseWebArguments(["--bogus"], {})).toThrow();
    expect(() => parseWebArguments([], { RUDDR_WEB_PORT: "NaN" })).toThrow();
  });
});


describe("auth regressions", () => {
  test("repairs existing permissions and preserves the token during concurrent creation", async () => {
    const file = join(root, "token-race");
    const tokens = await Promise.all(Array.from({ length: 8 }, () => loadToken(file)));
    expect(new Set(tokens).size).toBe(1);
    await chmod(file, 0o644);
    expect(await loadToken(file)).toBe(tokens[0]);
    expect((await stat(file)).mode & 0o777).toBe(0o600);
    const link = join(root, "token-link");
    await symlink(file, link);
    await expect(loadToken(link)).rejects.toThrow();
    const invalid = join(root, "invalid-token");
    await writeFile(invalid, "invalid");
    await expect(loadToken(invalid)).rejects.toThrow("invalid");
    expect(await readFile(invalid, "utf8")).toBe("invalid");
  });

  test("rejects malformed cookies and login bodies without throwing", async () => {
    expect((await request("/api/meta", { auth: false, headers: { cookie: "ruddr_web=%" } })).status).toBe(401);
    for (const token of [null, 42, {}, "wrong"]) {
      expect((await request("/api/login", { auth: false, method: "POST", mutation: true, body: JSON.stringify({ token }) })).status).toBe(401);
    }
    expect((await request("/api/login", { auth: false, method: "POST", mutation: true, body: "null" })).status).toBe(401);
  });

  test("requires same-origin login and rejects a scheme mismatch", async () => {
    expect((await request("/api/login", { auth: false, method: "POST", body: JSON.stringify({ token: TOKEN }) })).status).toBe(403);
    expect((await request("/api/login", { auth: false, method: "POST", mutation: true, headers: { origin: "https://evil.example" }, body: JSON.stringify({ token: TOKEN }) })).status).toBe(403);
    expect(isSameOriginMutation(new Request("http://localhost:4519/api/new", { headers: { host: "localhost:4519", origin: "https://localhost:4519", "x-ruddr-request": "1" } }))).toBe(false);
    const response = await request("/api/login", { auth: false, method: "POST", mutation: true, body: JSON.stringify({ token: TOKEN }) });
    expect(response.status).toBe(200);
    expect(response.headers.get("cache-control")).toBe("no-store");
  });
});

describe("session boundary regressions", () => {
  test("ignores artifact paths supplied by state metadata and refuses symlink artifacts", async () => {
    const secret = join(root, "outside-output");
    await writeFile(secret, "OUTSIDE");
    await writeFile(join(stateDir, "output.md"), "INSIDE");
    await writeState("completed", { outputPath: secret });
    await app.refreshSessions();
    const response = await request(`/api/run/output?dir=${encodeURIComponent(stateDir)}`);
    expect(await response.json()).toEqual({ text: "INSIDE" });
    await rm(join(stateDir, "output.md"));
    await symlink(secret, join(stateDir, "output.md"));
    expect((await request(`/api/run/output?dir=${encodeURIComponent(stateDir)}`)).status).toBe(500);
    await rm(join(stateDir, "output.md"));
    await writeState("active");
    await app.refreshSessions();
  });

  test("does not discover a stateDir outside the directory holding state.json", async () => {
    const forged = join(root, ".scratch", "forged");
    await mkdir(forged);
    await writeFile(join(forged, "state.json"), JSON.stringify({ pid: process.pid, status: "completed", stateDir: root }));
    await app.refreshSessions();
    expect(app.sessionFor(root)).toBeUndefined();
    await rm(forged, { recursive: true });
  });

  test("rejects steering without a turn ID and never relaunches rejected steers", async () => {
    await writeFile(argvLog, "");
    await writeState("active", { turnId: undefined });
    const response = await request("/api/prompt", { method: "POST", mutation: true, body: JSON.stringify({ stateDir, route: "steer", message: "left" }) });
    expect(response.status).toBe(409);
    expect(await readFile(argvLog, "utf8")).toBe("");
    await writeState("completed");
    const stale = await request("/api/prompt", { method: "POST", mutation: true, body: JSON.stringify({ stateDir, route: "steer", turnId: "turn-1", message: "left" }) });
    expect(stale.status).toBe(409);
    expect(await readFile(argvLog, "utf8")).toBe("");
    await writeState("active");
  });
});

describe("stream regressions", () => {
  test("keeps byte offsets and partial UTF-8 across tail boundaries", async () => {
    const file = join(root, "utf8-events");
    const bytes = Buffer.from('{"text":"é"}\n{"text":"😀"}\n');
    const partial = bytes.subarray(0, bytes.length - 4);
    await writeFile(file, partial);
    const tail = await readAlignedTail(file, partial.length - 1);
    expect(tail.offset).toBe(partial.length);
    expect(tail.text).toBe("");
    expect(Buffer.concat([tail.pending, bytes.subarray(partial.length)]).toString("utf8")).toBe('{"text":"😀"}\n');
  });

  test("cleans up cancellation, pre-aborted requests, and bounded slow readers", async () => {
    let starts = 0;
    let cleanups = 0;
    const controller = new AbortController();
    const request = new Request("http://localhost/", { signal: controller.signal });
    const removeListener = spyOn(request.signal, "removeEventListener");
    const response = eventStream(request, () => { starts++; return () => cleanups++; });
    await response.body!.cancel();
    controller.abort();
    expect(starts).toBe(1);
    expect(cleanups).toBe(1);
    expect(removeListener).toHaveBeenCalledWith("abort", expect.any(Function));
    removeListener.mockRestore();
    const aborted = eventStream(request, () => { starts++; return () => cleanups++; });
    expect((await aborted.body!.getReader().read()).done).toBe(true);
    expect(starts).toBe(1);
    const slow = eventStream(new Request("http://localhost/"), (send) => {
      for (let i = 0; i < 60; i++) send("append", { text: "x".repeat(1024 * 1024) });
      return () => cleanups++;
    });
    expect(cleanups).toBe(2);
    await slow.body!.cancel();
    expect(cleanups).toBe(2);
  });

  test("streams records larger than an append chunk and resets after rotation", async () => {
    const file = join(stateDir, "events.jsonl");
    await writeFile(file, "");
    const controller = new AbortController();
    const response = await request(`/api/run/events?dir=${encodeURIComponent(stateDir)}`, { signal: controller.signal });
    const reader = response.body!.getReader();
    const decoder = new TextDecoder();
    let pending = "";
    const nextEvent = async () => {
      while (true) {
        const end = pending.indexOf("\n\n");
        if (end >= 0) {
          const block = pending.slice(0, end); pending = pending.slice(end + 2);
          const event = /^event: (.+)$/m.exec(block)?.[1];
          const data = /^data: (.+)$/m.exec(block)?.[1];
          if (event && data) return { event, data: JSON.parse(data) };
          continue;
        }
        const value = await reader.read();
        if (value.done) throw new Error("stream closed");
        pending += decoder.decode(value.value, { stream: true });
      }
    };
    try {
      expect((await nextEvent()).event).toBe("reset");
      const record = JSON.stringify({ text: "é" + "x".repeat(4 * 1024 * 1024 + 100) }) + "\n";
      await appendFile(file, record);
      const append = await nextEvent();
      expect(append.event).toBe("append");
      expect(append.data.text).toBe(record);
      await rename(file, `${file}.old`);
      await writeFile(file, '{"new":true}\n');
      const reset = await nextEvent();
      expect(reset).toEqual({ event: "reset", data: { text: '{"new":true}\n', truncated: false } });
      await writeFile(file, '{}\n');
      expect((await nextEvent()).event).toBe("reset");
    } finally {
      controller.abort(); await reader.cancel().catch(() => {});
    }
  }, 10_000);
});


test("new sessions validate cwd and continuations launch detached with private artifacts", async () => {
  await writeFile(argvLog, "");
  const invalid = await request("/api/new", { method: "POST", mutation: true, body: JSON.stringify({ cwd: join(root, "missing"), message: "go" }) });
  expect(invalid.status).toBe(400);
  expect(await readFile(argvLog, "utf8")).toBe("");
  await writeState("completed");
  const response = await request("/api/prompt", { method: "POST", mutation: true, body: JSON.stringify({ stateDir, route: "continue", message: "follow-up" }) });
  expect(response.status).toBe(200);
  const result = await response.json() as { stateDir: string };
  expect((await stat(result.stateDir)).mode & 0o777).toBe(0o700);
  expect((await stat(join(result.stateDir, "prompt.md"))).mode & 0o777).toBe(0o600);
  expect((await stat(join(result.stateDir, "launch.stderr.log"))).mode & 0o777).toBe(0o600);
  const log = await readFile(argvLog, "utf8");
  expect(log).toContain("run --detach");
  expect(log).toContain("--resume-thread thread-1");
  expect(log).toContain("follow-up");
  await writeState("active");
});
