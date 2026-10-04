// Opt-in browser proof against the built Rust server. Never submits a prompt.
// RUDDR_WEB_BROWSER_TEST=1 bun test web/browser.test.ts
import { expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile, rename, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const browserTest = process.env.RUDDR_WEB_BROWSER_TEST === "1" ? test : test.skip;
browserTest("draft reload targets and edit cards on the real server", async () => {
  const root = await mkdtemp(join(tmpdir(), "ruddr-web-client-"));
  const run = join(root, "finished");
  await mkdir(run, { mode: 0o700 });
  const state = {
    version: 1, provider: "codex", pid: 0, status: "completed", threadId: "fixture-thread", turnId: "old-turn",
    model: "fixture", cwd: root, sandbox: "read-only", stateDir: run, socketPath: "",
    eventsPath: join(run, "events.jsonl"), tracePath: join(run, "trace.log"), outputPath: join(run, "output.md"),
    stderrPath: join(run, "stderr.log"), steers: 0, startedAt: "2026-10-04T12:00:00Z", updatedAt: "2026-10-04T12:00:01Z",
  };
  const saveState = () => writeFile(join(run, "state.json"), JSON.stringify(state), { mode: 0o600 });
  await saveState();
  const items = [
    { id: "patch", type: "fileChange", status: "completed", toolName: "apply_patch", input: { patch:
      '*** Begin Patch\n*** Update File: example.ts\n@@ function greet\n export function greet() {\n-  return "old";\n+  const name = "reader";\n+  return `Hello, ${name}`;\n }\n*** End Patch' } },
    { id: "write", type: "fileChange", status: "completed", toolName: "Write", input: { file_path: "notes.md", content: "# Supplied content\n\nPrior file content was not provided.\n" } },
  ];
  await writeFile(join(run, "events.jsonl"), items.map(item => JSON.stringify({ method: "item/completed", params: { threadId: state.threadId, item } })).join("\n") + "\n", { mode: 0o600 });
  for (const name of ["trace.log", "output.md", "stderr.log"]) await writeFile(join(run, name), "", { mode: 0o600 });
  const server = Bun.spawn([
    process.env.RUDDR_TEST_BINARY ?? resolve("target/debug/ruddr"), "web", "--host", "127.0.0.1", "--port", "0",
    "--root", root, "--token-file", join(root, "web-token"),
  ], { cwd: root, env: { ...process.env, RUDDR_NO_UPDATE_CHECK: "1", RUDDR_REGISTRY_DIR: join(root, "registry"), XDG_CONFIG_HOME: join(root, "config") }, stdout: "pipe", stderr: "pipe" });
  const session = `ruddr-web-test-${server.pid}`;
  const browser = (args: string[], input?: string) => {
    const result = Bun.spawnSync(["agent-browser", "--session", session, ...args], { stdin: input === undefined ? undefined : Buffer.from(input), stdout: "pipe", stderr: "pipe", timeout: 30000 });
    // Avoid printing the input: the initial navigation contains the private token.
    if (result.exitCode !== 0) throw new Error(`agent-browser ${args[0]} failed (${session}): ${result.stderr.toString()}`);
    return result.stdout.toString();
  };
  const evaluate = (source: string) => {
    const output = browser(["eval", "--stdin"], source);
    if (output.includes('"success":false')) throw new Error("browser assertion failed");
    return output;
  };
  const wait = (source: string) => browser(["wait", "--fn", source]);
  const syncSession = () => wait(`fetch('/api/sessions').then(r => r.json()).then(s => s.some(x => x.stateDir === ${JSON.stringify(run)} && x.status === ${JSON.stringify(state.status)} && x.turnId === ${JSON.stringify(state.turnId)}))`);
  const seed = (value: unknown) => evaluate(`localStorage.setItem('ruddr.selected', ${JSON.stringify(JSON.stringify(run))}); localStorage.setItem('ruddr.drafts', ${JSON.stringify(JSON.stringify({ [run]: value }))}); true`);
  const check = (route: string, text: string) => {
    wait(`document.querySelector('.composer-input')?.value === ${JSON.stringify(text)}`);
    evaluate(`if(document.querySelector('.composer').dataset.route !== ${JSON.stringify(route)} || document.querySelector('.send').disabled !== ${route === "none"}) throw new Error('wrong draft target'); if(${route === "none"} && document.querySelector('.draft-note').getBoundingClientRect().height === 0) throw new Error('missing target note'); if(performance.getEntriesByType('resource').some(e => e.name.endsWith('/api/prompt'))) throw new Error('unexpected prompt request'); true`);
    console.log(`browser: restored ${route}; retained text; verified Send state; no prompt request`);
  };
  const target = (route: string, turnId?: string) => ({ stateDir: run, threadId: state.threadId, route, turnId });
  try {
    let output = "";
    const reader = server.stdout.getReader();
    const url = await Promise.race([
      (async () => {
        while (true) {
          const chunk = await reader.read();
          if (chunk.done) throw new Error("web server exited before printing its URL");
          output += new TextDecoder().decode(chunk.value);
          const match = /Open (http:\/\/\S+)\n/.exec(output);
          if (match) return match[1];
        }
      })(),
      Bun.sleep(15000).then(() => { throw new Error("web server startup timed out"); }),
    ]);
    const origin = new URL(url).origin;
    const cookies = join(root, "cookies");
    const curl = (address: string) => {
      const config = `url = ${JSON.stringify(address)}\ncookie-jar = ${JSON.stringify(cookies)}\ncookie = ${JSON.stringify(cookies)}\nlocation\nsilent\nshow-error\nfail\nmax-time = 10\n`;
      const result = Bun.spawnSync(["curl", "--config", "-"], { stdin: Buffer.from(config), stdout: "pipe", stderr: "pipe" });
      if (result.exitCode !== 0) throw new Error("curl HTTP check failed");
      return result.stdout.toString();
    };
    const html = curl(url);
    const entry = /src="\.([^\"]+\.js)"/.exec(html)?.[1];
    expect(entry).toBeDefined();
    const served = curl(origin + entry);
    expect(served).toBe(await Bun.file(resolve("crates/ruddr-web/assets/client" + entry)).text());
    expect(served).toContain("The original draft target");
    expect(served).toContain("write/overwrite");
    expect(JSON.parse(curl(origin + "/api/sessions"))[0].stateDir).toBe(run);
    console.log("curl: token exchange and authenticated sessions passed; served bundle matches build and contains both changes");
    browser(["open", origin]);
    evaluate(`void(location.href = ${JSON.stringify(url)})`);
    wait("document.querySelector('.composer')?.dataset.route === 'continue'");
    seed({ text: "Valid continuation", target: target("continue") });
    browser(["reload"]); check("continue", "Valid continuation");
    seed({ text: "Old steer", target: target("steer", "old-turn") });
    browser(["reload"]); check("none", "Old steer");
    browser(["type", ".composer-input", " with an edit"]); check("none", "Old steer with an edit");
    browser(["reload"]); check("none", "Old steer with an edit");
    browser(["click", ".draft-note button"]); check("continue", "Old steer with an edit");
    browser(["reload"]); check("continue", "Old steer with an edit");
    seed("Legacy draft"); browser(["reload"]); check("none", "Legacy draft");
    state.status = "active"; state.pid = server.pid; await saveState(); syncSession();
    seed({ text: "Pinned active turn", target: target("steer", "old-turn") });
    browser(["reload"]); check("steer", "Pinned active turn");
    state.turnId = "next-turn"; await saveState(); syncSession();
    browser(["reload"]); check("none", "Pinned active turn");
    await rename(join(run, "state.json"), join(run, "state.hidden"));
    wait("fetch('/api/sessions').then(r => r.json()).then(s => s.length === 0)");
    browser(["reload"]); check("none", "Pinned active turn");
    wait("document.querySelector('.composer-input').getBoundingClientRect().height > 0");
    browser(["reload"]); check("none", "Pinned active turn");
    await rename(join(run, "state.hidden"), join(run, "state.json"));
    state.status = "completed"; state.pid = 0; await saveState(); syncSession();
    seed({ text: "Draft for the old running turn", target: target("steer", "old-turn") });
    browser(["reload"]); check("none", "Draft for the old running turn");
    wait("document.querySelectorAll('.edit-card').length === 2 && document.querySelector('.edit-card.update diffs-container')?.shadowRoot?.querySelector('code')");
    evaluate(`const patch = document.querySelector('.edit-card.update'); const write = document.querySelector('.edit-card.write'); if(!write.textContent.includes('write/overwrite') || !write.textContent.includes('prior content unknown') || write.querySelector('.add')) throw new Error('dishonest write'); const roots = [...patch.querySelectorAll('*')].map(e => e.shadowRoot).filter(Boolean); if(!roots.some(r => r.querySelector('pre[data-disable-line-numbers]'))) throw new Error('fragment coordinates'); true`);
    wait("document.querySelector('.edit-card.update diffs-container').shadowRoot.textContent.includes('Hello,')");
    evaluate("(async () => { await document.fonts.ready; await Promise.all(document.getAnimations().filter(a => a.effect?.getComputedTiming().iterations !== Infinity).map(a => a.finished.catch(() => {}))); await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))); return true; })()");
    const screenshotDir = process.env.RUDDR_WEB_SCREENSHOTS;
    if (screenshotDir) {
      await mkdir(screenshotDir, { recursive: true });
      browser(["screenshot", join(screenshotDir, "restored-draft-and-edits.png")]);
    }
    expect(server.exitCode).toBeNull();
    console.log("browser: fragment and write cards verified; empty-dashboard draft stays visible");
  } finally {
    try { browser(["close"]); } finally {
      server.kill("SIGTERM");
      await server.exited;
      await rm(root, { recursive: true, force: true });
    }
  }
}, 120000);
