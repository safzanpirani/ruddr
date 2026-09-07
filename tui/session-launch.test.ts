import { afterEach, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { launchSession } from "./session-launch";
import { PromptSubmission, sendControlPrompt } from "./prompt";

const directories: string[] = [];
afterEach(async () => {
  await Promise.all(directories.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

async function fixture(source: string): Promise<{ cwd: string; script: string }> {
  const cwd = await mkdtemp(join(tmpdir(), "ruddr-launch-test-"));
  directories.push(cwd);
  const script = join(cwd, "fake.ts");
  await writeFile(script, source);
  return { cwd, script };
}

test("concurrent launches use distinct private bundles and keep the prompt out of argv", async () => {
  const { cwd, script } = await fixture(`
    const [, , prompt, directory] = Bun.argv;
    const text = await Bun.file(prompt).text();
    if (text !== "private first prompt\\n") process.exit(9);
    await Bun.write(directory + "/state.json", JSON.stringify({status: "completed"}));
  `);
  const registered: string[] = [];
  const results = await Promise.all([1, 2].map(() => launchSession({
    ruddr: process.execPath, cwd, message: "private first prompt",
    argumentsForFiles: (prompt, directory) => [script, prompt, directory],
    onSpawn: (directory) => { registered.push(directory); },
  })));
  expect(new Set(results).size).toBe(2);
  expect(registered.sort()).toEqual(results.sort());
  if (process.platform !== "win32") {
    for (const dir of results) {
      expect((await stat(dir)).mode & 0o777).toBe(0o700);
      expect((await stat(join(dir, "prompt.md"))).mode & 0o777).toBe(0o600);
      expect((await stat(join(dir, "launch.stderr.log"))).mode & 0o777).toBe(0o600);
    }
  }
});

test("early child failure surfaces stderr and retains the draft and diagnostic bundle", async () => {
  const { cwd, script } = await fixture(`
    console.error("configured provider executable was not found");
    process.exit(7);
  `);
  let directory = "";
  let text = "preserve this draft";
  const submission = new PromptSubmission(() => text, () => { text = ""; });
  await expect(submission.submit(() => launchSession({
    ruddr: process.execPath, cwd, message: text,
    argumentsForFiles: () => [script],
    onSpawn: (dir) => { directory = dir; },
  }))).rejects.toThrow("configured provider executable was not found");
  expect(text).toBe("preserve this draft");
  expect(await readFile(join(directory, "prompt.md"), "utf8")).toBe(`${text}\n`);
  expect(await readFile(join(directory, "launch.stderr.log"), "utf8"))
    .toContain("configured provider executable was not found");
});

test("a live controller with slow startup is registered without encouraging a duplicate retry", async () => {
  const { cwd, script } = await fixture(`
    const directory = Bun.argv[2];
    await Bun.write(directory + "/state.json", JSON.stringify({status: "starting"}));
    await Bun.sleep(150);
    await Bun.write(directory + "/state.json", JSON.stringify({status: "completed"}));
  `);
  let registered = "";
  const directory = await launchSession({
    ruddr: process.execPath, cwd, message: "start",
    argumentsForFiles: (_, dir) => [script, dir],
    onSpawn: (dir) => { registered = dir; }, startupWindowMs: 25,
  });
  expect(directory).toBe(registered);
  // Wait for the finite fake child before deleting its fixture.
  const deadline = Date.now() + 2000;
  while (Date.now() < deadline) {
    try {
      if (JSON.parse(await readFile(join(directory, "state.json"), "utf8")).status === "completed") return;
    } catch {}
    await Bun.sleep(25);
  }
  throw new Error("fake controller did not finish");
});

test("control prompt uses a private file, propagates rejection, and removes the temporary file", async () => {
  const { script } = await fixture(`
    const file = Bun.argv[2];
    const {statSync} = await import("node:fs");
    if (process.platform !== "win32" && (statSync(file).mode & 0o777) !== 0o600) process.exit(8);
    if (await Bun.file(file).text() !== "private steer\\n") process.exit(9);
    console.error("turn changed");
    process.exit(1);
  `);
  let file = "";
  await expect(sendControlPrompt(process.execPath, "private steer", (path) => {
    file = path;
    return [script, path];
  })).rejects.toThrow("turn changed");
  expect(await Bun.file(file).exists()).toBe(false);
});
