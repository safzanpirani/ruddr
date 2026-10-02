// Locates or fetches the Ruddr binary for this platform. Shared by the npm
// launcher shim and the postinstall hook. Plain CommonJS so it runs under
// Node 18+ and Bun without a build step. The binary is the whole of Ruddr:
// the CLI, controller, provider adapters, TUI, and web dashboard.
"use strict";

const { createHash } = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const packageRoot = path.resolve(__dirname, "..");
const manifest = require(path.join(packageRoot, "package.json"));
const repository = "safzanpirani/ruddr";

function platformTarget() {
  const goos = { darwin: "darwin", linux: "linux", win32: "windows" }[process.platform];
  const goarch = { x64: "amd64", arm64: "arm64" }[process.arch];
  if (!goos || !goarch) return undefined;
  return { goos, goarch, extension: goos === "windows" ? ".exe" : "" };
}

function assetName(target) {
  return `ruddr-${target.goos}-${target.goarch}${target.extension}`;
}

function binaryPath() {
  if (process.env.RUDDR_BINARY) return process.env.RUDDR_BINARY;
  const target = platformTarget();
  return path.join(packageRoot, `ruddr${target ? target.extension : ""}`);
}

function readChecksums() {
  try {
    return JSON.parse(fs.readFileSync(path.join(packageRoot, "checksums.json"), "utf8"));
  } catch {
    return {};
  }
}

function sha256(file) {
  return createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

async function download(url, destination, log) {
  log(`ruddr: downloading ${url}`);
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) throw new Error(`download failed: ${response.status} ${response.statusText}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  fs.writeFileSync(destination, bytes, { mode: 0o755 });
}

/**
 * Returns the path of a usable binary, fetching or building one if needed.
 * Calls `options.onProvisioned(path)` when this call created the binary, so
 * callers can run first-install steps. Throws with actionable text when
 * neither a download nor a local build is possible.
 */
async function ensureBinary(options = {}) {
  const log = options.log || (() => undefined);
  const destination = binaryPath();
  if (fs.existsSync(destination)) return destination;
  if (process.env.RUDDR_BINARY)
    throw new Error(`RUDDR_BINARY points at ${destination}, which does not exist`);

  const target = platformTarget();
  const temporary = `${destination}.${process.pid}.tmp`;
  const checksums = readChecksums();
  const finish = () => {
    fs.chmodSync(temporary, 0o755);
    fs.renameSync(temporary, destination);
    if (options.onProvisioned) options.onProvisioned(destination);
    return destination;
  };

  if (target && process.env.RUDDR_SKIP_DOWNLOAD !== "1") {
    const asset = assetName(target);
    const expected = checksums[asset];
    const url = `https://github.com/${repository}/releases/download/v${manifest.version}/${asset}`;
    try {
      if (typeof expected !== "string" || !/^[0-9a-f]{64}$/i.test(expected))
        throw new Error(`no valid pinned checksum for ${asset}`);
      await download(url, temporary, log);
      const actual = sha256(temporary);
      if (actual !== expected.toLowerCase())
        throw new Error(`checksum mismatch for ${asset}: expected ${expected}, got ${actual}`);
      return finish();
    } catch (error) {
      try {
        fs.rmSync(temporary, { force: true });
      } catch {
        // Nothing to clean up.
      }
      log(`ruddr: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  throw new Error(
    [
      `ruddr: no prebuilt binary is available for ${os.platform()}/${os.arch()} at version ${manifest.version}.`,
      "Build one with `cargo build --release -p ruddr-cli` from https://github.com/safzanpirani/ruddr",
      "and set RUDDR_BINARY to it.",
    ].join("\n"),
  );
}

module.exports = { assetName, binaryPath, ensureBinary, platformTarget, packageRoot };
