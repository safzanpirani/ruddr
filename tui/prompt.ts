import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LatestRead } from "./core";
import { runControl } from "./process";

// Failed submissions leave the editor intact. A late success must not close
// an editor that was reopened or changed while the command was in flight.
export class PromptSubmission {
  private readonly submissions = new LatestRead();

  constructor(
    private readonly text: () => string,
    private readonly accepted: () => void,
  ) {}

  invalidate(): void {
    this.submissions.begin();
  }

  async submit<T>(send: () => Promise<T>): Promise<T> {
    const current = this.submissions.begin();
    const text = this.text();
    const result = await send();
    if (current() && this.text() === text) this.accepted();
    return result;
  }
}

export async function sendControlPrompt(
  ruddr: string,
  message: string,
  argumentsForFile: (path: string) => string[],
): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), "ruddr-tui-prompt-"));
  try {
    await chmod(directory, 0o700);
    const file = join(directory, "message.md");
    await writeFile(file, `${message}\n`, { mode: 0o600 });
    return await runControl(ruddr, argumentsForFile(file));
  } finally {
    // Cleanup failure must not turn an accepted prompt into a retryable send.
    await rm(directory, { recursive: true, force: true }).catch(() => {});
  }
}
