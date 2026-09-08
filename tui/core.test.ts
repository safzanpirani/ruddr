import {
  INITIAL_HIGHLIGHT_STATE,
  filetypeForPath,
  filetypeForFence,
  highlightCode,
  highlightLines,
  highlightLine,
  parseInline,
  parseMarkdown
} from "./syntax";
import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  artifactAllowsTextSelection,
  AsyncTaskGate,
  LatestRead,
  attachToolDetails,
  discoverSessions,
  diffTreeWidthForPointer,
  emptyPromptHint,
  compactSessionDetails,
  continuationRunArguments,
  contextualHelp,
  dashboardNavigation,
  filterSessions,
  initialViewState,
  idlePromptControlArguments,
  latestAgentUpdate,
  parseTraceActivities,
  parseToolEventDetails,
  readTail,
  reduceView,
  sessionDescription,
  sessionDetails,
  statusGlyph,
  steerControlArguments,
  visibleArtifactTail,
  visibleSessions,
  formatTokenUsage,
  gitDiffTree,
  gitDiffFileStats,
  gitDiffGutterWidth,
  gitDiffSummary,
  helpSegments,
  blendHex,
  parseGitDiffHunkHeader,
  spinnerFrame,
  statusGlyphForKind,
  statusTimeoutMs,
  visibleGitDiffLineIndices,
  clampScrollOffset,
  contextMeter,
  contextUsageFromEvents,
  DEFAULT_MOBILE_WIDTH_THRESHOLD,
  layoutForWidth,
  deleteSessionArtifacts,
  sessionIsDeletable,
  diffTreeWidthForRatio,
  filterPaletteCommands,
  nextDiffPollDelay,
  renderMeter,
  typewriterReveal,
  promptModeForSession,
  promptTargetForSession,
  resolvePromptTarget,
  modelPickerOptions,
  parseModelCatalog,
  newSessionRunArguments,
  nextGitDiffBoundary,
  parseDejaHits,
  parseChatTranscript,
  parseArguments,
  parseGitDiff,
  sessionsPanelTitle,
  FALLBACK_MODELS,
  type Session,
} from "./core";

function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function session(overrides: Partial<Session> = {}): Session {
  return {
    version: 1,
    pid: 123,
    status: "active",
    stateDir: "/tmp/run-a",
    stateFile: "/tmp/run-a/state.json",
    threadId: "thread-1234567890",
    turnId: "turn-1234567890",
    model: "gpt-test",
    startedAt: "2026-08-25T00:00:00Z",
    updatedAt: "2026-08-25T00:01:00Z",
    ...overrides,
  };
}

describe("view reducer", () => {
  test("preserves a selected session across refreshes", () => {
    const selected = { ...initialViewState, selectedStateDir: "/tmp/run-b" };
    const next = reduceView(selected, {
      type: "sessions",
      sessions: [session(), session({ stateDir: "/tmp/run-b" })],
    });
    expect(next.selectedStateDir).toBe("/tmp/run-b");
  });

  test("falls back to the first session when selection disappears", () => {
    const selected = { ...initialViewState, selectedStateDir: "/tmp/missing" };
    const next = reduceView(selected, {
      type: "sessions",
      sessions: [session()],
    });
    expect(next.selectedStateDir).toBe("/tmp/run-a");
  });

  test("tracks artifact, focus, steering, and guarded interrupt state", () => {
    let state = reduceView(initialViewState, { type: "toggle-artifact" });
    state = reduceView(state, { type: "toggle-focus" });
    state = reduceView(state, { type: "open-steer" });
    state = reduceView(state, { type: "arm-interrupt", now: 100 });
    expect(state).toMatchObject({
      artifact: "trace",
      focus: "steer",
      interruptArmedUntil: 2100,
    });
  });
});

describe("TUI layout helpers", () => {
  test("enables beta layout from either the flag or environment", () => {
    expect(parseArguments(["--ruddr", "/tmp/ruddr"], {}).beta).toBe(false);
    expect(
      parseArguments(["--ruddr", "/tmp/ruddr", "--beta"], {}).beta,
    ).toBe(true);
    expect(
      parseArguments(["--ruddr", "/tmp/ruddr"], {
        RUDDR_TUI_BETA: "1",
      }).beta,
    ).toBe(true);
  });

  test("routes Tab and Escape according to the selected layout", () => {
    expect(dashboardNavigation("beta", "artifact", "tab")).toBe(
      "show-sessions",
    );
    expect(dashboardNavigation("classic", "artifact", "tab")).toBe(
      "focus-sessions",
    );
    expect(dashboardNavigation("classic", "sessions", "tab")).toBe(
      "focus-artifact",
    );
    expect(dashboardNavigation("classic", "sessions", "escape")).toBe(
      "focus-artifact",
    );
  });

  test("describes persistent and overlay session navigation accurately", () => {
    expect(
      sessionsPanelTitle({
        layout: "classic",
        liveCount: 2,
        recentCount: 5,
      }),
    ).toBe(" sessions · 2 live · 5 recent ");
    expect(
      sessionsPanelTitle({
        layout: "beta",
        liveCount: 2,
        recentCount: 5,
      }),
    ).toContain("Enter open · Esc close");
    expect(
      contextualHelp({
        layout: "classic",
        focus: "sessions",
        hasQuery: false,
      }),
    ).toContain("Tab chat");
    expect(
      contextualHelp({
        layout: "beta",
        focus: "artifact",
        hasQuery: false,
        dejaAvailable: true,
      }),
    ).toContain("m model · f find");
    expect(
      contextualHelp({
        layout: "beta",
        focus: "artifact",
        hasQuery: false,
        compact: true,
      }),
    ).toContain("o tabs");
    expect(emptyPromptHint("classic")).toBe("n new session · Tab focus");
  });
});

describe("async task gate", () => {
  test("waits for an active refresh and rejects new work before teardown", async () => {
    const gate = new AsyncTaskGate();
    const active = deferred();
    let secondTaskRan = false;
    const firstRun = gate.run(() => active.promise);

    let stopped = false;
    const stopping = gate.stop().then(() => {
      stopped = true;
    });
    await Promise.resolve();
    expect(stopped).toBe(false);

    await gate.run(async () => {
      secondTaskRan = true;
    });
    expect(secondTaskRan).toBe(false);

    active.resolve();
    await Promise.all([firstRun, stopping]);
    expect(stopped).toBe(true);
  });
});

