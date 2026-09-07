import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runInNewContext } from "node:vm";

const require = createRequire(import.meta.url);
const source = readFileSync(new URL("./npm-binary.cjs", import.meta.url), "utf8");
const binary = Buffer.from("test binary");
const digest = createHash("sha256").update(binary).digest("hex");

// Run the real installer in an isolated package with fake network and Go.
async function withInstaller(checksum: unknown, run: (fixture: {
  install: () => Promise<string>; downloads: () => number; destination: string;
}) => Promise<void>) {
  const root = mkdtempSync(join(tmpdir(), "ruddr-npm-test-"));
  mkdirSync(join(root, "scripts"));
  writeFileSync(join(root, "package.json"), '{"version":"0.0.0"}');
  writeFileSync(join(root, "checksums.json"), JSON.stringify({ "ruddr-linux-amd64": checksum }));
  let downloads = 0;
  const module = { exports: {} as { ensureBinary: () => Promise<string> } };
  runInNewContext(source, {
    __dirname: join(root, "scripts"), module, Buffer,
    process: { platform: "linux", arch: "x64", pid: process.pid, env: {} },
    require: (name: string) => name === "node:child_process"
      ? { spawnSync: () => ({ status: 1 }) } : require(name),
    fetch: async () => {
      downloads++;
      return { ok: true, arrayBuffer: async () => binary };
    },
  });
  try {
    await run({ install: module.exports.ensureBinary, downloads: () => downloads, destination: join(root, "ruddr") });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("npm installer refuses missing or malformed checksums before downloading", async () => {
  for (const checksum of [undefined, "", "broken", 42]) {
    await withInstaller(checksum, async ({ install, downloads, destination }) => {
      await expect(install()).rejects.toThrow("no prebuilt binary");
      expect(downloads()).toBe(0);
      expect(existsSync(destination)).toBe(false);
    });
  }
});

test("npm installer accepts only a matching pinned binary", async () => {
  await withInstaller(digest.toUpperCase(), async ({ install, downloads, destination }) => {
    expect(await install()).toBe(destination);
    expect(downloads()).toBe(1);
    expect(readFileSync(destination)).toEqual(binary);
  });
  await withInstaller("0".repeat(64), async ({ install, downloads, destination }) => {
    await expect(install()).rejects.toThrow("no prebuilt binary");
    expect(downloads()).toBe(1);
    expect(existsSync(destination)).toBe(false);
    expect(existsSync(`${destination}.${process.pid}.tmp`)).toBe(false);
  });
});
