// Release helper: pins the published package to one tagged version and to
// the exact binaries attached to that GitHub release.
//   node scripts/npm-prepare.cjs <version> <checksums.txt>
"use strict";

const fs = require("node:fs");
const path = require("node:path");

const [version, checksumFile] = process.argv.slice(2);
if (!version || !checksumFile) {
  console.error("usage: npm-prepare.js <version> <checksums.txt>");
  process.exit(2);
}
const root = path.resolve(__dirname, "..");
const workspace = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
const cargoVersion = /\[workspace\.package\][^[]*?\nversion = "([^"]+)"/.exec(workspace)?.[1];
if (cargoVersion !== version) {
  console.error(`Cargo.toml declares version ${cargoVersion}, but the release tag is ${version}`);
  process.exit(1);
}
const checksums = {};
for (const line of fs.readFileSync(checksumFile, "utf8").split("\n")) {
  const match = /^([0-9a-f]{64})\s+\*?(\S+)$/.exec(line.trim());
  if (match) checksums[path.basename(match[2])] = match[1];
}
if (Object.keys(checksums).length === 0) {
  console.error(`no checksums found in ${checksumFile}`);
  process.exit(1);
}
fs.writeFileSync(path.join(root, "checksums.json"), `${JSON.stringify(checksums, null, 2)}\n`);
const manifestPath = path.join(root, "package.json");
const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
manifest.version = version;
fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`prepared ${manifest.name}@${version} with ${Object.keys(checksums).length} pinned binaries`);