describe("session discovery", () => {
  test("recurses, skips invalid state, marks dead runs stale, and sorts active first", async () => {
    const root = await mkdtemp(join(tmpdir(), "ruddr-tui-discovery-"));
    const activeDir = join(root, "nested", "active");
    const staleDir = join(root, "stale");
    await mkdir(activeDir, { recursive: true });
    await mkdir(staleDir, { recursive: true });
    await writeFile(
      join(activeDir, "state.json"),
      JSON.stringify(session({ stateDir: activeDir, pid: 1 })),
    );
    await writeFile(
      join(staleDir, "state.json"),
      JSON.stringify(
        session({ stateDir: staleDir, pid: 2, status: "starting" }),
      ),
    );
    await writeFile(join(root, "state.json"), "not json");

    const sessions = await discoverSessions({
      roots: [root],
      stateDirs: [],
      registryDirs: [],
      processAlive: (pid) => pid === 1,
    });
    expect(sessions.map(({ status }) => status)).toEqual(["active", "stale"]);
    expect(sessions.every(({ provider }) => provider === "codex")).toBe(true);
    expect(sessions[1]?.error).toContain("not running");
  });

  test("discovers runs referenced by the global registry", async () => {
    const root = await mkdtemp(join(tmpdir(), "ruddr-tui-registry-"));
    const registryDir = join(root, "registry");
    const stateDir = join(root, "outside-project", "run");
    await mkdir(registryDir, { recursive: true });
    await mkdir(stateDir, { recursive: true });
    await writeFile(
      join(stateDir, "state.json"),
      JSON.stringify(session({ stateDir, pid: 1 })),
    );
    await writeFile(join(registryDir, "active.run"), `${stateDir}\n`);

    const sessions = await discoverSessions({
      roots: [],
      stateDirs: [],
      registryDirs: [registryDir],
      processAlive: () => true,
    });
    expect(sessions.map(({ stateDir: discovered }) => discovered)).toEqual([
      stateDir,
    ]);
  });
});

