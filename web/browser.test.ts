import { expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { isAuthorized, isSameOriginMutation, parseWebArguments, WebApp } from "./server";

// Opt in with RUDDR_WEB_BROWSER_TEST=1 bun test web/browser.test.ts.
// The fixture binds an ephemeral port and mocks every agent-start/control request.
test.skipIf(process.env.RUDDR_WEB_BROWSER_TEST !== "1")("browser rejects stale UI responses and duplicate submissions", async () => {
  const directory = await mkdtemp(join(tmpdir(), "ruddr-web-browser-"));
  const token = "fixture-token-".repeat(4);
  const sessionName = `ruddr-review-${process.pid}`;
  const args = parseWebArguments(["--port", "0", "--root", directory, "--ruddr", "/nonexistent-review-fake"], {});
  const app = new WebApp(args, token);
  let prompts = 0;
  let launches = 0;
  let lastPrompt: Record<string, unknown> = {};
  let outputReads = 0;
  for (const name of ["alpha", "beta"]) {
    const stateDir = join(directory, name);
    await mkdir(stateDir, { mode: 0o700 });
    await writeFile(join(stateDir, "events.jsonl"), JSON.stringify({ method: "item/completed", params: { threadId: name, item: { id: name, type: "agentMessage", text: `${name} message <img src=x onerror=alert(1)>` } } }) + "\n", { mode: 0o600 });
    await writeFile(join(stateDir, "output.md"), `${name} output`, { mode: 0o600 });
    app.sessions.push({ version: 2, pid: process.pid, stateDir, stateFile: join(stateDir, "state.json"), status: "active", threadId: name, turnId: "original", cwd: stateDir, provider: "codex" });
  }
  app.refreshSessions = async () => {};
  await app.buildClient();
  const fixtureModule = join(directory, "fixture.ts");
  await writeFile(fixtureModule, `
    import { WorkspaceDiffView } from ${JSON.stringify(join(import.meta.dir, "client/diffs.ts"))};
    import { ChatView } from ${JSON.stringify(join(import.meta.dir, "client/chat.ts"))};
    export async function verify() {
      const NativeObserver = globalThis.IntersectionObserver;
      const observers = [];
      class Observer {
        targets = new Set();
        observes = 0;
        constructor(callback) { this.callback = callback; observers.push(this); }
        observe(target) { this.targets.add(target); this.observes++; }
        unobserve(target) { this.targets.delete(target); }
        disconnect() { this.targets.clear(); }
      }
      globalThis.IntersectionObserver = Observer;
      const preferences = { style: "unified", wrap: true, themeType: "dark" };
      const results = {};
      let view, chat;
      try {
        view = new WorkspaceDiffView(preferences, () => {});
        document.body.append(view.element);
        const observer = observers[0];
        const patch = 'diff --git a/x.ts b/x.ts\\n--- a/x.ts\\n+++ b/x.ts\\n@@ -1 +1 @@\\n-a\\n+b\\n';
        const data = { content: patch, untracked: [], touched: [], cwd: "fixture" };
        view.update(data);
        const section = [...observer.targets][0];
        view.toggleAll();
        observer.callback([{ isIntersecting: true, target: section }]);
        const before = observer.observes;
        view.toggleAll();
        results.lazyReobserved = observer.observes > before;
        observer.callback([{ isIntersecting: true, target: section }]);
        results.lazyMounted = section.querySelector('diffs-container') !== null;
        view.update({ ...data, touched: ["x.ts"] });
        const touchedSection = [...observer.targets][0];
        observer.callback([{ isIntersecting: true, target: touchedSection }]);
        results.touchedUpdated = view.files[0].name === 'x.ts' && view.touched.has('x.ts') && touchedSection !== section;
        view.reset();
        results.observersReleased = observer.targets.size === 0;
        view.update({ content: "", untracked: [], touched: [], error: "first error" });
        view.update({ content: "", untracked: [], touched: [], error: "second error" });
        results.errorUpdated = view.element.textContent.includes("second error");
        chat = new ChatView({ openFileInDiff() {}, preferences: () => preferences, toast() {} });
        document.body.append(chat.element);
        const record = item => JSON.stringify({ method: 'item/completed', params: { threadId: 'root', item } }) + '\\n';
        chat.reset('root', record({ id: 'files', type: 'fileChange', changes: [{ path: 'x.ts', kind: 'update', diff: '@@ -1 +1 @@\\n-a\\n+b\\n' }] }) + record({ id: 'agent', type: 'agentMessage', text: 'done' }), false);
        await new Promise(requestAnimationFrame);
        const fileRow = chat.element.querySelector('[data-entry="files"]');
        fileRow.querySelector('button').click();
        const chatObserver = observers.find(candidate => candidate.targets.size && candidate !== observer);
        const oldHost = [...chatObserver.targets][0];
        chat.reset('other', '', false);
        chatObserver.callback([{ isIntersecting: true, target: oldHost }]);
        results.staleObserverIgnored = oldHost.childElementCount === 0;
        results.chatObserversReleased = chatObserver.targets.size === 0;
        return results;
      } finally {
        view?.reset(); view?.element.remove();
        chat?.reset(undefined, '', false); chat?.element.remove();
        globalThis.IntersectionObserver = NativeObserver;
      }
    }
  `);
  const fixtureBuild = await Bun.build({ entrypoints: [fixtureModule], target: "browser", minify: true });
  expect(fixtureBuild.success).toBe(true);
  const fixtureJS = await fixtureBuild.outputs[0].text();
  const reply = (body: unknown) => Response.json(body);
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, idleTimeout: 0, async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/fixture.js") return new Response(fixtureJS, { headers: { "content-type": "text/javascript" } });
    if (!isAuthorized(request, token)) return app.fetch(request);
    if (request.method !== "GET" && !isSameOriginMutation(request)) return app.fetch(request);
    if (url.pathname === "/api/prompt") {
      prompts++; lastPrompt = await request.json() as Record<string, unknown>;
      await Bun.sleep(350);
      return reply({ status: "fixture prompt accepted" });
    }
    if (url.pathname === "/api/new") {
      launches++;
      await Bun.sleep(350);
      return reply({ status: "fixture launch accepted", stateDir: app.sessions[0].stateDir });
    }
    if (url.pathname === "/api/run/diff") return reply({ content: "", untracked: [], touched: [] });
    if (url.pathname === "/api/run/output") {
      outputReads++;
      const response = await app.fetch(request);
      // Delay only the first read to expose A -> B -> A races.
      if (outputReads === 1) await Bun.sleep(350);
      return response;
    }
    return app.fetch(request);
  }});
  const browser = async (...arguments_: string[]) => {
    const child = Bun.spawn(["agent-browser", "--session", sessionName, ...arguments_], { stdin: "ignore", stdout: "pipe", stderr: "pipe" });
    const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
    if (code !== 0) throw new Error(stderr || stdout);
    return stdout;
  };
  const evaluate = async (source: string) => {
    const child = Bun.spawn(["agent-browser", "--session", sessionName, "eval", "--stdin", "--json"], { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
    child.stdin.write(source); child.stdin.end();
    const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
    if (code !== 0) throw new Error(stderr || stdout);
    const result = JSON.parse(stdout);
    if (!result.success) throw new Error(JSON.stringify(result));
    return result.data.result;
  };
  try {
    console.log("Browser fixture: starting isolated headless session on an ephemeral port");
    await browser("open", `http://127.0.0.1:${server.port}/?token=${token}`);
    await browser("set", "media", "dark", "reduced-motion");
    await browser("wait", "--fn", "document.querySelectorAll('.session').length === 2 && document.querySelector('.chat-list').textContent.includes('alpha message')");
    expect(await evaluate("document.querySelector('.chat-list img') === null")).toBe(true);
    console.log("Browser fixture: draft ownership, prompt intent, and duplicate new-session submit");
    const results = await evaluate(`(async () => {
      const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
      const clickSession = name => [...document.querySelectorAll('.session')].find(el => el.textContent.includes(name)).click();
      const input = document.querySelector('.composer-input');
      input.value = 'same draft'; input.dispatchEvent(new Event('input', { bubbles: true }));
      document.querySelector('.composer').requestSubmit();
      clickSession('beta');
      input.value = 'same draft'; input.dispatchEvent(new Event('input', { bubbles: true }));
      await sleep(450);
      const draftPreserved = input.value === 'same draft';
      document.querySelector('.hdr-btn.accent').click();
      const form = document.querySelector('.new-session');
      form.querySelector('textarea').value = 'fixture only';
      form.requestSubmit(); form.requestSubmit();
      await sleep(450);
      return { draftPreserved };
    })()`);
    expect(results.draftPreserved).toBe(true);
    expect(prompts).toBe(1);
    expect(launches).toBe(1);
    expect(lastPrompt.turnId).toBe("original");
    console.log("Browser fixture: late output responses across A -> B -> A");
    const outputCheck = evaluate(`(async () => {
      const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
      const choose = name => [...document.querySelectorAll('.session')].find(el => el.textContent.includes(name)).click();
      document.querySelector('[data-tab="output"]').click();
      await sleep(60); choose('beta'); choose('alpha');
      await sleep(500);
      return document.querySelector('.output-md').textContent;
    })()`);
    await Bun.sleep(30);
    await writeFile(join(app.sessions[0].stateDir, "output.md"), "alpha newer output");
    expect(await outputCheck).toContain("alpha newer output");
    expect(outputReads).toBe(3);
    console.log("Browser fixture: Pierre lazy mounting, reset cleanup, touched files, and error updates");
    const widgetResults = await evaluate("import('/fixture.js').then(module => module.verify())");
    for (const [name, passed] of Object.entries(widgetResults)) {
      expect(passed, name).toBe(true);
    }
    const outputDirectory = process.env.RUDDR_WEB_SCREENSHOTS;
    if (outputDirectory) {
      await mkdir(outputDirectory, { recursive: true });
      await browser("set", "viewport", "1280", "900");
      await browser("screenshot", join(outputDirectory, "desktop.png"));
      await browser("set", "viewport", "390", "844");
      await evaluate("document.querySelector('.session.selected').click()");
      await browser("screenshot", join(outputDirectory, "phone.png"));
    }
  } finally {
    await browser("close").catch(() => {});
    server.stop(true);
    await rm(directory, { recursive: true, force: true });
  }
}, 60_000);
