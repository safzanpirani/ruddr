#!/usr/bin/env node
// npm launcher: runs the native Ruddr binary that lives beside the TUI and
// adapter sources in this package, so the binary's sibling lookup finds them.
"use strict";

const { spawnSync } = require("node:child_process");
const { ensureBinary } = require("../scripts/npm-binary.cjs");

const log = (message) => process.stderr.write(`${message}\n`);

// Bun and `--ignore-scripts` skip the postinstall hook, so the delegate skill
// would never be installed. Do it here the one time the launcher provisions
// the binary itself. Failures only warn: `ruddr skill install` still works.
function installSkill(binary) {
  if (process.argv[2] === "skill") return;
  const result = spawnSync(binary, ["skill", "install"], { stdio: ["ignore", "pipe", "pipe"] });
  if (result.error || result.status !== 0)
    log(`ruddr: could not install the ruddr-delegate skill; run \`ruddr skill install\` later${result.stderr ? `: ${String(result.stderr).trim()}` : ""}`);
  else log(`ruddr: ${String(result.stdout).trim().split("\n").join("\nruddr: ")}`);
}

ensureBinary({ log, onProvisioned: installSkill })
  .then((binary) => {
    const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
    if (result.error) throw result.error;
    if (result.signal) {
      process.kill(process.pid, result.signal);
      return;
    }
    process.exit(result.status ?? 1);
  })
  .catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(1);
  });