describe("artifact and display helpers", () => {
  test("reserves native mouse text selection for the output pane", () => {
    expect(artifactAllowsTextSelection("trace")).toBe(false);
    expect(artifactAllowsTextSelection("output")).toBe(true);
    expect(artifactAllowsTextSelection("diff")).toBe(true);
  });

  test("cycles through the live diff artifact", () => {
    let state = initialViewState;
    for (const artifact of ["trace", "output", "diff", "chat"] as const) {
      state = reduceView(state, { type: "toggle-artifact" });
      expect(state.artifact).toBe(artifact);
    }
  });

  test("classifies unified Git diff lines for terminal styling", () => {
    expect(
      parseGitDiff(
        [
          "diff --git a/a.ts b/a.ts",
          "index 123..456 100644",
          "--- a/a.ts",
          "+++ b/a.ts",
          "@@ -1 +1 @@",
          "-const oldValue = 1;",
          "+const newValue = 2;",
          " unchanged",
        ].join("\n"),
      ).map(({ kind }) => kind),
    ).toEqual([
      "file",
      "metadata",
      "metadata",
      "metadata",
      "hunk",
      "deletion",
      "addition",
      "context",
    ]);
  });

  test("builds a hierarchical changed-file tree with line counts", () => {
    const tree = gitDiffTree(
      parseGitDiff(
        [
          "diff --git a/src/api.ts b/src/api.ts",
          "@@ -1 +1,2 @@",
          "-old",
          "+new",
          "+next",
          "diff --git a/src/ui/view.ts b/src/ui/view.ts",
          "@@ -1 +1 @@",
          "-before",
          "+after",
          "diff --git a/README.md b/README.md",
          "+docs",
        ].join("\n"),
      ),
    );
    expect(tree).toEqual([
      { label: "▾ 󰉋 src", rowIndex: 0, kind: "directory", path: "src", expanded: true },
      { label: "    󰈔 api.ts  M  +2 −1", rowIndex: 0, kind: "file", path: "src/api.ts", status: "M" },
      { label: "  ▾ 󰉋 ui", rowIndex: 5, kind: "directory", path: "src/ui", expanded: true },
      { label: "      󰈔 view.ts  M  +1 −1", rowIndex: 5, kind: "file", path: "src/ui/view.ts", status: "M" },
      { label: "  󰈔 README.md  M  +1 −0", rowIndex: 9, kind: "file", path: "README.md", status: "M" },
    ]);
    expect(gitDiffTree(parseGitDiff("diff --git a/src/a.ts b/src/a.ts\n+a"), new Set(["src"]))).toEqual([
      { label: "▸ 󰉋 src", rowIndex: 0, kind: "directory", path: "src", expanded: false },
    ]);
    expect(
      gitDiffTree(
        parseGitDiff(
          "diff --git a/new.ts b/new.ts\nnew file mode 100644\n+new\n" +
            "diff --git a/old.ts b/old.ts\ndeleted file mode 100644\n-old",
        ),
      ).filter((entry) => entry.kind === "file").map((entry) => entry.status),
    ).toEqual(["A", "D"]);
  });

  test("wraps diff navigation across hunks and files", () => {
    const lines = parseGitDiff(
      "diff --git a/a.ts b/a.ts\n@@ -1 +1 @@\n-old\n+new\n" +
        "@@ -5 +5 @@\n-old\n+new\n" +
        "diff --git a/b.ts b/b.ts\n@@ -1 +1 @@",
    );
    expect(nextGitDiffBoundary(lines, -1, "hunk", 1)).toBe(1);
    expect(nextGitDiffBoundary(lines, 1, "hunk", 1)).toBe(4);
    expect(nextGitDiffBoundary(lines, 8, "hunk", 1)).toBe(1);
    expect(nextGitDiffBoundary(lines, 4, "file", -1)).toBe(0);
    expect(nextGitDiffBoundary(lines, 0, "file", -1)).toBe(7);
  });

  test("numbers diff lines from hunk headers and tags them with their file", () => {
    const lines = parseGitDiff(
      [
        "diff --git a/src/a.ts b/src/a.ts",
        "index 1..2 100644",
        "--- a/src/a.ts",
        "+++ b/src/a.ts",
        "@@ -10,3 +12,4 @@ function main() {",
        " keep",
        "-old",
        "+new",
        "+next",
        " tail",
        "diff --git a/b.ts b/b.ts",
        "@@ -1 +1 @@",
        "-x",
        "+y",
      ].join("\n"),
    );
    expect(lines.map((line) => [line.path, line.oldLine, line.newLine])).toEqual([
      ["src/a.ts", undefined, undefined],
      ["src/a.ts", undefined, undefined],
      ["src/a.ts", undefined, undefined],
      ["src/a.ts", undefined, undefined],
      ["src/a.ts", undefined, undefined],
      ["src/a.ts", 10, 12],
      ["src/a.ts", 11, undefined],
      ["src/a.ts", undefined, 13],
      ["src/a.ts", undefined, 14],
      ["src/a.ts", 12, 15],
      ["b.ts", undefined, undefined],
      ["b.ts", undefined, undefined],
      ["b.ts", 1, undefined],
      ["b.ts", undefined, 1],
    ]);
    expect(parseGitDiffHunkHeader("@@ -10,3 +12,4 @@ function main() {")).toEqual({
      oldStart: 10,
      oldCount: 3,
      newStart: 12,
      newCount: 4,
      context: "function main() {",
    });
    expect(parseGitDiffHunkHeader("@@ -1 +1 @@")?.newCount).toBe(1);
    expect(parseGitDiffHunkHeader("not a hunk")).toBeUndefined();
    expect(gitDiffSummary(lines)).toEqual({ files: 2, additions: 3, deletions: 2 });
    expect(gitDiffGutterWidth(lines)).toBe(2);
    expect(gitDiffGutterWidth([])).toBe(2);
    expect(gitDiffGutterWidth(parseGitDiff("diff --git a/x b/x\n@@ -1200 +1 @@\n-x"))).toBe(4);
    expect([...gitDiffFileStats(lines).entries()]).toEqual([
      ["src/a.ts", { additions: 2, deletions: 1, status: "M" }],
      ["b.ts", { additions: 1, deletions: 1, status: "M" }],
    ]);
  });

  test("keeps file headers visible while folded file bodies are hidden", () => {
    const lines = parseGitDiff(
      "diff --git a/a.ts b/a.ts\n@@ -1 +1 @@\n-old\n+new\n" +
        "diff --git a/b.ts b/b.ts\n@@ -1 +1 @@\n-x\n+y",
    );
    expect(visibleGitDiffLineIndices(lines, new Set())).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);
    expect(visibleGitDiffLineIndices(lines, new Set(["a.ts"]))).toEqual([0, 4, 5, 6, 7]);
    expect(visibleGitDiffLineIndices(lines, new Set(["a.ts", "b.ts"]))).toEqual([0, 4]);
    const tree = gitDiffTree(lines, new Set(), new Set(["b.ts"]));
    expect(tree.map((entry) => [entry.path, entry.collapsed])).toEqual([
      ["a.ts", undefined],
      ["b.ts", true],
    ]);
  });

  test("splits footer help into key and label segments", () => {
    expect(helpSegments("s prompt · x x stop · Tab focus · q quit")).toEqual([
      { key: "s", label: "prompt" },
      { key: "x x", label: "stop" },
      { key: "Tab", label: "focus" },
      { key: "q", label: "quit" },
    ]);
    expect(helpSegments("]c ]f next hunk/file · [c [f previous · Enter fold file")).toEqual([
      { key: "]c ]f", label: "next hunk/file" },
      { key: "[c [f", label: "previous" },
      { key: "Enter", label: "fold file" },
    ]);
    expect(helpSegments("j/k select")).toEqual([{ key: "j/k", label: "select" }]);
    expect(helpSegments("")).toEqual([]);
  });

  test("offers diff navigation help only while the diff pane is focused", () => {
    const base = { layout: "classic" as const, focus: "artifact" as const, hasQuery: false };
    expect(contextualHelp({ ...base, artifact: "diff" })).toContain("]c ]f next hunk/file");
    expect(contextualHelp({ ...base, artifact: "diff", compact: true })).toBe(
      "]c hunk · ]f file · Enter fold · s prompt · Tab focus · q quit",
    );
    expect(contextualHelp({ ...base, artifact: "chat" })).not.toContain("fold");
    expect(
      contextualHelp({ ...base, artifact: "diff", session: { status: "active" } }),
    ).toContain("x x stop");
  });

  test("animates and colors status chrome deterministically", () => {
    expect(spinnerFrame(0)).toBe("⠋");
    expect(spinnerFrame(10)).toBe("⠋");
    expect(spinnerFrame(3)).toBe(spinnerFrame(13));
    expect(spinnerFrame(-1)).toBe("⠏");
    expect(blendHex("#000000", "#ffffff", 0.5)).toBe("#808080");
    expect(blendHex("#101318", "#7bd88f", 0)).toBe("#101318");
    expect(blendHex("#101318", "#7bd88f", 1)).toBe("#7bd88f");
    expect(blendHex("#fff", "#000", 2)).toBe("#000000");
    expect(statusGlyphForKind("success")).toBe("✓");
    expect(statusGlyphForKind("error")).toBe("×");
    expect(statusGlyphForKind("warning")).toBe("!");
    expect(statusGlyphForKind("info")).toBe("›");
    expect(statusTimeoutMs("error")).toBeGreaterThan(statusTimeoutMs("success"));
  });

  test("tokenizes code lines by language", () => {
    expect(filetypeForPath("src/app.tsx")).toBe("typescript");
    expect(filetypeForPath("main.go")).toBe("go");
    expect(filetypeForPath("Dockerfile")).toBe("shell");
    expect(filetypeForPath("notes.txt")).toBe("plain");
    expect(filetypeForPath(undefined)).toBe("plain");
    expect(filetypeForFence("tsx")).toBe("typescript");
    expect(filetypeForFence("bash title=x")).toBe("shell");
    expect(filetypeForFence("")).toBe("plain");
    expect(highlightCode('const x = "hi"; // note', "typescript")).toEqual([
      { text: "const", token: "keyword" },
      { text: " x ", token: "plain" },
      { text: "=", token: "operator" },
      { text: " ", token: "plain" },
      { text: '"hi"', token: "string" },
      { text: ";", token: "punctuation" },
      { text: " ", token: "plain" },
      { text: "// note", token: "comment" },
    ]);
    expect(highlightCode("return foo(42)", "go").map((span) => span.token)).toEqual([
      "keyword",
      "plain",
      "function",
      "punctuation",
      "number",
      "punctuation",
    ]);
    expect(highlightCode("func Run(ctx context.Context) error {", "go").map((span) => [span.text, span.token])).toEqual([
      ["func", "keyword"],
      [" ", "plain"],
      ["Run", "function"],
      ["(", "punctuation"],
      ["ctx context", "plain"],
      [".", "punctuation"],
      ["Context", "type"],
      [")", "punctuation"],
      [" ", "plain"],
      ["error", "type"],
      [" ", "plain"],
      ["{", "punctuation"],
    ]);
    expect(highlightCode("def run(self): # go", "python").map((span) => span.token)).toContain("comment");
    expect(highlightCode("echo $HOME # c", "shell").map((span) => [span.text, span.token])).toEqual([
      ["echo ", "plain"],
      ["$HOME", "property"],
      [" ", "plain"],
      ["# c", "comment"],
    ]);
    expect(highlightCode('{"key": 1}', "json").map((span) => span.token)).toEqual([
      "punctuation",
      "property",
      "operator",
      "plain",
      "number",
      "punctuation",
    ]);
    expect(highlightCode("plain words", "plain")).toEqual([{ text: "plain words", token: "plain" }]);
    expect(highlightCode("", "go")).toEqual([]);
    expect(highlightCode("a.b", "typescript").map((span) => span.token)).toEqual([
      "plain",
      "punctuation",
      "property",
    ]);
  });

  test("recognizes literals, markup, and decorators the old scanner missed", () => {
    const tokensOf = (line: string, filetype: string) =>
      highlightCode(line, filetype)
        .filter((span) => span.text.trim())
        .map((span) => [span.text.trim(), span.token]);
    expect(tokensOf("const re = /ab+c/gi.test(s)", "typescript")).toContainEqual(["/ab+c/gi", "regex"]);
    expect(tokensOf("const n = a / b / c", "typescript").filter(([, token]) => token === "regex")).toEqual([]);
    expect(tokensOf("return <Card title=\"x\" onClick={go}>", "typescript")).toEqual([
      ["return", "keyword"],
      ["<", "punctuation"],
      ["Card", "tag"],
      ["title", "attribute"],
      ["=", "operator"],
      ['"x"', "string"],
      ["onClick", "attribute"],
      ["=", "operator"],
      ["{", "punctuation"],
      ["go", "plain"],
      ["}>", "punctuation"],
    ]);
    expect(tokensOf("if (a < b) {", "typescript")).not.toContainEqual(["b", "tag"]);
    expect(tokensOf("`hi ${name}!`", "typescript")).toEqual([
      ["`hi", "string"],
      ["${", "punctuation"],
      ["name", "plain"],
      ["}", "punctuation"],
      ["!`", "string"],
    ]);
    expect(tokensOf("@Injectable() class Svc {}", "typescript")[0]).toEqual(["@Injectable", "attribute"]);
    expect(tokensOf("#[derive(Debug)] struct Point;", "rust")).toEqual([
      ["#[derive(Debug)]", "attribute"],
      ["struct", "keyword"],
      ["Point", "type"],
      [";", "punctuation"],
    ]);
    expect(tokensOf("MAX_RETRIES = 3", "python")[0]).toEqual(["MAX_RETRIES", "constant"]);
    expect(tokensOf("x = None", "python")).toContainEqual(["None", "constant"]);
    expect(tokensOf("SELECT id FROM users WHERE active = true", "sql")).toEqual([
      ["SELECT", "keyword"],
      ["id", "plain"],
      ["FROM", "keyword"],
      ["users", "plain"],
      ["WHERE", "keyword"],
      ["active", "plain"],
      ["=", "operator"],
      ["true", "constant"],
    ]);
    expect(tokensOf(".card { color: #fff; }", "css")).toEqual([
      [".card", "tag"],
      ["{", "punctuation"],
      ["color", "property"],
      [":", "operator"],
      ["#fff", "number"],
      [";", "punctuation"],
      ["}", "punctuation"],
    ]);
    expect(tokensOf('<a href="/x">hi</a>', "html")).toEqual([
      ["<", "punctuation"],
      ["a", "tag"],
      ["href", "attribute"],
      ["=", "operator"],
      ['"/x"', "string"],
      [">", "punctuation"],
      ["hi", "plain"],
      ["</", "punctuation"],
      ["a", "tag"],
      [">", "punctuation"],
    ]);
    expect(tokensOf("## Title", "markdown")).toEqual([["## Title", "heading"]]);
    expect(tokensOf("- use `x` now", "markdown")).toEqual([
      ["-", "keyword"],
      ["use", "plain"],
      ["`x`", "string"],
      ["now", "plain"],
    ]);
  });

  test("carries block comments and multi-line strings across lines", () => {
    const spans = highlightLines(
      ["const a = 1; /* start", "still comment", "end */ const b = 2;"],
      "typescript",
    );
    expect(spans[1]).toEqual([{ text: "still comment", token: "comment" }]);
    expect(spans[2][0]).toEqual({ text: "end */", token: "comment" });
    expect(spans[2].map((span) => span.token)).toContain("keyword");
    const python = highlightLines(['x = """doc', "more", 'done"""', "y = 1"], "python");
    expect(python[1]).toEqual([{ text: "more", token: "string" }]);
    expect(python[2][0]).toEqual({ text: 'done"""', token: "string" });
    expect(python[3].map((span) => span.token)).toContain("number");
    const template = highlightLines(["const s = `line one", "line two`;"], "typescript");
    expect(template[1][0].token).toBe("string");
    // State never leaks across an independent call.
    expect(highlightCode("still comment", "typescript")).toEqual([{ text: "still comment", token: "plain" }]);
    const state = highlightLine("/* open", "go");
    expect(state.state.mode).toBe("block-comment");
    expect(highlightLine("closed */ x", "go", state.state).state).toEqual(INITIAL_HIGHLIGHT_STATE);
  });

  test("parses markdown blocks and inline emphasis for chat rendering", () => {
    expect(parseInline("use `x` and **bold** or *it* and [a](http://b)")).toEqual([
      { text: "use ", style: "plain" },
      { text: "x", style: "code" },
      { text: " and ", style: "plain" },
      { text: "bold", style: "bold" },
      { text: " or ", style: "plain" },
      { text: "it", style: "italic" },
      { text: " and ", style: "plain" },
      { text: "a", style: "link" },
    ]);
    expect(parseInline("snake_case_name stays")).toEqual([
      { text: "snake_case_name stays", style: "plain" },
    ]);
    const lines = parseMarkdown(
      "# Title\n\n- one\n  - two\n1. first\n> quoted\n---\n```ts\nconst a = 1\n```\ntail",
    );
    expect(lines.map((line) => line.kind)).toEqual([
      "heading",
      "blank",
      "bullet",
      "bullet",
      "numbered",
      "quote",
      "rule",
      "fence",
      "code",
      "paragraph",
    ]);
    expect(lines[0].level).toBe(1);
    expect(lines[3].indent).toBe(1);
    expect(lines[4].marker).toBe("1.");
    expect(lines[8].language).toBe("typescript");
    expect(lines[8].spans[0].text).toBe("const a = 1");
    expect(parseMarkdown("```\nunterminated").map((line) => line.kind)).toEqual(["fence", "code"]);
  });

  test("builds a context meter and filters palette commands", () => {
    expect(contextMeter(undefined)).toBeUndefined();
    expect(contextMeter({ totalTokens: 100 })).toBeUndefined();
    const meter = contextMeter({ totalTokens: 2_500_000, contextTokens: 50_000, contextWindow: 200_000 }, 8)!;
    expect(meter.filled).toBe(2);
    expect(meter.label).toBe("50.0K · 25%");
    expect(renderMeter(meter)).toBe("▰▰▱▱▱▱▱▱");
    expect(renderMeter(contextMeter({ contextTokens: 900, contextWindow: 100 }, 4)!)).toBe("▰▰▰▰");
    expect(contextMeter({ totalTokens: 2_500_000, contextWindow: 200_000 })).toBeUndefined();
    expect(contextMeter({ contextTokens: 0, contextWindow: 200_000 })?.label).toBe("0 · 0%");
    expect(contextMeter({ contextTokens: Number.NaN, contextWindow: 200_000 })).toBeUndefined();
    const commands = [
      { id: "new", label: "New session", key: "n" },
      { id: "theme", label: "Change theme", key: "t", hint: "colors" },
      { id: "quit", label: "Quit", key: "q" },
    ];
    expect(filterPaletteCommands(commands, "").map((c) => c.id)).toEqual(["new", "theme", "quit"]);
    expect(filterPaletteCommands(commands, "the").map((c) => c.id)).toEqual(["theme"]);
    expect(filterPaletteCommands(commands, "colors").map((c) => c.id)).toEqual(["theme"]);
    expect(filterPaletteCommands(commands, "q").map((c) => c.id)).toEqual(["quit"]);
    expect(filterPaletteCommands(commands, "zzz")).toEqual([]);
  });

  test("backs off diff polling and derives the sidebar from a ratio", () => {
    expect(nextDiffPollDelay(1_000, false)).toBe(2_000);
    expect(nextDiffPollDelay(4_000, false)).toBe(8_000);
    expect(nextDiffPollDelay(8_000, false)).toBe(8_000);
    expect(nextDiffPollDelay(8_000, true)).toBe(1_000);
    expect(diffTreeWidthForRatio(0.25, 120)).toBe(30);
    expect(diffTreeWidthForRatio(0.9, 120)).toBe(60);
    expect(diffTreeWidthForRatio(0.01, 120)).toBe(20);
    expect(diffTreeWidthForRatio(undefined, 120, 33)).toBe(33);
    expect(diffTreeWidthForRatio(0.5, 70)).toBe(30);
    expect(typewriterReveal(0, 100, 24)).toBe(24);
    expect(typewriterReveal(90, 100, 24)).toBe(100);
    expect(typewriterReveal(120, 100)).toBe(100);
  });

  test("rechecks persisted liveness before deleting a stale selection", async () => {
    const root = await mkdtemp(join(tmpdir(), "ruddr-delete-live-"));
    const stateDir = join(root, "run");
    const registry = join(root, "registry");
    await mkdir(stateDir);
    await mkdir(registry);
    await writeFile(join(stateDir, "state.json"), JSON.stringify({ stateDir, pid: process.pid, status: "active" }));
    const entry = join(registry, "live.run");
    await writeFile(entry, stateDir);
    for (const status of ["stale", "completed"]) {
      await expect(deleteSessionArtifacts({ stateDir, status }, [registry])).rejects.toThrow("stop it before deleting");
      expect(await readFile(entry, "utf8")).toBe(stateDir);
      expect(JSON.parse(await readFile(join(stateDir, "state.json"), "utf8")).status).toBe("active");
    }
    await writeFile(join(stateDir, "state.json"), JSON.stringify({ stateDir, pid: 0, status: "active" }));
    expect((await deleteSessionArtifacts({ stateDir, status: "stale" }, [registry])).removedStateDir).toBe(true);
  });

  test("deletes finished session state and its registry entries only", async () => {
    const root = await mkdtemp(join(tmpdir(), "ruddr-delete-"));
    const stateDir = join(root, "runs", "done.run");
    const registry = join(root, "registry");
    const otherDir = join(root, "runs", "other.run");
    await mkdir(stateDir, { recursive: true });
    await mkdir(otherDir, { recursive: true });
    await mkdir(registry, { recursive: true });
    await writeFile(join(stateDir, "state.json"), JSON.stringify({ stateDir, pid: 1, status: "completed" }));
    await writeFile(join(stateDir, "events.jsonl"), "{}\n");
    await writeFile(join(otherDir, "state.json"), JSON.stringify({ stateDir: otherDir, pid: 1, status: "completed" }));
    await writeFile(join(registry, "a.run"), `${stateDir}\n`);
    await writeFile(join(registry, "b.run"), `${otherDir}\n`);
    expect(sessionIsDeletable({ status: "active" })).toBe(false);
    expect(sessionIsDeletable({ status: "stale" })).toBe(true);
    await expect(
      deleteSessionArtifacts({ stateDir, status: "active" }, [registry]),
    ).rejects.toThrow("stop it before deleting");
    const result = await deleteSessionArtifacts({ stateDir, status: "completed" }, [registry]);
    expect(result).toEqual({ removedStateDir: true, removedRegistryEntries: 1 });
    await expect(readFile(join(stateDir, "state.json"), "utf8")).rejects.toThrow();
    expect(await readFile(join(otherDir, "state.json"), "utf8")).toContain("other.run");
    expect(await readFile(join(registry, "b.run"), "utf8")).toContain("other.run");
    await expect(readFile(join(registry, "a.run"), "utf8")).rejects.toThrow();
    // A directory whose state file points elsewhere is never removed.
    const decoy = join(root, "runs", "decoy.run");
    await mkdir(decoy, { recursive: true });
    await writeFile(join(decoy, "state.json"), JSON.stringify({ stateDir: otherDir, pid: 1, status: "failed" }));
    const decoyResult = await deleteSessionArtifacts({ stateDir: decoy, status: "failed" }, [registry]);
    expect(decoyResult.removedStateDir).toBe(false);
    expect(await readFile(join(decoy, "state.json"), "utf8")).toContain("other.run");
  });

  test("reads the pending update from the launcher environment", () => {
    expect(parseArguments(["--ruddr", "ruddr"], {}).updateAvailable).toBeUndefined();
    expect(
      parseArguments(["--ruddr", "ruddr"], { RUDDR_UPDATE_AVAILABLE: " 0.4.0 " }).updateAvailable,
    ).toBe("0.4.0");
    expect(
      parseArguments(["--ruddr", "ruddr"], { RUDDR_UPDATE_AVAILABLE: "" }).updateAvailable,
    ).toBeUndefined();
  });

  test("switches to the mobile layout at the width threshold", () => {
    expect(layoutForWidth(120, DEFAULT_MOBILE_WIDTH_THRESHOLD, "classic")).toEqual({
      layout: "classic",
      mobile: false,
    });
    expect(layoutForWidth(64, DEFAULT_MOBILE_WIDTH_THRESHOLD, "classic")).toEqual({
      layout: "beta",
      mobile: true,
    });
    expect(layoutForWidth(65, 64, "beta")).toEqual({ layout: "beta", mobile: false });
    expect(layoutForWidth(200, 64, "classic", true).mobile).toBe(true);
    expect(layoutForWidth(40, 0, "classic").mobile).toBe(false);
    expect(parseArguments(["--ruddr", "r", "--mobile"], {}).mobile).toBe(true);
    expect(parseArguments(["--ruddr", "r"], { RUDDR_TUI_MOBILE: "1" }).mobile).toBe(true);
    expect(parseArguments(["--ruddr", "r"], { RUDDER_TUI_MOBILE: "1" }).mobile).toBe(true);
    expect(parseArguments(["--ruddr", "r"], {}).mobile).toBe(false);
  });

  test("clamps wheel scrolling of a list to its content", () => {
    expect(clampScrollOffset(-3, 10, 4)).toBe(0);
    expect(clampScrollOffset(2, 10, 4)).toBe(2);
    expect(clampScrollOffset(9, 10, 4)).toBe(6);
    expect(clampScrollOffset(5, 3, 4)).toBe(0);
    expect(clampScrollOffset(1.6, 10, 4)).toBe(2);
  });

  test("clamps the draggable diff tree width", () => {
    expect(diffTreeWidthForPointer(45, 10, 120)).toBe(35);
    expect(diffTreeWidthForPointer(15, 10, 120)).toBe(20);
    expect(diffTreeWidthForPointer(100, 10, 120)).toBe(60);
    expect(diffTreeWidthForPointer(80, 10, 70)).toBe(30);
  });

  test("reads only complete lines from a bounded tail", async () => {
    const root = await mkdtemp(join(tmpdir(), "ruddr-tui-tail-"));
    const artifact = join(root, "trace.log");
    await writeFile(artifact, "first\nsecond\nthird\n");
    expect(await readTail(artifact, 13)).toBe("second\nthird");
  });

  test("keeps the newest artifact lines visible", () => {
    expect(visibleArtifactTail("one\ntwo\nthree\nfour", 2)).toBe("three\nfour");
  });

  test("turns raw trace lines into a compact semantic activity feed", () => {
    const activities = parseTraceActivities(
      [
        "2026-08-25T18:00:00Z [think] **Inspecting tests** **Planning fix**",
        `2026-08-25T18:00:01Z [in_progress] $ /bin/zsh -lc 'go test ./...'`,
        `2026-08-25T18:00:03Z [completed] $ /bin/zsh -lc 'go test ./...'`,
        "2026-08-25T18:00:03Z [usage] updated",
        "2026-08-25T18:00:03Z [say] Still working.",
        "2026-08-25T18:00:04Z [in_progress] file changes",
        "2026-08-25T18:00:05Z [say] Tests are green.",
        `2026-08-25T18:00:06Z [in_progress] $ /bin/zsh -lc 'rg -n "needle" very-long-path…`,
      ].join("\n"),
    );
    expect(activities).toEqual([
      {
        timestamp: "2026-08-25T18:00:00Z",
        kind: "thought",
        text: "Inspecting tests · Planning fix",
      },
      {
        timestamp: "2026-08-25T18:00:03Z",
        kind: "tool",
        label: "shell",
        text: "go test ./...",
        toolStatus: "completed",
        durationMs: 2000,
      },
      {
        timestamp: "2026-08-25T18:00:04Z",
        kind: "tool",
        label: "files",
        text: "changed",
        toolStatus: "running",
      },
      {
        timestamp: "2026-08-25T18:00:05Z",
        kind: "message",
        text: "Tests are green.",
      },
      {
        timestamp: "2026-08-25T18:00:06Z",
        kind: "tool",
        label: "shell",
        text: 'rg -n "needle" very-long-path…',
        toolStatus: "running",
      },
    ]);
  });

  test("keeps live sessions plus recent history by default", () => {
    const runs = [
      session({ stateDir: "/active", status: "active" }),
      ...Array.from({ length: 24 }, (_, index) =>
        session({ stateDir: `/history-${index}`, status: "completed" }),
      ),
    ];
    expect(visibleSessions(runs, false, [])).toHaveLength(21);
    expect(visibleSessions(runs, false, ["/history-23"])).toHaveLength(22);
    expect(visibleSessions(runs, true, [])).toHaveLength(25);
  });

  test("selects the latest full commentary update instead of the final answer", () => {
    const events = [
      "partial tail record",
      JSON.stringify({
        method: "item/completed",
        params: {
          item: {
            type: "agentMessage",
            phase: "commentary",
            text: "First update",
          },
        },
      }),
      JSON.stringify({
        method: "item/completed",
        params: {
          item: {
            type: "agentMessage",
            phase: "commentary",
            text: "Full latest update",
          },
        },
      }),
      JSON.stringify({
        method: "item/completed",
        params: {
          item: {
            type: "agentMessage",
            phase: "final",
            text: "Long final handoff",
          },
        },
      }),
    ].join("\n");
    expect(latestAgentUpdate(events)).toBe("Full latest update");
  });

  test("joins tool lifecycle events into expandable details", () => {
    const events = [
      JSON.stringify({
        method: "item/started",
        params: {
          item: {
            id: "tool-1",
            type: "commandExecution",
            command: "go test ./...",
            cwd: "/work/parser",
            status: "inProgress",
          },
        },
      }),
      JSON.stringify({
        method: "item/completed",
        params: {
          item: {
            id: "tool-1",
            type: "commandExecution",
            command: "go test ./...",
            cwd: "/work/parser",
            status: "completed",
            aggregatedOutput: "ok parser",
            exitCode: 0,
            durationMs: 1250,
          },
        },
      }),
    ].join("\n");
    expect(parseToolEventDetails(events)).toEqual([
      {
        id: "tool-1",
        type: "commandExecution",
        command: "go test ./...",
        cwd: "/work/parser",
        status: "completed",
        output: "ok parser",
        exitCode: 0,
        durationMs: 1250,
        query: undefined,
      },
    ]);
  });

  test("joins Claude generic tool updates by stable item id", () => {
    const events = [
      JSON.stringify({
        method: "item/started",
        params: {
          item: {
            id: "tool-claude",
            type: "toolCall",
            toolName: "Grep",
            input: {},
            command: "Grep",
            status: "inProgress",
          },
        },
      }),
      JSON.stringify({
        method: "item/updated",
        params: {
          item: {
            id: "tool-claude",
            type: "toolCall",
            toolName: "Grep",
            input: { pattern: "provider", path: "tui" },
            command: "Grep provider",
            status: "inProgress",
          },
        },
      }),
      JSON.stringify({
        method: "item/completed",
        params: {
          item: {
            id: "tool-claude",
            type: "toolCall",
            toolName: "Grep",
            input: { pattern: "provider", path: "tui" },
            command: "Grep provider",
            aggregatedOutput: "tui/core.ts: provider",
            status: "completed",
          },
        },
      }),
    ].join("\n");
    expect(parseToolEventDetails(events)).toEqual([
      expect.objectContaining({
        id: "tool-claude",
        type: "toolCall",
        toolName: "Grep",
        input: { pattern: "provider", path: "tui" },
        status: "completed",
        output: "tui/core.ts: provider",
      }),
    ]);
  });

  test("joins sub-agent activity to the child final response", () => {
    const events = [
      JSON.stringify({
        method: "item/started",
        emittedAtMs: 1787942847416,
        params: {
          threadId: "parent-thread",
          item: {
            id: "subagent-completed-child-turn",
            type: "subAgentActivity",
            kind: "completed",
            agentThreadId: "child-thread",
            agentPath: "/root/tests_review",
          },
        },
      }),
      JSON.stringify({
        method: "item/completed",
        emittedAtMs: 1787942847000,
        params: {
          threadId: "child-thread",
          item: {
            id: "child-message",
            type: "agentMessage",
            phase: "final_answer",
            text: "Child review found no issues.",
          },
        },
      }),
      JSON.stringify({
        method: "item/completed",
        emittedAtMs: 1787942847417,
        params: {
          threadId: "parent-thread",
          item: {
            id: "subagent-completed-child-turn",
            type: "subAgentActivity",
            kind: "completed",
            agentThreadId: "child-thread",
            agentPath: "/root/tests_review",
          },
        },
      }),
      JSON.stringify({
        method: "turn/completed",
        params: {
          threadId: "child-thread",
          turn: { status: "completed", durationMs: 121565 },
        },
      }),
    ].join("\n");

    expect(parseToolEventDetails(events)).toEqual([
      expect.objectContaining({
        id: "subagent-completed-child-turn",
        type: "subAgentActivity",
        command: "/root/tests_review",
        status: "completed",
        output: "Child review found no issues.",
        durationMs: 121565,
        toolName: "subAgentActivity",
        agentThreadId: "child-thread",
        agentPath: "/root/tests_review",
        activityKind: "completed",
        timestampMs: 1787942847417,
      }),
    ]);
  });

  test("matches bounded-tail sub-agent details to the newest trace rows", () => {
    const activities = parseTraceActivities(
      [
        "2026-08-28T18:45:21Z [completed] subAgentActivity",
        "2026-08-28T18:45:25Z [completed] subAgentActivity",
        "2026-08-28T18:47:22Z [completed] subAgentActivity",
        "2026-08-28T18:47:27Z [completed] subAgentActivity",
      ].join("\n"),
    );
    const details = [
      {
        id: "interacted",
        type: "subAgentActivity",
        status: "completed" as const,
        toolName: "subAgentActivity",
        timestampMs: Date.parse("2026-08-28T18:47:22.964Z"),
      },
      {
        id: "completed",
        type: "subAgentActivity",
        status: "completed" as const,
        toolName: "subAgentActivity",
        timestampMs: Date.parse("2026-08-28T18:47:27.417Z"),
      },
    ];

    expect(
      attachToolDetails(activities, details).map((detail) => detail?.id),
    ).toEqual([undefined, undefined, "interacted", "completed"]);
  });

  test("filters sessions across project, ids, status, and model", () => {
    const runs = [
      session({
        cwd: "/work/parser",
        status: "completed",
        model: "gpt-parser",
      }),
      session({
        stateDir: "/active",
        cwd: "/work/payments",
        threadId: "thread-pay",
      }),
    ];
    expect(filterSessions(runs, "payments")).toHaveLength(1);
    expect(filterSessions(runs, "completed")).toHaveLength(1);
    expect(filterSessions(runs, "PARSER")).toHaveLength(1);
    expect(filterSessions(runs, "thread-pay")).toHaveLength(1);
  });

  test("continues a thread with its working directory and model settings", () => {
    const args = continuationRunArguments(
      session({
        cwd: "/work/parser",
        threadId: "thread-parser",
        model: "gpt-parser",
        effort: "high",
        sandbox: "danger-full-access",
      }),
      "/private/prompt.md",
      "/private/new.run",
    );
    expect(args).toEqual([
      "run",
      "--provider",
      "codex",
      "--cwd",
      "/work/parser",
      "--resume-thread",
      "thread-parser",
      "--prompt-file",
      "/private/prompt.md",
      "--state-dir",
      "/private/new.run",
      "--sandbox",
      "danger-full-access",
      "--approval-policy",
      "never",
      "--idle",
      "--model",
      "gpt-parser",
      "--effort",
      "high",
    ]);
  });

  test("continues Claude with its provider session and no invented model", () => {
    const args = continuationRunArguments(
      session({
        provider: "claude",
        cwd: "/work/claude",
        threadId: "claude-session",
        model: undefined,
      }),
      "/private/prompt.md",
      "/private/claude.run",
    );
    expect(args.slice(0, 8)).toEqual([
      "run",
      "--provider",
      "claude",
      "--cwd",
      "/work/claude",
      "--resume-thread",
      "claude-session",
      "--prompt-file",
    ]);
    expect(args).not.toContain("--model");
  });

  test("keeps statuses distinguishable without color", () => {
    expect(
      new Set(
        [
          "active",
          "starting",
          "completed",
          "failed",
          "interrupted",
          "stale",
        ].map(statusGlyph),
      ).size,
    ).toBe(6);
  });

  test("renders concise list and detail text", () => {
    const value = session({ cwd: "/work/parser", steers: 2 });
    expect(sessionDescription(value)).toContain(
      "active · turn-1234567… · gpt-test",
    );
    expect(sessionDetails(value)).toContain("cwd      /work/parser");
    expect(sessionDetails(value)).toContain("steers 2");
    expect(compactSessionDetails(value).split("\n")).toHaveLength(4);
    expect(compactSessionDetails(value)).not.toContain("thread");
  });

  test("treats Go's zero completion time as an unfinished run", () => {
    const value = session({
      startedAt: "2020-01-01T00:00:00Z",
      completedAt: "0001-01-01T00:00:00Z",
    });
    expect(sessionDetails(value)).not.toContain("runtime  0s");
  });
});

