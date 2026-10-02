import { describe, expect, test } from "bun:test";
import { displayCommand, editsFromPatchText, editStats, fileEditsFromItem, Transcript, unifiedPatchForEdit } from "./transcript";

const line = (value: unknown) => `${JSON.stringify(value)}\n`;

describe("Transcript", () => {
  test("streams agent deltas and lets the completed item win", () => {
    const transcript = new Transcript("t1");
    transcript.applyText(line({ method: "item/agentMessage/delta", params: { threadId: "t1", itemId: "a1", delta: "Hel" } }));
    transcript.applyText(line({ method: "item/agentMessage/delta", params: { threadId: "t1", itemId: "a1", delta: "lo" } }));
    expect(transcript.entries).toMatchObject([{ kind: "agent", text: "Hello", streaming: true }]);
    transcript.applyText(line({ method: "item/completed", params: { threadId: "t1", item: { id: "a1", type: "agentMessage", text: "Hello there" } } }));
    expect(transcript.entries).toMatchObject([{ kind: "agent", text: "Hello there", streaming: false }]);
  });

  test("keeps one bubble when the provider echoes a Ruddr prompt", () => {
    const transcript = new Transcript();
    transcript.applyText(
      line({ method: "item/completed", params: { threadId: "t1", item: { id: "p1", type: "userMessage", origin: "ruddr", text: "fix it" } } }) +
        line({ method: "item/completed", params: { threadId: "t1", item: { id: "u2", type: "userMessage", content: [{ type: "text", text: "fix it" }] } } }),
    );
    expect(transcript.entries.filter((entry) => entry.kind === "user")).toHaveLength(1);
  });

  test("drops rejected prompts", () => {
    const transcript = new Transcript();
    transcript.applyText(
      line({ method: "item/completed", params: { threadId: "t1", item: { id: "p1", type: "userMessage", origin: "ruddr", text: "steer" } } }) +
        line({ method: "ruddr/prompt/rejected", params: { promptId: "p1" } }),
    );
    expect(transcript.entries).toHaveLength(0);
  });

  test("streams command output and records the exit code", () => {
    const transcript = new Transcript("t1");
    transcript.applyText(
      line({ method: "item/started", params: { threadId: "t1", item: { id: "c1", type: "commandExecution", command: "/bin/zsh -lc 'ls -la'" } } }) +
        line({ method: "item/commandExecution/outputDelta", params: { threadId: "t1", itemId: "c1", delta: "a\n" } }),
    );
    expect(transcript.entries[0]).toMatchObject({ kind: "command", command: "ls -la", status: "running", output: "a\n" });
    transcript.applyText(line({ method: "item/completed", params: { threadId: "t1", item: { id: "c1", type: "commandExecution", status: "completed", exitCode: 2, aggregatedOutput: "a\nb\n" } } }));
    expect(transcript.entries[0]).toMatchObject({ status: "failed", exitCode: 2, output: "a\nb\n" });
  });

  test("ignores sub-agent threads but finishes their activity row", () => {
    const transcript = new Transcript();
    transcript.applyText(
      line({ method: "item/completed", params: { threadId: "root", item: { id: "p", type: "userMessage", origin: "ruddr", text: "go" } } }) +
        line({ method: "item/started", params: { threadId: "root", item: { id: "s1", type: "subAgentActivity", agentPath: "reviewer", agentThreadId: "child" } } }) +
        line({ method: "item/completed", params: { threadId: "child", item: { id: "x", type: "agentMessage", text: "child says" } } }) +
        line({ method: "turn/completed", params: { threadId: "child", turn: { status: "completed", durationMs: 1200 } } }),
    );
    expect(transcript.entries.map((entry) => entry.kind)).toEqual(["user", "agentRun"]);
    expect(transcript.entries[1]).toMatchObject({ status: "completed", output: "child says", durationMs: 1200 });
  });

  test("a finished turn settles running rows and adds a divider", () => {
    const transcript = new Transcript("t1");
    transcript.applyText(
      line({ method: "item/started", params: { threadId: "t1", item: { id: "c1", type: "commandExecution", command: "make" } } }) +
        line({ method: "turn/completed", params: { threadId: "t1", turn: { id: "turn1", status: "interrupted", durationMs: 5000 } } }),
    );
    expect(transcript.entries).toMatchObject([{ kind: "command", status: "stopped" }, { kind: "turn", status: "interrupted", durationMs: 5000 }]);
  });

  test("a clean turn end still settles tools as completed", () => {
    const transcript = new Transcript("t1");
    transcript.applyText(
      line({ method: "item/started", params: { threadId: "t1", item: { id: "c1", type: "commandExecution", command: "make" } } }) +
        line({ method: "turn/completed", params: { threadId: "t1", turn: { id: "turn1", status: "completed" } } }),
    );
    expect(transcript.entries[0]).toMatchObject({ status: "completed" });
  });

  test("tracks context usage", () => {
    const transcript = new Transcript();
    transcript.applyText(line({ method: "thread/tokenUsage/updated", params: { tokenUsage: { total: { totalTokens: 900 }, last: { totalTokens: 400 }, modelContextWindow: 1000 } } }));
    expect(transcript.usage).toEqual({ totalTokens: 900, contextTokens: 400, contextWindow: 1000 });
  });

  test("skips a partial first line from a bounded tail", () => {
    const transcript = new Transcript();
    transcript.applyText(`ms":1}\n${line({ method: "item/completed", params: { item: { id: "a", type: "agentMessage", text: "ok" } } })}`);
    expect(transcript.entries).toHaveLength(1);
  });

  test("reports only changed entries as dirty", () => {
    const transcript = new Transcript();
    transcript.applyText(line({ method: "item/started", params: { item: { id: "a", type: "agentMessage", text: "one" } } }));
    expect(transcript.takeDirty()).toMatchObject({ structure: true });
    transcript.applyText(line({ method: "item/agentMessage/delta", params: { itemId: "a", delta: "!" } }));
    const dirty = transcript.takeDirty();
    expect([...dirty.ids]).toEqual(["a"]);
    expect(dirty.structure).toBe(false);
  });
});

