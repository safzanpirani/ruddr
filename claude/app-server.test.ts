import { expect, test } from "bun:test";

test("shared Claude transport preserves response IDs, errors, order, and EOF shutdown", async () => {
  const child = Bun.spawn([process.execPath, `${import.meta.dir}/app-server.ts`], {
    stdin: "pipe", stdout: "pipe", stderr: "pipe",
  });
  const timeout = setTimeout(() => child.kill(), 4000);
  try {
    child.stdin.write([
      "not JSON",
      JSON.stringify({ id: "init", method: "initialize", params: {} }),
      JSON.stringify({ id: 2, method: "thread/start", params: [] }),
      JSON.stringify({ id: null, method: "unsupported" }),
      JSON.stringify({ method: "unsupported" }),
      "",
    ].join("\n"));
    child.stdin.end();
    const [stdout, stderr, code] = await Promise.all([
      new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited,
    ]);
    expect(code).toBe(0);
    expect(stderr).toBe("");
    const messages = stdout.trim().split("\n").map((line) => JSON.parse(line));
    expect(messages).toHaveLength(5);
    expect(messages[0]).toMatchObject({ id: null, error: { code: -32700 } });
    expect(messages[1]).toMatchObject({ id: "init", result: { serverInfo: { name: "ruddr-claude-adapter" } } });
    expect(messages[2]).toMatchObject({ id: 2, error: { code: -32602 } });
    expect(messages[3]).toMatchObject({ id: null, error: { code: -32601 } });
    expect(messages[4]).toMatchObject({ method: "error" });
  } finally {
    clearTimeout(timeout);
    if (child.exitCode === null) {
      child.kill();
      await child.exited;
    }
  }
});