describe("promptable TUI helpers", () => {
  test("labels cumulative token usage without treating it as context", () => {
    expect(
      formatTokenUsage({ totalTokens: 186_100, contextWindow: 1_000_000, costUsd: 0.1 }),
    ).toBe("186.1K total · $0.10");
    expect(formatTokenUsage({ totalTokens: 2_400 })).toBe("2.4K total");
    expect(formatTokenUsage({ totalTokens: 2_400, contextWindow: 1_000 })).toBe("2.4K total");
    expect(formatTokenUsage({ totalTokens: 0 })).toBe("");
    expect(formatTokenUsage(undefined)).toBe("");
    expect(formatTokenUsage({ totalTokens: 1_500_000, costUsd: 12.345 })).toBe("1.5M total · $12.35");
  });

  test("recovers latest root context from older event logs without using totals", () => {
    const usage = (threadId: string, tokens?: number) => JSON.stringify({
      method: "thread/tokenUsage/updated", params: { threadId, tokenUsage: {
        total: { totalTokens: 2_500_000 }, last: tokens === undefined ? null : { totalTokens: tokens }, modelContextWindow: 200_000,
      } },
    });
    const events = [usage("root", 180_000), usage("root", 50_000), usage("subagent", 199_000), '{"partial"'].join("\n");
    expect(contextUsageFromEvents(events, "root")).toEqual({ contextTokens: 50_000, contextWindow: 200_000 });
    expect(contextUsageFromEvents(`${events}\n${usage("root")}`, "root")).toBeUndefined();
    expect(contextUsageFromEvents(usage("root", 0), "root")?.contextTokens).toBe(0);
  });

  test("routes prompts by session status without converting", () => {
    expect(promptModeForSession(session({ status: "active" }))).toBe("steer");
    expect(promptModeForSession(session({ status: "idle" }))).toBe("prompt");
    expect(
      promptModeForSession(session({ status: "completed", threadId: "t", cwd: "/w" })),
    ).toBe("continue");
    expect(promptModeForSession(session({ status: "completed", threadId: undefined }))).toBeUndefined();
    expect(promptModeForSession(session({ status: "starting" }))).toBeUndefined();
  expect(promptModeForSession(session({ status: "stale", threadId: "t", cwd: "/w" }))).toBeUndefined();
    expect(promptModeForSession(undefined)).toBeUndefined();
  });

  test("builds distinct idle-prompt and turn-bound steer commands", () => {
    expect(idlePromptControlArguments("/run-a", "/message.md")).toEqual([
      "prompt",
      "--state-dir",
      "/run-a",
      "--message-file",
      "/message.md",
    ]);
    expect(
      steerControlArguments("/run-a", "turn-a", "/message.md"),
    ).toEqual([
      "steer",
      "--state-dir",
      "/run-a",
      "--expected-turn-id",
      "turn-a",
      "--message-file",
      "/message.md",
    ]);
  });

  test("binds an open prompt to its original compatible session", () => {
    const original = session({ stateDir: "/run-a", status: "active" });
    const other = session({ stateDir: "/run-b", status: "active" });
    const target = promptTargetForSession(original);
    expect(target).toEqual({
      stateDir: "/run-a",
      route: "steer",
      turnId: "turn-1234567890",
    });

    const refreshedOriginal = { ...original, steers: 1 };
    expect(resolvePromptTarget([other, refreshedOriginal], target!)).toBe(
      refreshedOriginal,
    );
    expect(resolvePromptTarget([other], target!)).toBeUndefined();
    expect(
      resolvePromptTarget(
        [other, { ...refreshedOriginal, status: "idle" }],
        target!,
      ),
    ).toBeUndefined();
    expect(
      resolvePromptTarget(
        [other, { ...refreshedOriginal, turnId: "turn-next" }],
        target!,
      ),
    ).toBeUndefined();
  });

  test("keeps a continuation target valid across terminal statuses", () => {
    const completed = session({
      stateDir: "/run-a",
      status: "completed",
      threadId: "thread-a",
      cwd: "/work/a",
    });
    const target = promptTargetForSession(completed)!;
    const failed = { ...completed, status: "failed" };

    expect(target.route).toBe("continue");
    expect(resolvePromptTarget([failed], target)).toBe(failed);

    const newer = Array.from({ length: 20 }, (_, index) =>
      session({
        stateDir: `/run-newer-${index}`,
        status: "completed",
        completedAt: `2026-08-26T00:${String(index).padStart(2, "0")}:00Z`,
      }),
    );
    const all = [...newer, completed];
    expect(visibleSessions(all, false, [])).not.toContain(completed);
    expect(resolvePromptTarget(all, target)).toBe(completed);
  });

  test("distinguishes the idle glyph and ranks idle sessions live", () => {
    expect(statusGlyph("idle")).toBe("◌");
    const shown = visibleSessions([session({ status: "idle" })], false, [], 0);
    expect(shown).toHaveLength(1);
  });

  test("builds picker options for OpenCode and Pi", () => {
    const options = modelPickerOptions(FALLBACK_MODELS);
    const codexDefault = options.find((option) => option.value === "codex/gpt-5.6-sol");
    expect(codexDefault?.name).toContain("*");
    const astra = options.find((option) => option.value === "codex/gpt-6-astra");
    expect(astra?.name).toBe("GPT-6-Astra");
    expect(astra?.disabled).toBe(false);
    const fable51 = options.find((option) => option.value === "claude/claude-fable-5-1");
    const opencode = options.find((option) => option.model.provider === "opencode");
    const pi = options.find((option) => option.model.provider === "pi");
    expect(fable51?.name).toBe("Claude Fable 5.1");
    expect(fable51?.disabled).toBe(false);
    expect(opencode?.disabled).toBe(false);
    expect(pi?.disabled).toBe(false);
    expect(pi?.model.efforts).toEqual(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);
  });

  test("falls back to the embedded catalog on bad JSON", () => {
    expect(parseModelCatalog("not json")).toBe(FALLBACK_MODELS);
    expect(parseModelCatalog("[]")).toBe(FALLBACK_MODELS);
    const parsed = parseModelCatalog(
      JSON.stringify([{ provider: "codex", id: "gpt-x", available: true }]),
    );
    expect(parsed).toHaveLength(1);
    expect(parsed[0].id).toBe("gpt-x");
  });

  test("new session arguments always run idle", () => {
    const args = newSessionRunArguments({
      provider: "claude",
      model: "claude-fable-5",
      cwd: "/work/app",
      promptFile: "/tmp/p.md",
      stateDirectory: "/tmp/run",
    });
    expect(args).toContain("--idle");
    expect(args.slice(0, 3)).toEqual(["run", "--provider", "claude"]);
    expect(args).toContain("claude-fable-5");
    const resumed = newSessionRunArguments({
      provider: "codex",
      cwd: "/w",
      promptFile: "/p",
      stateDirectory: "/s",
      resumeThreadId: "session-9",
    });
    expect(resumed).toContain("--resume-thread");
    expect(resumed).toContain("session-9");
  });

  test("continuations now run idle and accept model overrides", () => {
    const args = continuationRunArguments(
      session({ threadId: "t-1", cwd: "/w", model: "gpt-test" }),
      "/tmp/p.md",
      "/tmp/run",
      { model: "gpt-5.6-terra" },
    );
    expect(args).toContain("--idle");
    expect(args).toContain("gpt-5.6-terra");
    expect(args).not.toContain("gpt-test");
  });

  test("parses deja hits into resumable sessions", () => {
    const json = JSON.stringify({
      hits: [
        { resume: "codex resume abc-123", project: "app", date: "2026-08-29", openingPrompt: "fix the bug" },
        { resume: "claude --resume def-456", project: "web", date: "2026-08-28", openingPrompt: "add tests" },
        { project: "no-resume" },
        { resume: "pi something else" },
      ],
    });
    const hits = parseDejaHits(json);
    expect(hits).toHaveLength(2);
    expect(hits[0]).toMatchObject({ provider: "codex", sessionId: "abc-123", project: "app" });
    expect(hits[1]).toMatchObject({ provider: "claude", sessionId: "def-456" });
    expect(parseDejaHits("broken")).toEqual([]);
  });

  test("builds a chat transcript from events.jsonl", () => {
    const lines = [
      { method: "item/completed", params: { threadId: "root", item: { type: "userMessage", origin: "ruddr", text: "do the thing" } } },
      { method: "item/started", params: { threadId: "root", item: { id: "cmd-1", type: "commandExecution", command: "bun test" } } },
      { method: "item/completed", params: { threadId: "root", item: { id: "cmd-1", type: "commandExecution", command: "bun test", exitCode: 0 } } },
      { method: "item/completed", params: { threadId: "sub", item: { id: "sub-1", type: "commandExecution", command: "hidden" } } },
      { method: "item/completed", params: { threadId: "root", item: { type: "agentMessage", text: "done" } } },
      { method: "item/completed", params: { threadId: "root", item: { type: "userMessage", origin: "ruddr", text: "now this" } } },
      { method: "item/completed", params: { threadId: "root", item: { id: "prompt-rejected", type: "userMessage", origin: "ruddr", text: "do not show" } } },
      { method: "ruddr/prompt/rejected", params: { promptId: "prompt-rejected" } },
    ]
      .map((line) => JSON.stringify(line))
      .join("\n");
    const entries = parseChatTranscript(lines);
    expect(entries.map((entry) => entry.kind)).toEqual(["user", "tool", "agent", "user"]);
    expect(entries[1]).toMatchObject({ text: "bun test", status: "completed" });
    expect(entries.some((entry) => entry.text === "hidden")).toBe(false);
  });

  test("filters old transcripts by the selected root thread", () => {
  const lines = [
    { method: "item/completed", params: { threadId: "sub", item: { type: "agentMessage", text: "hidden" } } },
    { method: "item/completed", params: { threadId: "root", item: { type: "agentMessage", text: "visible" } } },
  ]
    .map((line) => JSON.stringify(line))
    .join("\n");
  expect(parseChatTranscript(lines, "root").map((entry) => entry.text)).toEqual(["visible"]);
  });
});


describe("artifact read lifecycle", () => {
  test("a newer tab read wins even when the old read finishes last", async () => {
    const reads = new LatestRead();
    const oldRead = deferred();
    let visible = "";
    const render = async (tab: string, ready: Promise<void>) => {
      const current = reads.begin();
      await ready;
      if (current()) visible = tab;
    };
    const old = render("diff", oldRead.promise);
    await render("chat", Promise.resolve());
    oldRead.resolve();
    await old;
    expect(visible).toBe("chat");
  });

  test("shutdown retires pending reads and rejects later reads", () => {
    const reads = new LatestRead();
    const current = reads.begin();
    expect(current()).toBe(true);
    reads.stop();
    expect(current()).toBe(false);
    expect(reads.begin()()).toBe(false);
  });
});
