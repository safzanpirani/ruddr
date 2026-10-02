// Fetches the platform binary at install time so the first `ruddr` call is
// instant. Failures only warn: the launcher retries on first use.
"use strict";

const { spawnSync } = require("node:child_process");
const { ensureBinary } = require("./npm-binary.cjs");

const log = (message) => process.stderr.write(`${message}\n`);

ensureBinary({ log })
  .then((binary) => {
    log(`ruddr: installed binary at ${binary}`);
    const skill = spawnSync(binary, ["skill", "install"], { stdio: ["ignore", "pipe", "pipe"] });
    if (skill.error || skill.status !== 0)
      log(`ruddr: could not install the ruddr-delegate skill; run \`ruddr skill install\` later${skill.stderr ? `: ${String(skill.stderr).trim()}` : ""}`);
    else log(`ruddr: ${String(skill.stdout).trim().split("\n").join("\nruddr: ")}`);
  })
  .catch((error) => {
    log(error instanceof Error ? error.message : String(error));
    log("ruddr: the binary will be fetched again the first time you run `ruddr`.");
  });
