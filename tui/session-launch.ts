import { chmod, mkdir, mkdtemp, open, readFile } from "node:fs/promises";
import { join } from "node:path";
import { readTail } from "./core";

interface LaunchOptions {
  ruddr: string;
  cwd: string;
  message: string;
  argumentsForFiles: (promptFile: string, stateDirectory: string) => string[];
  onSpawn: (stateDirectory: string) => void;
  startupWindowMs?: number;
}

// Both fresh and continued sessions use this private launch bundle. File-backed
// stderr stays available after the TUI exits and cannot block a long-lived child.
export async function launchSession(options: LaunchOptions): Promise<string> {
  const base = join(options.cwd, ".scratch", "ruddr-tui");
  await mkdir(base, { recursive: true, mode: 0o700 });
  await chmod(base, 0o700);
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const stateDirectory = await mkdtemp(join(base, `${stamp}-`));
  await chmod(stateDirectory, 0o700);
  const promptFile = join(stateDirectory, "prompt.md");
  const prompt = await open(promptFile, "wx", 0o600);
  try {
    await prompt.writeFile(`${options.message}\n`);
  } finally {
    await prompt.close();
  }
  const stderrPath = join(stateDirectory, "launch.stderr.log");
  const stderr = await open(stderrPath, "wx", 0o600);
  let child: ReturnType<typeof Bun.spawn>;
  try {
    child = Bun.spawn([
      options.ruddr,
      ...options.argumentsForFiles(promptFile, stateDirectory),
    ], { cwd: options.cwd, stdin: "ignore", stdout: "ignore", stderr: stderr.fd });
    child.unref();
  } finally {
    // The child owns its duplicate now; parent-handle cleanup cannot make a
    // successful spawn safe to retry.
    await stderr.close().catch(() => {});
  }
  options.onSpawn(stateDirectory);
  const deadline = Date.now() + (options.startupWindowMs ?? 1_500);
  let exitCode: number | undefined;
  void child.exited.then((code) => { exitCode = code; });
  do {
    let status: string | undefined;
    try {
      status = JSON.parse(await readFile(join(stateDirectory, "state.json"), "utf8")).status;
    } catch {
      // The controller creates state after spawn; a missing file is expected.
    }
    if (status === "failed" || status === "interrupted" ||
        (exitCode !== undefined && status !== "completed")) {
      const diagnostic = (await readTail(stderrPath, 4096)).trim();
      throw new Error(diagnostic || `Session exited during startup${exitCode === undefined ? "" : ` (${exitCode})`}; see ${stateDirectory}`);
    }
    if (status === "active" || status === "idle" || status === "completed") break;
    if (Date.now() >= deadline) break;
    await Bun.sleep(25);
  } while (true);
  // A still-starting controller remains registered. Do not report a timeout as
  // a failed launch: retrying it could create a second live session.
  return stateDirectory;
}