describe("file edits", () => {
  test("normalizes Codex changes", () => {
    const edits = fileEditsFromItem({ changes: [{ path: "/r/a.ts", kind: { type: "update", move_path: "/r/b.ts" }, diff: "@@ -1 +1 @@\n-a\n+b\n" }] });
    expect(edits).toEqual([{ path: "/r/a.ts", kind: "update", movePath: "/r/b.ts", diff: "@@ -1 +1 @@\n-a\n+b\n" }]);
    expect(unifiedPatchForEdit(edits[0])).toBe("--- a//r/a.ts\n+++ b//r/b.ts\n@@ -1 +1 @@\n-a\n+b\n");
    expect(editStats(edits[0])).toEqual({ additions: 1, deletions: 1 });
  });

  test("reads Claude Edit, Write, and MultiEdit inputs", () => {
    expect(fileEditsFromItem({ input: { file_path: "x.ts", old_string: "a", new_string: "b\nc" } })).toEqual([{ path: "x.ts", kind: "update", oldText: "a", newText: "b\nc" }]);
    expect(fileEditsFromItem({ input: { file_path: "y.ts", content: "new" } })).toEqual([{ path: "y.ts", kind: "add", oldText: "", newText: "new" }]);
    expect(fileEditsFromItem({ input: { file_path: "z.ts", edits: [{ old_string: "1", new_string: "2" }] } })).toHaveLength(1);
    expect(fileEditsFromItem({ input: { filePath: "o.ts", oldString: "p", newString: "q" } })).toEqual([{ path: "o.ts", kind: "update", oldText: "p", newText: "q" }]);
  });

  test("splits apply_patch envelopes", () => {
    const edits = editsFromPatchText("*** Begin Patch\n*** Add File: n.txt\n+hi\n*** Update File: u.txt\n@@\n-a\n+b\n*** End Patch");
    expect(edits).toEqual([
      { path: "n.txt", kind: "add", diff: "hi" },
      { path: "u.txt", kind: "update", diff: "@@ -1 +1 @@\n-a\n+b" },
    ]);
  });

  test("unwraps shell commands", () => {
    expect(displayCommand("/bin/zsh -lc 'echo '\\''hi'\\'''")).toBe("echo 'hi'");
    expect(displayCommand("bash -lc \"ls \\\"x\\\"\"")).toBe('ls "x"');
    expect(displayCommand("git status")).toBe("git status");
  });
});


describe("transcript regressions", () => {
  test("filters child usage and Ruddr prompts before applying them", () => {
    const transcript = new Transcript("root");
    transcript.applyText(
      line({ method: "thread/tokenUsage/updated", params: { threadId: "child", tokenUsage: { last: { totalTokens: 999 } } } }) +
      line({ method: "item/completed", params: { threadId: "child", item: { id: "child-prompt", type: "userMessage", origin: "ruddr", text: "child only" } } }) +
      line({ method: "item/completed", params: { threadId: "child", item: { id: "child-agent", type: "agentMessage", text: "child answer" } } }),
    );
    expect(transcript.usage).toEqual({});
    expect(transcript.entries).toHaveLength(0);
  });

  test("preserves streamed text on completion without text and ignores late starts and deltas", () => {
    const transcript = new Transcript("root");
    transcript.applyText(
      line({ method: "item/agentMessage/delta", params: { threadId: "root", itemId: "a", delta: "hello" } }) +
      line({ method: "item/completed", params: { threadId: "root", item: { id: "a", type: "agentMessage" } } }) +
      line({ method: "item/started", params: { threadId: "root", item: { id: "a", type: "agentMessage", text: "old" } } }) +
      line({ method: "item/agentMessage/delta", params: { threadId: "root", itemId: "a", delta: " duplicated" } }),
    );
    expect(transcript.entries).toMatchObject([{ kind: "agent", text: "hello", streaming: false }]);
  });

  test("keeps repeated Ruddr prompts and consumes provider echoes across intervening items", () => {
    const transcript = new Transcript("root");
    for (const id of ["p1", "p2"]) {
      transcript.applyText(line({ method: "item/completed", params: { threadId: "root", item: { id, type: "userMessage", origin: "ruddr", text: "same" } } }));
    }
    transcript.applyText(line({ method: "item/started", params: { threadId: "root", item: { id: "tool", type: "toolCall", toolName: "read" } } }));
    transcript.applyText(line({ method: "item/completed", params: { threadId: "root", item: { id: "echo", type: "userMessage", text: "same" } } }));
    expect(transcript.entries.filter((entry) => entry.kind === "user")).toHaveLength(2);
  });

  test("renders Pi/OpenCode edit toolCalls and their tool outputs", () => {
    const transcript = new Transcript("root");
    transcript.applyText(
      line({ method: "item/completed", params: { threadId: "root", item: { id: "edit", type: "toolCall", toolName: "edit", input: { path: "x.ts", oldText: "a", newText: "b" } } } }) +
      line({ method: "item/completed", params: { threadId: "root", item: { id: "read", type: "toolCall", toolName: "read", input: { path: "x.ts" }, output: "file contents" } } }),
    );
    expect(transcript.entries).toMatchObject([
      { kind: "files", edits: [{ path: "x.ts", oldText: "a", newText: "b" }] },
      { kind: "tool", name: "read", output: "file contents" },
    ]);
  });
});
