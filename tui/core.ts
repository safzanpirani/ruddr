import { open, readdir, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";

export interface RunState {
  version: number;
  provider?: "codex" | "claude" | "opencode" | "pi";
  pid: number;
  childPid?: number;
  status: string;
  threadId?: string;
  turnId?: string;
  model?: string;
  effort?: string;
  cwd?: string;
  sandbox?: string;
  stateDir: string;
  socketPath?: string;
  eventsPath?: string;
  tracePath?: string;
  outputPath?: string;
  steers?: number;
  idle?: boolean;
  turns?: number;
  tokenUsage?: TokenUsage;
  startedAt?: string;
  updatedAt?: string;
  completedAt?: string;
  error?: string;
}

export interface TokenUsage {
  inputTokens?: number;
  cachedInputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  contextTokens?: number;
  contextWindow?: number;
  costUsd?: number;
}

export interface Session extends RunState {
  stateFile: string;
}

export type TraceActivityKind =
  "thought" | "tool" | "message" | "warning" | "error" | "status";
export type ToolActivityStatus = "running" | "completed" | "failed";

export interface TraceActivity {
  timestamp: string;
  kind: TraceActivityKind;
  text: string;
  label?: string;
  toolStatus?: ToolActivityStatus;
  durationMs?: number;
}

export interface ToolEventDetail {
  id: string;
  type: string;
  command?: string;
  cwd?: string;
  status: ToolActivityStatus;
  output?: string;
  exitCode?: number;
  durationMs?: number;
  query?: string;
  toolName?: string;
  input?: Record<string, unknown>;
  agentThreadId?: string;
  agentPath?: string;
  activityKind?: string;
  timestampMs?: number;
}

export interface DiscoverOptions {
  roots: string[];
  stateDirs: string[];
  registryDirs?: string[];
  processAlive?: (pid: number) => boolean;
}

export type Artifact = "chat" | "trace" | "output" | "diff";

export type GitDiffLineKind =
  | "file"
  | "hunk"
  | "addition"
  | "deletion"
  | "metadata"
  | "context";

export interface GitDiffLine {
  kind: GitDiffLineKind;
  text: string;
  /** Path of the file this line belongs to (from the `diff --git` header). */
  path?: string;
  /** Pre-image line number for context and deletion lines. */
  oldLine?: number;
  /** Post-image line number for context and addition lines. */
  newLine?: number;
}

export interface GitDiffSummary {
  files: number;
  additions: number;
  deletions: number;
}

export interface GitDiffHunkHeader {
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  context: string;
}

export function parseGitDiffHunkHeader(
  text: string,
): GitDiffHunkHeader | undefined {
  const match = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)$/.exec(text);
  if (!match) return;
  return {
    oldStart: Number(match[1]),
    oldCount: match[2] === undefined ? 1 : Number(match[2]),
    newStart: Number(match[3]),
    newCount: match[4] === undefined ? 1 : Number(match[4]),
    context: match[5] ?? "",
  };
}

export function gitDiffFilePath(headerText: string): string {
  const match = /^diff --git (?:"?a\/)(.+?)"? (?:"?b\/)(.+?)"?$/.exec(headerText);
  return match?.[2] ?? headerText.replace(/^diff --git /, "");
}

export function gitDiffSummary(lines: readonly GitDiffLine[]): GitDiffSummary {
  let files = 0;
  let additions = 0;
  let deletions = 0;
  for (const line of lines) {
    if (line.kind === "file") files++;
    else if (line.kind === "addition") additions++;
    else if (line.kind === "deletion") deletions++;
  }
  return { files, additions, deletions };
}

/**
 * Row indices that stay visible when some files are folded. File headers are
 * always visible so a folded file can be unfolded again.
 */
export function visibleGitDiffLineIndices(
  lines: readonly GitDiffLine[],
  collapsedFiles: ReadonlySet<string>,
): number[] {
  const visible: number[] = [];
  for (const [index, line] of lines.entries()) {
    if (line.kind === "file" || !line.path || !collapsedFiles.has(line.path))
      visible.push(index);
  }
  return visible;
}

export interface GitDiffFileStats {
  additions: number;
  deletions: number;
  status: "M" | "A" | "D" | "R";
}

export function gitDiffFileStats(
  lines: readonly GitDiffLine[],
): Map<string, GitDiffFileStats> {
  const stats = new Map<string, GitDiffFileStats>();
  let current: GitDiffFileStats | undefined;
  for (const line of lines) {
    if (line.kind === "file") {
      current = { additions: 0, deletions: 0, status: "M" };
      stats.set(line.path ?? gitDiffFilePath(line.text), current);
    } else if (!current) continue;
    else if (line.text.startsWith("new file mode ")) current.status = "A";
    else if (line.text.startsWith("deleted file mode ")) current.status = "D";
    else if (line.text.startsWith("rename from ")) current.status = "R";
    else if (line.kind === "addition") current.additions++;
    else if (line.kind === "deletion") current.deletions++;
  }
  return stats;
}

/** Width of a line-number gutter wide enough for every number in the diff. */
export function gitDiffGutterWidth(lines: readonly GitDiffLine[]): number {
  let largest = 0;
  for (const line of lines) {
    if (line.oldLine !== undefined && line.oldLine > largest)
      largest = line.oldLine;
    if (line.newLine !== undefined && line.newLine > largest)
      largest = line.newLine;
  }
  return Math.max(2, String(largest).length);
}

export interface GitDiffTreeEntry {
  label: string;
  rowIndex: number;
  kind: "directory" | "file";
  path: string;
  expanded?: boolean;
  collapsed?: boolean;
  status?: "M" | "A" | "D" | "R";
}

export function diffTreeWidthForPointer(
  pointerX: number,
  containerX: number,
  containerWidth: number,
): number {
  const minimum = 20;
  const maximum = Math.max(minimum, Math.min(60, containerWidth - 40));
  return Math.max(minimum, Math.min(maximum, pointerX - containerX));
}

export function parseGitDiff(content: string): GitDiffLine[] {
  if (!content) return [];
  let path: string | undefined;
  let oldLine = 0;
  let newLine = 0;
  let inHunk = false;
  return content.replace(/\n$/, "").split("\n").map((text) => {
    let kind: GitDiffLineKind = "context";
    if (text.startsWith("diff --git ")) {
      kind = "file";
      path = gitDiffFilePath(text);
      inHunk = false;
      return { kind, text, path };
    }
    if (text.startsWith("@@")) {
      kind = "hunk";
      const header = parseGitDiffHunkHeader(text);
      inHunk = Boolean(header);
      oldLine = header?.oldStart ?? 0;
      newLine = header?.newStart ?? 0;
      return { kind, text, path };
    }
    if (text.startsWith("+") && !text.startsWith("+++")) {
      kind = "addition";
      const line: GitDiffLine = { kind, text, path };
      if (inHunk) line.newLine = newLine++;
      return line;
    }
    if (text.startsWith("-") && !text.startsWith("---")) {
      kind = "deletion";
      const line: GitDiffLine = { kind, text, path };
      if (inHunk) line.oldLine = oldLine++;
      return line;
    }
    if (
      text.startsWith("index ") ||
      text.startsWith("---") ||
      text.startsWith("+++") ||
      text.startsWith("new file mode ") ||
      text.startsWith("deleted file mode ") ||
      text.startsWith("similarity index ") ||
      text.startsWith("rename from ") ||
      text.startsWith("rename to ") ||
      text.startsWith("Binary files ") ||
      text === "\\ No newline at end of file"
    ) {
      kind = "metadata";
      return { kind, text, path };
    }
    const line: GitDiffLine = { kind, text, path };
    if (inHunk) {
      line.oldLine = oldLine++;
      line.newLine = newLine++;
    }
    return line;
  });
}

export function nextGitDiffBoundary(
  lines: GitDiffLine[],
  selectedRow: number,
  kind: "hunk" | "file",
  direction: number,
): number | undefined {
  const boundaries = lines.flatMap((line, index) =>
    line.kind === kind ? [index] : [],
  );
  if (boundaries.length === 0) return;
  if (direction > 0)
    return boundaries.find((index) => index > selectedRow) ?? boundaries[0];
  return (
    [...boundaries].reverse().find((index) => index < selectedRow) ??
    boundaries[boundaries.length - 1]
  );
}

export function gitDiffTree(
  lines: GitDiffLine[],
  collapsedDirectories: ReadonlySet<string> = new Set(),
  collapsedFiles: ReadonlySet<string> = new Set(),
): GitDiffTreeEntry[] {
  const files: Array<{
    path: string;
    rowIndex: number;
    additions: number;
    deletions: number;
    status: "M" | "A" | "D" | "R";
  }> = [];
  let current: (typeof files)[number] | undefined;
  for (const [rowIndex, line] of lines.entries()) {
    if (line.kind === "file") {
      const path = line.path ?? gitDiffFilePath(line.text);
      current = { path, rowIndex, additions: 0, deletions: 0, status: "M" };
      files.push(current);
    } else if (current && line.text.startsWith("new file mode "))
      current.status = "A";
    else if (current && line.text.startsWith("deleted file mode "))
      current.status = "D";
    else if (current && line.text.startsWith("rename from "))
      current.status = "R";
    else if (current && line.kind === "addition") current.additions++;
    else if (current && line.kind === "deletion") current.deletions++;
  }

  const entries: GitDiffTreeEntry[] = [];
  const directories = new Set<string>();
  for (const file of files) {
    const parts = file.path.split("/");
    let hidden = false;
    for (let depth = 0; depth < parts.length - 1; depth++) {
      const directory = parts.slice(0, depth + 1).join("/");
      if (!directories.has(directory)) {
        directories.add(directory);
        const expanded = !collapsedDirectories.has(directory);
        entries.push({
          label: `${"  ".repeat(depth)}${expanded ? "▾" : "▸"} 󰉋 ${parts[depth]}`,
          rowIndex: file.rowIndex,
          kind: "directory",
          path: directory,
          expanded,
        });
      }
      if (collapsedDirectories.has(directory)) {
        hidden = true;
        break;
      }
    }
    if (hidden) continue;
    const depth = parts.length - 1;
    const counts = `+${file.additions} −${file.deletions}`;
    entries.push({
      label: `${"  ".repeat(depth)}  󰈔 ${parts[depth]}  ${file.status}  ${counts}`,
      rowIndex: file.rowIndex,
      kind: "file",
      path: file.path,
      status: file.status,
      ...(collapsedFiles.has(file.path) ? { collapsed: true } : {}),
    });
  }
  return entries;
}

export function artifactAllowsTextSelection(artifact: Artifact): boolean {
  return artifact === "output" || artifact === "diff";
}

export function nextArtifact(artifact: Artifact): Artifact {
  return artifact === "chat"
    ? "trace"
    : artifact === "trace"
      ? "output"
      : artifact === "output"
        ? "diff"
        : "chat";
}
export type Focus = "sessions" | "artifact" | "steer";
export type TUILayout = "classic" | "beta";

export interface TUIArguments {
  ruddr: string;
  roots: string[];
  stateDirs: string[];
  interval: number;
  includeAll: boolean;
  theme?: string;
  beta: boolean;
  /** Force the narrow single-column layout regardless of terminal width. */
  mobile: boolean;
  /** A newer release the Ruddr binary found on its last daily check. */
  updateAvailable?: string;
}

export interface ViewState {
  selectedStateDir?: string;
  artifact: Artifact;
  focus: Focus;
  interruptArmedUntil: number;
}

export type ViewEvent =
  | { type: "sessions"; sessions: Session[] }
  | { type: "select"; stateDir?: string }
  | { type: "toggle-artifact" }
  | { type: "toggle-focus" }
  | { type: "open-steer" }
  | { type: "close-steer" }
  | { type: "arm-interrupt"; now: number }
  | { type: "clear-interrupt" };

export const initialViewState: ViewState = {
  artifact: "chat",
  focus: "sessions",
  interruptArmedUntil: 0,
};

export function parseArguments(
  argv: string[],
  environment: Record<string, string | undefined> = process.env,
): TUIArguments {
  const roots: string[] = [];
  const stateDirs: string[] = [];
  let ruddr = "";
  let interval = 500;
  // Every registered run stays listed; --all remains accepted for scripts.
  let includeAll = true;
  let theme: string | undefined;
  let beta =
    (environment.RUDDR_TUI_BETA ?? environment.RUDDER_TUI_BETA) === "1";
  let mobile =
    (environment.RUDDR_TUI_MOBILE ?? environment.RUDDER_TUI_MOBILE) === "1";
  const updateAvailable = environment.RUDDR_UPDATE_AVAILABLE?.trim() || undefined;
  for (let index = 0; index < argv.length; index++) {
    const argument = argv[index];
    const value = argv[index + 1];
    if (
      (argument === "--ruddr" ||
        argument === "--root" ||
        argument === "--state-dir" ||
        argument === "--interval" ||
        argument === "--theme") &&
      !value
    )
      throw new Error(`${argument} requires a value`);
    if (argument === "--ruddr") {
      ruddr = value;
      index++;
    } else if (argument === "--root") {
      roots.push(value);
      index++;
    } else if (argument === "--state-dir") {
      stateDirs.push(value);
      index++;
    } else if (argument === "--interval") {
      interval = parseInterval(value);
      index++;
    } else if (argument === "--theme") {
      theme = value;
      index++;
    } else if (argument === "--all") includeAll = true;
    else if (argument === "--beta") beta = true;
    else if (argument === "--mobile") mobile = true;
    else throw new Error(`unknown TUI argument ${argument}`);
  }
  if (!ruddr)
    throw new Error("--ruddr is required (launch the TUI through ruddr tui)");
  if (roots.length === 0 && stateDirs.length === 0)
    roots.push(join(process.cwd(), ".scratch"));
  return {
    ruddr,
    roots,
    stateDirs,
    interval,
    includeAll,
    theme,
    beta,
    mobile,
    updateAvailable,
  };
}

function parseInterval(value: string): number {
  const match = /^(\d+)(ms|s)$/.exec(value);
  if (!match)
    throw new Error(
      "--interval must use milliseconds or seconds, for example 500ms or 2s",
    );
  const amount = Number(match[1]);
  const milliseconds = match[2] === "s" ? amount * 1_000 : amount;
  if (milliseconds < 100) throw new Error("--interval must be at least 100ms");
  return milliseconds;
}

export const DEFAULT_MOBILE_WIDTH_THRESHOLD = 64;

export interface ResolvedLayout {
  layout: TUILayout;
  /** Narrow single-column chrome with a tappable action bar. */
  mobile: boolean;
}

/**
 * Picks the layout for a terminal width. At or below the threshold (phones,
 * split panes) the chat-first layout takes over with touch chrome; above it
 * the launch layout returns.
 */
export function layoutForWidth(
  width: number,
  threshold: number,
  launchLayout: TUILayout,
  forceMobile = false,
): ResolvedLayout {
  if (forceMobile || width <= threshold) return { layout: "beta", mobile: true };
  return { layout: launchLayout, mobile: false };
}

export type DashboardNavigation =
  | "show-sessions"
  | "focus-sessions"
  | "focus-artifact";

export function dashboardNavigation(
  layout: TUILayout,
  focus: Focus,
  key: string,
): DashboardNavigation | undefined {
  if (key === "tab") {
    if (layout === "beta") return "show-sessions";
    return focus === "sessions" ? "focus-artifact" : "focus-sessions";
  }
  if (layout === "classic" && key === "escape" && focus === "sessions")
    return "focus-artifact";
  return undefined;
}

export function sessionsPanelTitle(options: {
  layout: TUILayout;
  liveCount: number;
  recentCount: number;
  query?: string;
}): string {
  const filterSuffix =
    options.layout === "beta" && options.query ? ` · /${options.query}` : "";
  const actions =
    options.layout === "beta" ? " · Enter open · Esc close" : "";
  return ` sessions · ${options.liveCount} live · ${options.recentCount} recent${filterSuffix}${actions} `;
}

export function emptyPromptHint(layout: TUILayout): string {
  return layout === "classic"
    ? "n new session · Tab focus"
    : "n new session · Tab sessions";
}

export function contextualHelp(options: {
  layout: TUILayout;
  focus: Focus;
  session?: Pick<Session, "status">;
  hasQuery: boolean;
  dejaAvailable?: boolean;
  compact?: boolean;
  artifact?: Artifact;
}): string {
  if (options.focus === "sessions") {
    return options.layout === "classic"
      ? "j/k select · / filter · Tab chat · Esc chat"
      : "j/k select · / filter · Enter open · Esc close";
  }
  const matches = options.hasQuery ? " · n/N matches" : "";
  const stoppable =
    options.session?.status === "active" || options.session?.status === "idle";
  const tab = options.layout === "classic" ? "Tab focus" : "Tab sessions";
  const stop = stoppable ? " · x x" : "";
  if (options.artifact === "diff") {
    const fold = options.compact ? "Enter fold" : "Enter fold file";
    return options.compact
      ? `]c hunk · ]f file · ${fold} · s prompt${stop} · ${tab} · q quit`
      : `]c ]f next hunk/file · [c [f previous · ${fold} · s prompt${matches}${stop ? " · x x stop" : ""} · ${tab} · q quit`;
  }
  if (options.compact)
    return `s prompt · n new · o tabs${matches}${stop} · ${tab} · q quit`;
  const find = options.dejaAvailable ? " · f find" : "";
  const action = stoppable ? " · x x stop" : "";
  return `s prompt · n new · m model${find}${matches}${action} · ${tab} · q quit`;
}

export interface HelpSegment {
  key: string;
  label: string;
}

/**
 * Splits "s prompt · x x stop · Tab focus" into key/label pairs so the footer
 * can color the key and dim the label. Repeated single-character keys such as
 * "x x" and paired keys such as "]c ]f" stay together as the key.
 */
export function helpSegments(help: string): HelpSegment[] {
  return help
    .split(" · ")
    .map((segment) => segment.trim())
    .filter(Boolean)
    .map((segment) => {
      const words = segment.split(" ");
      let keyWords = 1;
      if (
        words.length > 2 &&
        (words[0] === words[1] ||
          (/^[\[\]]\w$/.test(words[0]) && /^[\[\]]\w$/.test(words[1])))
      )
        keyWords = 2;
      return {
        key: words.slice(0, keyWords).join(" "),
        label: words.slice(keyWords).join(" "),
      };
    });
}

const SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

export function spinnerFrame(tick: number): string {
  return SPINNER_FRAMES[((tick % SPINNER_FRAMES.length) + SPINNER_FRAMES.length) % SPINNER_FRAMES.length];
}

/** Mixes `tint` into `base`; `amount` 0 returns base, 1 returns tint. */
export function blendHex(base: string, tint: string, amount: number): string {
  const parse = (value: string): [number, number, number] => {
    const hex = value.replace("#", "").slice(0, 6);
    const full =
      hex.length === 3
        ? hex.split("").map((char) => char + char).join("")
        : hex.padEnd(6, "0");
    return [
      parseInt(full.slice(0, 2), 16),
      parseInt(full.slice(2, 4), 16),
      parseInt(full.slice(4, 6), 16),
    ];
  };
  const ratio = Math.max(0, Math.min(1, amount));
  const from = parse(base);
  const to = parse(tint);
  const channel = (index: number) =>
    Math.round(from[index] + (to[index] - from[index]) * ratio)
      .toString(16)
      .padStart(2, "0");
  return `#${channel(0)}${channel(1)}${channel(2)}`;
}

export type StatusKind = "info" | "success" | "warning" | "error";

export function statusGlyphForKind(kind: StatusKind): string {
  switch (kind) {
    case "success":
      return "✓";
    case "warning":
      return "!";
    case "error":
      return "×";
    default:
      return "›";
  }
}

export function statusTimeoutMs(kind: StatusKind): number {
  return kind === "error" || kind === "warning" ? 12_000 : 6_000;
}

// Each asynchronous read owns a validity check. New reads and shutdown retire it.
export class LatestRead {
  private generation = 0;
  private stopped = false;

  begin(): () => boolean {
    const generation = ++this.generation;
    return () => !this.stopped && generation === this.generation;
  }

  stop(): void {
    this.stopped = true;
    this.generation++;
  }
}

export class AsyncTaskGate {
  private active?: Promise<void>;
  private stopping = false;

  run(task: () => Promise<void>): Promise<void> {
    if (this.stopping || this.active) return Promise.resolve();
    const active = Promise.resolve()
      .then(task)
      .finally(() => {
        if (this.active === active) this.active = undefined;
      });
    this.active = active;
    return active;
  }

  async stop(): Promise<void> {
    this.stopping = true;
    await this.active;
  }
}

export function reduceView(state: ViewState, event: ViewEvent): ViewState {
  switch (event.type) {
    case "sessions": {
      const selectedExists = event.sessions.some(
        (session) => session.stateDir === state.selectedStateDir,
      );
      return {
        ...state,
        selectedStateDir: selectedExists
          ? state.selectedStateDir
          : event.sessions[0]?.stateDir,
      };
    }
    case "select":
      return {
        ...state,
        selectedStateDir: event.stateDir,
        interruptArmedUntil: 0,
      };
    case "toggle-artifact":
      return { ...state, artifact: nextArtifact(state.artifact) };
    case "toggle-focus":
      return {
        ...state,
        focus: state.focus === "artifact" ? "sessions" : "artifact",
      };
    case "open-steer":
      return { ...state, focus: "steer", interruptArmedUntil: 0 };
    case "close-steer":
      return { ...state, focus: "sessions" };
    case "arm-interrupt":
      return { ...state, interruptArmedUntil: event.now + 2_000 };
    case "clear-interrupt":
      return { ...state, interruptArmedUntil: 0 };
  }
}

export async function discoverSessions(
  options: DiscoverOptions,
): Promise<Session[]> {
  const stateFiles = new Set<string>();
  for (const stateDir of options.stateDirs) {
    stateFiles.add(resolve(stateDir, "state.json"));
  }
  for (const root of options.roots) {
    await collectStateFiles(resolve(root), stateFiles);
  }
  for (const registryDir of options.registryDirs ?? defaultRegistryDirectories()) {
    for (const stateDir of await readRegisteredStateDirs(registryDir)) {
      stateFiles.add(resolve(stateDir, "state.json"));
    }
  }

  const sessions = (
    await Promise.all(
      [...stateFiles].map(async (stateFile) => {
        try {
          const parsed = JSON.parse(
            await readFile(stateFile, "utf8"),
          ) as RunState;
          if (
            !parsed.stateDir ||
            typeof parsed.pid !== "number" ||
            typeof parsed.status !== "string"
          )
            return undefined;
          parsed.provider ??= "codex";
          const alive = options.processAlive ?? defaultProcessAlive;
          if (!isTerminalStatus(parsed.status) && !alive(parsed.pid)) {
            parsed.status = "stale";
            parsed.error = `Ruddr pid ${parsed.pid} is not running; persisted state is stale`;
          }
          return { ...parsed, stateFile } satisfies Session;
        } catch {
          return undefined;
        }
      }),
    )
  ).filter((session): session is Session => session !== undefined);

  return sessions.sort(compareSessions);
}

async function readRegisteredStateDirs(registryDir: string): Promise<string[]> {
  let entries;
  try {
    entries = await readdir(registryDir, { withFileTypes: true });
  } catch {
    return [];
  }
  const paths = await Promise.all(
    entries
      .filter((entry) => entry.isFile() && entry.name.endsWith(".run"))
      .map(async (entry) => {
        try {
          return (await readFile(join(registryDir, entry.name), "utf8")).trim();
        } catch {
          return "";
        }
      }),
  );
  return paths.filter(Boolean);
}

function defaultRegistryDirectories(): string[] {
  const configured =
    process.env.RUDDR_REGISTRY_DIR ||
    process.env.RUDDER_REGISTRY_DIR ||
    process.env.CODEX_RUDDER_REGISTRY_DIR;
  if (configured) return [resolve(configured)];
  const stateHome =
    process.env.XDG_STATE_HOME || join(homedir(), ".local", "state");
  // Registries written before the rename stay visible.
  return [
    join(stateHome, "ruddr", "runs"),
    join(stateHome, "rudder", "runs"),
    join(stateHome, "codex-rudder", "runs"),
  ];
}

async function collectStateFiles(
  directory: string,
  results: Set<string>,
): Promise<void> {
  let entries;
  try {
    entries = await readdir(directory, { withFileTypes: true });
  } catch {
    return;
  }
  await Promise.all(
    entries.map(async (entry) => {
      if (entry.isSymbolicLink()) return;
      const entryPath = join(directory, entry.name);
      if (entry.isFile() && entry.name === "state.json") {
        results.add(entryPath);
      } else if (entry.isDirectory() && !SKIPPED_DIRECTORIES.has(entry.name)) {
        await collectStateFiles(entryPath, results);
      }
    }),
  );
}

const SKIPPED_DIRECTORIES = new Set([".git", "node_modules"]);

function defaultProcessAlive(pid: number): boolean {
  if (pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error instanceof Error && "code" in error && error.code === "EPERM";
  }
}

function isTerminalStatus(status: string): boolean {
  return (
    status === "completed" || status === "failed" || status === "interrupted"
  );
}

function compareSessions(left: Session, right: Session): number {
  const rank = (status: string) =>
    status === "active"
      ? 0
      : status === "idle"
        ? 1
        : status === "starting"
          ? 2
          : status === "stale"
            ? 4
            : 3;
  const byRank = rank(left.status) - rank(right.status);
  if (byRank !== 0) return byRank;
  return Date.parse(right.updatedAt ?? "") - Date.parse(left.updatedAt ?? "");
}

export async function readTail(
  path: string | undefined,
  maxBytes = 128 * 1024,
): Promise<string> {
  if (!path) return "";
  let handle;
  try {
    handle = await open(path, "r");
    const { size } = await handle.stat();
    const start = Math.max(0, size - maxBytes);
    const readStart = Math.max(0, start - 1);
    const buffer = Buffer.alloc(size - readStart);
    await handle.read(buffer, 0, buffer.length, readStart);
    const startsOnLineBoundary = start === 0 || buffer[0] === 0x0a;
    let text = buffer.subarray(start === 0 ? 0 : 1).toString("utf8");
    if (!startsOnLineBoundary) {
      const firstNewline = text.indexOf("\n");
      text = firstNewline >= 0 ? text.slice(firstNewline + 1) : text;
    }
    return text.trimEnd();
  } catch {
    return "";
  } finally {
    await handle?.close();
  }
}

export function visibleArtifactTail(content: string, maxLines: number): string {
  if (maxLines <= 0) return "";
  return content.split("\n").slice(-maxLines).join("\n");
}

export function parseTraceActivities(content: string): TraceActivity[] {
  const activities: TraceActivity[] = [];
  for (const line of content.split("\n")) {
    const match = /^(\S+) \[([^\]]+)\](?: (.*))?$/.exec(line);
    if (!match) continue;
    const [, timestamp, tag, rawText = ""] = match;
    if (tag === "usage") continue;
    if (tag === "think") {
      const text = cleanThought(rawText);
      if (text) activities.push({ timestamp, kind: "thought", text });
      continue;
    }
    if (tag === "say") {
      if (rawText)
        activities.push({ timestamp, kind: "message", text: rawText });
      continue;
    }
    if (tag === "in_progress" || tag === "completed" || tag === "failed") {
      const toolStatus: ToolActivityStatus =
        tag === "in_progress"
          ? "running"
          : tag === "completed"
            ? "completed"
            : "failed";
      const tool = parseTool(rawText);
      if (toolStatus !== "running") {
        const runningIndex = activities.findLastIndex(
          (activity) =>
            activity.kind === "tool" &&
            activity.toolStatus === "running" &&
            activity.label === tool.label &&
            activity.text === tool.text,
        );
        if (runningIndex >= 0) {
          const started = Date.parse(activities[runningIndex].timestamp);
          const finished = Date.parse(timestamp);
          activities[runningIndex] = {
            timestamp,
            kind: "tool",
            label: tool.label,
            text: tool.text,
            toolStatus,
            durationMs:
              Number.isFinite(started) && Number.isFinite(finished)
                ? Math.max(0, finished - started)
                : undefined,
          };
          continue;
        }
      }
      activities.push({
        timestamp,
        kind: "tool",
        label: tool.label,
        text: tool.text,
        toolStatus,
      });
      continue;
    }
    if (tag === "warn") {
      activities.push({ timestamp, kind: "warning", text: rawText });
    } else if (tag === "error") {
      activities.push({ timestamp, kind: "error", text: rawText });
    } else {
      activities.push({ timestamp, kind: "status", label: tag, text: rawText });
    }
  }
  const latestMessage = activities.findLastIndex(
    (activity) => activity.kind === "message",
  );
  return activities.filter(
    (activity, index) => activity.kind !== "message" || index === latestMessage,
  );
}

export function visibleSessions(
  sessions: Session[],
  includeAll: boolean,
  explicitStateDirs: string[],
  historyLimit = 20,
): Session[] {
  if (includeAll) return sessions;
  const explicit = new Set(explicitStateDirs);
  let historyCount = 0;
  return sessions.filter((session) => {
    const live =
      session.status === "active" ||
      session.status === "idle" ||
      session.status === "starting";
    if (live || explicit.has(session.stateDir)) return true;
    if (historyCount >= historyLimit) return false;
    historyCount++;
    return true;
  });
}

export function latestAgentUpdate(content: string): string | undefined {
  let latest: string | undefined;
  for (const line of content.split("\n")) {
    try {
      const event = JSON.parse(line) as {
        method?: string;
        params?: { item?: { type?: string; phase?: string; text?: string } };
      };
      const item = event.params?.item;
      if (
        event.method === "item/completed" &&
        item?.type === "agentMessage" &&
        item.phase === "commentary" &&
        item.text
      ) {
        latest = item.text;
      }
    } catch {
      // A bounded tail may begin in the middle of a JSONL record.
    }
  }
  return latest;
}

export function parseToolEventDetails(content: string): ToolEventDetail[] {
  const byID = new Map<string, ToolEventDetail>();
  const order: string[] = [];
  const messagesByThread = new Map<string, string>();
  const turnsByThread = new Map<
    string,
    { status?: string; durationMs?: number }
  >();
  for (const line of content.split("\n")) {
    try {
      const event = JSON.parse(line) as {
        method?: string;
        emittedAtMs?: number;
        params?: {
          threadId?: string;
          turn?: { status?: string; durationMs?: number };
          item?: {
            id?: string;
            type?: string;
            text?: string;
            command?: string;
            cwd?: string;
            status?: string;
            aggregatedOutput?: string;
            exitCode?: number;
            durationMs?: number;
            query?: string;
            toolName?: string;
            input?: Record<string, unknown>;
            agentThreadId?: string;
            agentPath?: string;
            kind?: string;
          };
        };
      };
      const item = event.params?.item;
      const threadId = event.params?.threadId;
      if (
        event.method === "item/completed" &&
        item?.type === "agentMessage" &&
        item.text &&
        threadId
      )
        messagesByThread.set(threadId, item.text);
      if (event.method === "turn/completed" && threadId)
        turnsByThread.set(threadId, event.params?.turn ?? {});
      if (!item?.id || !item.type || !event.method?.startsWith("item/"))
        continue;
      if (
        item.type !== "commandExecution" &&
        item.type !== "webSearch" &&
        item.type !== "fileChange" &&
        item.type !== "toolCall" &&
        item.type !== "subAgentActivity"
      )
        continue;
      const previous = byID.get(item.id);
      if (!previous) order.push(item.id);
      const eventStatus =
        event.method === "item/started"
          ? "running"
          : event.method === "item/updated" && item.status === "inProgress"
            ? "running"
          : item.status === "failed" ||
              (item.exitCode !== undefined && item.exitCode !== 0)
            ? "failed"
            : "completed";
      byID.set(item.id, {
        ...previous,
        id: item.id,
        type: item.type,
        command:
          item.command ??
          (item.type === "subAgentActivity" ? item.agentPath : undefined) ??
          previous?.command,
        cwd: item.cwd ?? previous?.cwd,
        status: eventStatus,
        output: item.aggregatedOutput ?? previous?.output,
        exitCode: item.exitCode ?? previous?.exitCode,
        durationMs: item.durationMs ?? previous?.durationMs,
        query: item.query ?? previous?.query,
        toolName:
          item.toolName ??
          (item.type === "subAgentActivity" ? "subAgentActivity" : undefined) ??
          previous?.toolName,
        input: item.input ?? previous?.input,
        agentThreadId: item.agentThreadId ?? previous?.agentThreadId,
        agentPath: item.agentPath ?? previous?.agentPath,
        activityKind: item.kind ?? previous?.activityKind,
        timestampMs: event.emittedAtMs ?? previous?.timestampMs,
      });
    } catch {
      // A bounded tail may begin in the middle of a JSONL record.
    }
  }
  for (const detail of byID.values()) {
    if (detail.type !== "subAgentActivity" || !detail.agentThreadId) continue;
    detail.output = messagesByThread.get(detail.agentThreadId) ?? detail.output;
    const turn = turnsByThread.get(detail.agentThreadId);
    detail.durationMs = turn?.durationMs ?? detail.durationMs;
    if (turn?.status)
      detail.status = turn.status === "completed" ? "completed" : "failed";
  }
  return order.flatMap((id) => {
    const detail = byID.get(id);
    return detail ? [detail] : [];
  });
}

export function attachToolDetails(
  activities: TraceActivity[],
  details: ToolEventDetail[],
): Array<ToolEventDetail | undefined> {
  const used = new Set<string>();
  return activities.map((activity) => {
    if (activity.kind !== "tool") return undefined;
    const summary = activity.text.replace(/…$/, "").toLocaleLowerCase();
    const candidates = details.filter((detail) => {
      if (used.has(detail.id)) return false;
      const text =
        `${detail.command || ""} ${detail.query || ""} ${detail.toolName || ""}`.toLocaleLowerCase();
      if (summary && (text.includes(summary) || summary.includes(text.trim())))
        return true;
      if (activity.label === "files") return detail.type === "fileChange";
      if (activity.label?.toLocaleLowerCase().includes("websearch"))
        return detail.type === "webSearch";
      if (activity.label?.toLocaleLowerCase() === "subagentactivity")
        return detail.type === "subAgentActivity";
      return activity.label === "shell" && detail.type === "commandExecution";
    });
    let match = candidates[0];
    if (activity.label?.toLocaleLowerCase() === "subagentactivity") {
      const activityTime = Date.parse(activity.timestamp);
      match = candidates
        .filter(
          (detail) =>
            Number.isFinite(activityTime) &&
            detail.timestampMs !== undefined &&
            Math.abs(detail.timestampMs - activityTime) < 2_000,
        )
        .sort(
          (left, right) =>
            Math.abs(left.timestampMs! - activityTime) -
            Math.abs(right.timestampMs! - activityTime),
        )[0];
    }
    if (match) used.add(match.id);
    return match;
  });
}

export type ChatEntryKind = "user" | "agent" | "tool" | "thought";

export interface ChatEntry {
  kind: ChatEntryKind;
  text: string;
  status?: ToolActivityStatus;
  itemId?: string;
}

// Builds a conversation transcript from events.jsonl: the user's prompts
// (synthetic userMessage items written by the controller), agent messages,
// and compact one-line tool entries.
export function parseChatTranscript(
  content: string,
  rootThreadId?: string,
): ChatEntry[] {
  const entries: ChatEntry[] = [];
  const toolIndexById = new Map<string, number>();
  const rejectedPromptIds = new Set<string>();
  const rootThreads = new Set<string>(rootThreadId ? [rootThreadId] : []);
  for (const line of content.split("\n")) {
    try {
      const event = JSON.parse(line) as {
        method?: string;
        params?: {
          threadId?: string;
          promptId?: string;
          item?: {
            id?: string;
            type?: string;
            text?: string;
            command?: string;
            status?: string;
            summary?: unknown;
            toolName?: string;
            query?: string;
            exitCode?: number;
            origin?: string;
          };
        };
      };
      if (
        event.method === "ruddr/prompt/rejected" &&
        event.params?.promptId
      ) {
        rejectedPromptIds.add(event.params.promptId);
        continue;
      }
      const item = event.params?.item;
      const threadId = event.params?.threadId;
      if (item?.type === "userMessage" && item.origin === "ruddr" && threadId)
        rootThreads.add(threadId);
      if (!item?.type || !event.method?.startsWith("item/")) continue;
      // Sub-agent items carry a different threadId; keep the root conversation.
      if (threadId && rootThreads.size > 0 && !rootThreads.has(threadId))
        continue;
      if (item.type === "userMessage") {
        const text = item.text?.trim();
        if (event.method === "item/completed" && text)
          entries.push({ kind: "user", text, itemId: item.id });
        continue;
      }
      if (item.type === "agentMessage") {
        const text = item.text?.trim();
        if (event.method === "item/completed" && text)
          entries.push({ kind: "agent", text });
        continue;
      }
      if (item.type === "reasoning") {
        if (event.method === "item/completed") {
          const text = cleanThought(flattenSummary(item.summary));
          if (text) entries.push({ kind: "thought", text });
        }
        continue;
      }
      if (
        item.type === "commandExecution" ||
        item.type === "fileChange" ||
        item.type === "webSearch" ||
        item.type === "toolCall" ||
        item.type === "subAgentActivity"
      ) {
        if (!item.id) continue;
        const rawLabel =
          item.command ||
          item.query ||
          item.toolName ||
          (item.type === "fileChange" ? "file changes" : item.type);
        const flat = rawLabel.split(/\s+/).join(" ").trim();
        const label = flat.length > 160 ? `${flat.slice(0, 160)}…` : flat;
        const status: ToolActivityStatus =
          event.method === "item/started"
            ? "running"
            : item.status === "failed" ||
                (item.exitCode !== undefined && item.exitCode !== 0)
              ? "failed"
              : event.method === "item/completed"
                ? "completed"
                : "running";
        const existing = toolIndexById.get(item.id);
        const entry: ChatEntry = {
          kind: "tool",
          text: label,
          status,
          itemId: item.id,
        };
        if (existing !== undefined) entries[existing] = entry;
        else {
          toolIndexById.set(item.id, entries.length);
          entries.push(entry);
        }
      }
    } catch {
      // A bounded tail may begin in the middle of a JSONL record.
    }
  }
  return entries.filter(
    (entry) =>
      entry.kind !== "user" ||
      !entry.itemId ||
      !rejectedPromptIds.has(entry.itemId),
  );
}

function flattenSummary(value: unknown): string {
  if (typeof value === "string") return value.trim();
  if (Array.isArray(value))
    return value
      .map((part) => flattenSummary(part))
      .filter(Boolean)
      .join(" ");
  if (value && typeof value === "object") {
    const record = value as Record<string, unknown>;
    return flattenSummary(record.text ?? record.content ?? "");
  }
  return "";
}

export function filterSessions(sessions: Session[], query: string): Session[] {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return sessions;
  return sessions.filter((session) =>
    [
      session.status,
      session.provider,
      session.cwd,
      basename(session.cwd || dirname(session.stateDir)),
      session.threadId,
      session.turnId,
      session.model,
      session.effort,
    ]
      .filter(Boolean)
      .some((value) => value!.toLocaleLowerCase().includes(needle)),
  );
}

function cleanThought(text: string): string {
  return text
    .replace(/\*\*\s+\*\*/g, " · ")
    .replace(/\*\*/g, "")
    .trim();
}

function parseTool(text: string): { label: string; text: string } {
  if (text === "file changes") return { label: "files", text: "changed" };
  const command = text.startsWith("$ ") ? text.slice(2) : text;
  const shell = /^(?:\/bin\/)?(?:zsh|bash|sh) -lc (.+)$/.exec(command);
  if (shell) {
    const quote = shell[1][0];
    let script = quote === '"' || quote === "'" ? shell[1].slice(1) : shell[1];
    if (script.endsWith(quote)) script = script.slice(0, -1);
    return { label: "shell", text: script };
  }
  const executable = command.split(/\s+/, 1)[0];
  return { label: basename(executable) || "tool", text: command };
}

export function sessionLabel(session: Session): string {
  const glyph = statusGlyph(session.status);
  const project =
    basename(session.cwd || dirname(session.stateDir)) || session.stateDir;
  const age = formatAge(session.updatedAt);
  return `${glyph} ${project}  ${age}`;
}

export function sessionDescription(session: Session): string {
  return `${session.provider ?? "codex"} · ${session.status} · ${shortID(session.turnId || session.threadId)} · ${session.model || "default model"}`;
}

export function compactSessionDetails(session: Session | undefined): string {
  if (!session) return sessionDetails(session);
  const elapsed = formatElapsed(session.startedAt, session.completedAt);
  return [
    `status   ${statusGlyph(session.status)} ${session.status}    ${elapsed}`,
    `provider ${session.provider ?? "codex"}`,
    `model    ${session.model || "—"}${session.effort ? ` / ${session.effort}` : ""}`,
    `cwd      ${session.cwd || "—"}`,
    session.tokenUsage ? `tokens   ${formatTokenUsage(session.tokenUsage)}` : "",
    session.error ? `error    ${session.error}` : "",
  ]
    .filter(Boolean)
    .join("\n");
}

export function continuationRunArguments(
  session: Session,
  promptFile: string,
  stateDirectory: string,
  overrides: { model?: string; effort?: string; provider?: string } = {},
): string[] {
  if (!session.threadId || !session.cwd)
    throw new Error("continuation requires a thread and working directory");
  const args = [
    "run",
    "--provider",
    overrides.provider ?? session.provider ?? "codex",
    "--cwd",
    session.cwd,
    "--resume-thread",
    session.threadId,
    "--prompt-file",
    promptFile,
    "--state-dir",
    stateDirectory,
    "--sandbox",
    session.sandbox || "workspace-write",
    "--approval-policy",
    "never",
    "--idle",
  ];
  const model = overrides.model ?? session.model;
  const effort = overrides.effort ?? session.effort;
  if (model) args.push("--model", model);
  if (effort) args.push("--effort", effort);
  return args;
}

export function formatTokenCount(value: number): string {
  if (value >= 1_000_000)
    return `${(value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return `${value}`;
}

// Session totals describe billed work, not the current context window.
export function formatTokenUsage(usage: TokenUsage | undefined): string {
  if (!usage) return "";
  const total = usage.totalTokens ?? 0;
  const parts: string[] = [];
  if (total > 0) {
    parts.push(`${formatTokenCount(total)} total`);
  }
  if (usage.costUsd && usage.costUsd > 0)
    parts.push(`$${usage.costUsd.toFixed(2)}`);
  return parts.join(" · ");
}

export type PromptRoute = "steer" | "prompt" | "continue";

export interface PromptTarget {
  stateDir: string;
  route: PromptRoute;
  turnId?: string;
}

export function idlePromptControlArguments(
  stateDir: string,
  messageFile: string,
): string[] {
  return ["prompt", "--state-dir", stateDir, "--message-file", messageFile];
}

export function steerControlArguments(
  stateDir: string,
  turnId: string,
  messageFile: string,
): string[] {
  return [
    "steer",
    "--state-dir",
    stateDir,
    "--expected-turn-id",
    turnId,
    "--message-file",
    messageFile,
  ];
}

// Routes a typed prompt: active turns get steered, idle sessions get a new
// turn over the control socket, finished threads get a continuation run.
// Never converts one route into another.
export function promptModeForSession(
  session: Session | undefined,
): PromptRoute | undefined {
  if (!session) return undefined;
  if (session.status === "active") return "steer";
  if (session.status === "idle") return "prompt";
  if (
    (session.status === "completed" ||
      session.status === "failed" ||
      session.status === "interrupted") &&
    session.threadId &&
    session.cwd
  )
    return "continue";
  return undefined;
}

export function promptTargetForSession(
  session: Session | undefined,
): PromptTarget | undefined {
  const route = promptModeForSession(session);
  if (!session || !route) return undefined;
  if (route === "steer") {
    return session.turnId
      ? { stateDir: session.stateDir, route, turnId: session.turnId }
      : undefined;
  }
  return { stateDir: session.stateDir, route };
}

export function resolvePromptTarget(
  sessions: Session[],
  target: PromptTarget,
): Session | undefined {
  const session = sessions.find(
    (candidate) => candidate.stateDir === target.stateDir,
  );
  if (promptModeForSession(session) !== target.route) return undefined;
  if (target.route === "steer" && session?.turnId !== target.turnId)
    return undefined;
  return session;
}

export interface ModelInfo {
  provider: string;
  id?: string;
  label?: string;
  efforts?: string[];
  contextWindow?: number;
  default?: boolean;
  available: boolean;
  note?: string;
}

// Embedded fallback for older ruddr binaries without `ruddr models`.
export const FALLBACK_MODELS: ModelInfo[] = [
  { provider: "codex", id: "gpt-5.6-sol", label: "GPT-5.6-Sol", default: true, available: true },
  { provider: "codex", id: "gpt-5.6-terra", label: "GPT-5.6-Terra", available: true },
  { provider: "codex", id: "gpt-5.6-luna", label: "GPT-5.6-Luna", available: true },
  { provider: "claude", id: "claude-fable-5-1", label: "Claude Fable 5.1", available: true },
  { provider: "claude", id: "claude-fable-5", label: "Claude Fable 5", available: true },
  { provider: "claude", id: "claude-opus-5", label: "Claude Opus 5", default: true, available: true },
  { provider: "claude", id: "claude-sonnet-5", label: "Claude Sonnet 5", available: true },
  { provider: "claude", id: "claude-haiku-4-5-20251001", label: "Claude Haiku 4.5", available: true },
  { provider: "opencode", id: "openrouter/deepseek/deepseek-v4-flash-vision-exp", label: "DeepSeek V4 Flash Vision Exp", default: true, available: true },
  { provider: "pi", id: "openrouter/deepseek/deepseek-v4-flash-vision-exp", label: "DeepSeek V4 Flash Vision Exp", efforts: ["off", "minimal", "low", "medium", "high", "xhigh", "max"], default: true, available: true },
];

export interface ModelPickerOption {
  name: string;
  description: string;
  value: string;
  disabled: boolean;
  model: ModelInfo;
}

export function modelPickerOptions(models: ModelInfo[]): ModelPickerOption[] {
  return models.map((model) => ({
    name: model.available
      ? `${model.label || model.id}${model.default ? " *" : ""}`
      : `${model.provider} (${model.note || "unavailable"})`,
    description: model.available
      ? model.provider
      : model.note || "unavailable",
    value: model.available ? `${model.provider}/${model.id}` : model.provider,
    disabled: !model.available,
    model,
  }));
}

export function parseModelCatalog(json: string): ModelInfo[] {
  try {
    const parsed = JSON.parse(json) as ModelInfo[];
    if (!Array.isArray(parsed)) return FALLBACK_MODELS;
    const valid = parsed.filter(
      (model) =>
        model &&
        typeof model.provider === "string" &&
        (model.available === false || typeof model.id === "string"),
    );
    return valid.length > 0 ? valid : FALLBACK_MODELS;
  } catch {
    return FALLBACK_MODELS;
  }
}

export interface NewSessionOptions {
  provider: string;
  model?: string;
  effort?: string;
  cwd: string;
  promptFile: string;
  stateDirectory: string;
  resumeThreadId?: string;
}

export function newSessionRunArguments(options: NewSessionOptions): string[] {
  const args = [
    "run",
    "--provider",
    options.provider,
    "--cwd",
    options.cwd,
    "--prompt-file",
    options.promptFile,
    "--state-dir",
    options.stateDirectory,
    "--sandbox",
    "workspace-write",
    "--approval-policy",
    "never",
    "--idle",
  ];
  if (options.resumeThreadId)
    args.push("--resume-thread", options.resumeThreadId);
  if (options.model) args.push("--model", options.model);
  if (options.effort) args.push("--effort", options.effort);
  return args;
}

export interface DejaHit {
  provider: "codex" | "claude" | "opencode" | "pi";
  sessionId: string;
  project: string;
  date: string;
  openingPrompt: string;
}

// deja find --json hits carry a `resume` command like "codex resume <id>" or
// "claude --resume <id>"; the id doubles as ruddr's --resume-thread value.
export function parseDejaHits(json: string): DejaHit[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return [];
  }
  const hits =
    parsed && typeof parsed === "object" && Array.isArray((parsed as { hits?: unknown[] }).hits)
      ? ((parsed as { hits: unknown[] }).hits as Array<Record<string, unknown>>)
      : [];
  const results: DejaHit[] = [];
  for (const hit of hits) {
    const resume = typeof hit.resume === "string" ? hit.resume : "";
    const match =
      /^claude --resume (\S+)$/.exec(resume) ??
      /^codex resume (\S+)$/.exec(resume);
    if (!match) continue;
    results.push({
      provider: resume.startsWith("claude") ? "claude" : "codex",
      sessionId: match[1],
      project: typeof hit.project === "string" ? hit.project : "",
      date: typeof hit.date === "string" ? hit.date : "",
      openingPrompt:
        typeof hit.openingPrompt === "string" ? hit.openingPrompt : "",
    });
  }
  return results;
}

export function sessionDetails(session: Session | undefined): string {
  if (!session) {
    return `No active Ruddr sessions.\n\nWatching the global registry and ${join(process.cwd(), ".scratch")}.\nNew runs appear automatically.`;
  }
  const elapsed = formatElapsed(session.startedAt, session.completedAt);
  return [
    `status   ${statusGlyph(session.status)} ${session.status}`,
    `provider ${session.provider ?? "codex"}`,
    `model    ${session.model || "—"}${session.effort ? ` / ${session.effort}` : ""}`,
    `thread   ${session.threadId || "—"}`,
    `turn     ${session.turnId || "—"}`,
    `cwd      ${session.cwd || "—"}`,
    `runtime  ${elapsed}    pid ${session.pid}    steers ${session.steers ?? 0}${session.turns ? `    turns ${session.turns}` : ""}`,
    session.tokenUsage ? `tokens   ${formatTokenUsage(session.tokenUsage)}` : "",
    session.error ? `error    ${session.error}` : "",
  ]
    .filter(Boolean)
    .join("\n");
}

export function statusGlyph(status: string): string {
  switch (status) {
    case "active":
      return "●";
    case "idle":
      return "◌";
    case "starting":
      return "◐";
    case "completed":
      return "✓";
    case "failed":
      return "×";
    case "interrupted":
      return "■";
    case "stale":
      return "!";
    default:
      return "?";
  }
}

function shortID(value: string | undefined): string {
  if (!value) return "no turn";
  return value.length > 14 ? `${value.slice(0, 12)}…` : value;
}

function formatAge(value: string | undefined, now = Date.now()): string {
  const timestamp = Date.parse(value ?? "");
  if (!Number.isFinite(timestamp)) return "unknown";
  const seconds = Math.max(0, Math.floor((now - timestamp) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

export function formatElapsed(
  startValue: string | undefined,
  endValue: string | undefined,
  now = Date.now(),
): string {
  const start = Date.parse(startValue ?? "");
  if (!Number.isFinite(start)) return "unknown";
  const parsedEnd = Date.parse(endValue ?? "");
  const end =
    Number.isFinite(parsedEnd) && parsedEnd >= start ? parsedEnd : now;
  const seconds = Math.max(0, Math.floor((end - start) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

// ---------------------------------------------------------------------------
// Stateful syntax highlighting. A hand-written scanner keeps the diff and chat
// renderers deterministic and dependency-free: OpenTUI's Tree-sitter path needs
// a worker plus downloaded grammars for most languages a session touches.

export type CodeToken =
  | "plain"
  | "keyword"
  | "string"
  | "comment"
  | "number"
  | "type"
  | "function"
  | "punctuation"
  | "operator"
  | "property"
  | "tag"
  | "attribute"
  | "constant"
  | "regex"
  | "heading";

export interface CodeSpan {
  text: string;
  token: CodeToken;
}

/**
 * Scanner state carried between consecutive lines of one file so block
 * comments and multi-line strings keep their color past the first line.
 */
export interface HighlightState {
  mode: "code" | "block-comment" | "string";
  /** Closing delimiter for a multi-line string (``` ` ```, `"""`, `'''`). */
  close?: string;
}

export const INITIAL_HIGHLIGHT_STATE: HighlightState = { mode: "code" };

interface LanguageSpec {
  keywords: Set<string>;
  types: Set<string>;
  constants: Set<string>;
  lineComment: string[];
  blockComment?: [string, string];
  /** Quote characters whose strings may span lines. */
  multilineQuotes: string[];
  /** Triple-quote openers (Python docstrings). */
  tripleQuotes: string[];
  regexLiterals: boolean;
  jsx: boolean;
  decorators: "@" | "#[" | undefined;
  caseInsensitiveKeywords: boolean;
  /** Treat `$name` as a variable (shell). */
  dollarVariables: boolean;
  /** `#` starts a comment only when not part of `$#` or `${#`. */
  hashComment: boolean;
}

const words = (list: string): Set<string> => new Set(list.split(/\s+/).filter(Boolean));

const LANGUAGES: Record<string, LanguageSpec> = {
  typescript: {
    keywords: words(
      "abstract as async await break case catch class const continue debugger declare default delete do else enum export extends finally for from function get if implements import in instanceof interface is keyof let module namespace new of override package private protected public readonly return satisfies set static super switch this throw try type typeof var void while with yield",
    ),
    types: words("any bigint boolean never number object string symbol unknown Array Promise Record Partial Readonly Map Set Date Error RegExp"),
    constants: words("true false null undefined NaN Infinity"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ["`"],
    tripleQuotes: [],
    regexLiterals: true,
    jsx: true,
    decorators: "@",
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  go: {
    keywords: words("break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var"),
    types: words("bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uint8 uint16 uint32 uint64 uintptr any comparable"),
    constants: words("true false nil iota"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ["`"],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  python: {
    keywords: words("and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield match case"),
    types: words("int float str bytes bool list dict set tuple object type Optional Any Union List Dict"),
    constants: words("True False None self cls"),
    lineComment: ["#"],
    multilineQuotes: [],
    tripleQuotes: ['"""', "'''"],
    regexLiterals: false,
    jsx: false,
    decorators: "@",
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: true,
  },
  rust: {
    keywords: words("as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return static struct super trait type unsafe use where while"),
    types: words("bool char f32 f64 i8 i16 i32 i64 i128 isize str u8 u16 u32 u64 u128 usize String Vec Option Result Box Rc Arc Self"),
    constants: words("true false self None Some Ok Err"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ['"'],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: "#[",
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  shell: {
    keywords: words("if then else elif fi for while until do done case esac function in return exit export local readonly set unset source alias break continue select time"),
    types: words(""),
    constants: words("true false"),
    lineComment: ["#"],
    multilineQuotes: ["'", '"'],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: true,
    hashComment: true,
  },
  json: {
    keywords: words(""),
    types: words(""),
    constants: words("true false null"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  yaml: {
    keywords: words(""),
    types: words(""),
    constants: words("true false null yes no on off ~"),
    lineComment: ["#"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: true,
  },
  c: {
    keywords: words("auto break case catch class const constexpr continue default delete do else enum explicit export extern for friend goto if inline namespace new noexcept operator override private protected public register return sizeof static struct switch template this throw try typedef typename union using virtual volatile while final"),
    types: words("bool char double float int long short signed unsigned void wchar_t size_t int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t string vector map auto var"),
    constants: words("true false nullptr NULL"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  java: {
    keywords: words("abstract assert break case catch class const continue default do else enum extends final finally for goto if implements import instanceof interface native new package private protected public return static strictfp super switch synchronized this throw throws transient try volatile while var record sealed permits yield fun val when object data is in"),
    types: words("boolean byte char double float int long short void String Integer Long Boolean List Map Set Unit"),
    constants: words("true false null"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ['"""'],
    tripleQuotes: ['"""'],
    regexLiterals: false,
    jsx: false,
    decorators: "@",
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  ruby: {
    keywords: words("alias and begin break case class def defined? do else elsif end ensure for if in module next not or redo rescue retry return super then undef unless until when while yield require require_relative attr_reader attr_writer attr_accessor include extend private public protected raise"),
    types: words("String Integer Float Array Hash Symbol Proc"),
    constants: words("true false nil self"),
    lineComment: ["#"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: true,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: true,
  },
  sql: {
    keywords: words("select from where and or not in is null as join left right inner outer full on group by order having limit offset insert into values update set delete create table alter drop index view primary key foreign references default unique check constraint begin commit rollback with recursive union all distinct case when then else end exists between like ilike asc desc returning if cascade transaction grant revoke"),
    types: words("int integer bigint smallint serial bigserial text varchar char boolean bool date timestamp timestamptz time interval numeric decimal real float double json jsonb uuid bytea"),
    constants: words("true false null current_timestamp now"),
    lineComment: ["--"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ["'"],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: true,
    dollarVariables: false,
    hashComment: false,
  },
  css: {
    keywords: words("important media supports import font-face keyframes from to and not only screen print"),
    types: words(""),
    constants: words("inherit initial unset none auto"),
    lineComment: [],
    blockComment: ["/*", "*/"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: true,
    dollarVariables: false,
    hashComment: false,
  },
  html: {
    keywords: words(""),
    types: words(""),
    constants: words(""),
    lineComment: [],
    blockComment: ["<!--", "-->"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: true,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  markdown: {
    keywords: words(""),
    types: words(""),
    constants: words(""),
    lineComment: [],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
};

const EXTENSION_FILETYPES: Record<string, string> = {
  ts: "typescript", tsx: "typescript", mts: "typescript", cts: "typescript",
  js: "typescript", jsx: "typescript", mjs: "typescript", cjs: "typescript",
  go: "go",
  py: "python", pyi: "python",
  rs: "rust",
  sh: "shell", bash: "shell", zsh: "shell", fish: "shell",
  json: "json", jsonc: "json",
  yaml: "yaml", yml: "yaml", toml: "yaml",
  c: "c", h: "c", cc: "c", cpp: "c", hpp: "c", cs: "c", swift: "c", m: "c",
  kt: "java", kts: "java", java: "java", scala: "java",
  rb: "ruby",
  md: "markdown", markdown: "markdown", mdx: "markdown",
  css: "css", scss: "css", less: "css",
  html: "html", htm: "html", svelte: "html", vue: "html", xml: "html", svg: "html",
  sql: "sql",
};

const FENCE_ALIASES: Record<string, string> = {
  js: "typescript", jsx: "typescript", ts: "typescript", tsx: "typescript",
  javascript: "typescript", typescript: "typescript", typescriptreact: "typescript",
  py: "python", python: "python",
  rs: "rust", rust: "rust",
  sh: "shell", bash: "shell", zsh: "shell", shell: "shell", console: "shell",
  c: "c", cpp: "c", "c++": "c", csharp: "c", cs: "c", swift: "c", objc: "c",
  java: "java", kotlin: "java", kt: "java", scala: "java",
  rb: "ruby", ruby: "ruby",
  json: "json", jsonc: "json",
  yaml: "yaml", yml: "yaml", toml: "yaml",
  css: "css", scss: "css",
  html: "html", xml: "html", svg: "html", vue: "html", svelte: "html",
  sql: "sql", psql: "sql",
  md: "markdown", markdown: "markdown",
  go: "go", golang: "go",
};

export function filetypeForPath(path: string | undefined): string {
  if (!path) return "plain";
  const name = basename(path);
  if (name === "Dockerfile" || name === "Makefile" || name.startsWith(".")) return "shell";
  const extension = name.includes(".") ? name.split(".").pop()! : "";
  return EXTENSION_FILETYPES[extension.toLowerCase()] ?? "plain";
}

export function filetypeForFence(info: string | undefined): string {
  const language = (info ?? "").trim().split(/\s+/)[0]?.toLowerCase() ?? "";
  if (!language) return "plain";
  return FENCE_ALIASES[language] ?? EXTENSION_FILETYPES[language] ?? "plain";
}

export function highlightCode(line: string, filetype: string): CodeSpan[] {
  return highlightLine(line, filetype, INITIAL_HIGHLIGHT_STATE).spans;
}

/** Highlights consecutive lines of one file, threading scanner state. */
export function highlightLines(lines: readonly string[], filetype: string): CodeSpan[][] {
  let state = INITIAL_HIGHLIGHT_STATE;
  return lines.map((line) => {
    const result = highlightLine(line, filetype, state);
    state = result.state;
    return result.spans;
  });
}

const WORD = /^[A-Za-z_$][\w$]*/;
const NUMBER = /^(?:0[xX][0-9a-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?[a-zA-Z]{0,3})\b/;
const OPERATOR = /^(?:=>|->|::|\?\?=?|\?\.|\.\.\.?|&&=?|\|\|=?|<<=?|>>>?=?|[-+*/%&|^!=<>]=?|~|\?|:)/;
const PUNCTUATION = /^[{}()[\],;.]+/;

export function highlightLine(
  line: string,
  filetype: string,
  state: HighlightState = INITIAL_HIGHLIGHT_STATE,
): { spans: CodeSpan[]; state: HighlightState } {
  if (line.length === 0) return { spans: [], state };
  const spec = LANGUAGES[filetype];
  if (!spec) return { spans: [{ text: line, token: "plain" }], state };
  if (filetype === "markdown") return { spans: highlightMarkdownLine(line), state };

  const spans: CodeSpan[] = [];
  const push = (text: string, token: CodeToken) => {
    if (!text) return;
    const last = spans[spans.length - 1];
    if (last && last.token === token) last.text += text;
    else spans.push({ text, token });
  };
  let index = 0;
  let mode = state;
  let lastSignificant: CodeSpan | undefined;
  const significant = () => {
    for (let cursor = spans.length - 1; cursor >= 0; cursor--) {
      if (spans[cursor].text.trim()) return spans[cursor];
    }
    return undefined;
  };

  while (index < line.length) {
    const rest = line.slice(index);

    if (mode.mode === "block-comment") {
      const close = spec.blockComment?.[1] ?? "*/";
      const end = rest.indexOf(close);
      if (end < 0) {
        push(rest, "comment");
        index = line.length;
        break;
      }
      push(rest.slice(0, end + close.length), "comment");
      index += end + close.length;
      mode = INITIAL_HIGHLIGHT_STATE;
      continue;
    }

    if (mode.mode === "string") {
      const close = mode.close ?? '"';
      const end = findUnescaped(rest, close);
      if (end < 0) {
        push(rest, "string");
        index = line.length;
        break;
      }
      push(rest.slice(0, end + close.length), "string");
      index += end + close.length;
      mode = INITIAL_HIGHLIGHT_STATE;
      continue;
    }

    // Comments.
    const lineComment = spec.lineComment.find((marker) => rest.startsWith(marker));
    if (
      lineComment &&
      !(spec.hashComment && lineComment === "#" && index > 0 && /[$\w{]/.test(line[index - 1]))
    ) {
      push(rest, "comment");
      index = line.length;
      break;
    }
    if (spec.blockComment && rest.startsWith(spec.blockComment[0])) {
      const [open, close] = spec.blockComment;
      const end = rest.indexOf(close, open.length);
      if (end < 0) {
        push(rest, "comment");
        mode = { mode: "block-comment" };
        index = line.length;
        break;
      }
      push(rest.slice(0, end + close.length), "comment");
      index += end + close.length;
      continue;
    }

    // Strings.
    const triple = spec.tripleQuotes.find((quote) => rest.startsWith(quote));
    if (triple) {
      const end = rest.indexOf(triple, triple.length);
      if (end < 0) {
        push(rest, "string");
        mode = { mode: "string", close: triple };
        index = line.length;
        break;
      }
      push(rest.slice(0, end + triple.length), "string");
      index += end + triple.length;
      continue;
    }
    const quote = rest[0];
    if (quote === '"' || quote === "'" || quote === "`") {
      const end = findUnescaped(rest.slice(1), quote);
      if (end < 0) {
        push(rest, "string");
        if (spec.multilineQuotes.includes(quote)) mode = { mode: "string", close: quote };
        index = line.length;
        break;
      }
      const text = rest.slice(0, end + 2);
      if (quote === "`" && spec.regexLiterals) {
        // Template literal: color interpolations as code.
        pushTemplate(text, push, spec);
      } else push(text, "string");
      index += text.length;
      continue;
    }

    // JSX / HTML tags.
    if (spec.jsx && rest[0] === "<") {
      const tag = /^<\/?[A-Za-z][\w.:-]*/.exec(rest);
      if (tag && (filetype === "html" || looksLikeJsx(line, index))) {
        push(tag[0].slice(0, tag[0].startsWith("</") ? 2 : 1), "punctuation");
        push(tag[0].slice(tag[0].startsWith("</") ? 2 : 1), "tag");
        index += tag[0].length;
        // Attributes until the closing bracket.
        while (index < line.length) {
          const inner = line.slice(index);
          const closeTag = /^\s*\/?>/.exec(inner);
          if (closeTag) {
            push(closeTag[0], "punctuation");
            index += closeTag[0].length;
            break;
          }
          const space = /^\s+/.exec(inner);
          if (space) {
            push(space[0], "plain");
            index += space[0].length;
            continue;
          }
          const attribute = /^[A-Za-z_:@][\w.:-]*/.exec(inner);
          if (attribute) {
            push(attribute[0], "attribute");
            index += attribute[0].length;
            continue;
          }
          if (inner[0] === "=") {
            push("=", "operator");
            index++;
            continue;
          }
          if (inner[0] === '"' || inner[0] === "'") {
            const end = findUnescaped(inner.slice(1), inner[0]);
            const text = end < 0 ? inner : inner.slice(0, end + 2);
            push(text, "string");
            index += text.length;
            continue;
          }
          if (inner[0] === "{") {
            const end = matchBrace(inner);
            const text = inner.slice(0, end);
            push("{", "punctuation");
            const innerSpans = highlightLine(text.slice(1, -1), filetype).spans;
            for (const span of innerSpans) push(span.text, span.token);
            if (text.endsWith("}")) push("}", "punctuation");
            index += text.length;
            continue;
          }
          break;
        }
        continue;
      }
      if (filetype === "html" && rest.startsWith("<!")) {
        const end = rest.indexOf(">");
        const text = end < 0 ? rest : rest.slice(0, end + 1);
        push(text, "comment");
        index += text.length;
        continue;
      }
    }

    // Decorators and attributes.
    if (spec.decorators === "@" && rest[0] === "@") {
      const decorator = /^@[\w.]+/.exec(rest);
      if (decorator) {
        push(decorator[0], "attribute");
        index += decorator[0].length;
        continue;
      }
    }
    if (spec.decorators === "#[" && rest.startsWith("#[")) {
      const end = rest.indexOf("]");
      const text = end < 0 ? rest : rest.slice(0, end + 1);
      push(text, "attribute");
      index += text.length;
      continue;
    }

    // Shell variables.
    if (spec.dollarVariables && rest[0] === "$") {
      const variable = /^\$(?:\{[^}]*\}|[\w@#?*!$-]+)/.exec(rest);
      if (variable) {
        push(variable[0], "property");
        index += variable[0].length;
        continue;
      }
    }

    // Regex literals after an operator or open bracket.
    if (spec.regexLiterals && rest[0] === "/" && !rest.startsWith("//") && !rest.startsWith("/*")) {
      const previous = significant();
      const allowed =
        !previous ||
        previous.token === "operator" ||
        previous.token === "keyword" ||
        (previous.token === "punctuation" && /[([{,;]$/.test(previous.text));
      if (allowed) {
        const regex = /^\/(?:\\.|\[(?:\\.|[^\]\\])*\]|[^/\\\n[])+\/[a-z]*/.exec(rest);
        if (regex) {
          push(regex[0], "regex");
          index += regex[0].length;
          continue;
        }
      }
    }

    // CSS: selectors and properties.
    if (filetype === "css") {
      const atRule = /^@[a-z-]+/.exec(rest);
      if (atRule) {
        push(atRule[0], "keyword");
        index += atRule[0].length;
        continue;
      }
      const color = /^#[0-9a-fA-F]{3,8}\b/.exec(rest);
      if (color) {
        push(color[0], "number");
        index += color[0].length;
        continue;
      }
      // `name:` at the start of a declaration is a property; elsewhere a
      // colon means a pseudo-class, which falls through to the word scanner.
      const property = /^([a-zA-Z-]+)(\s*:)/.exec(rest);
      const before = line.slice(0, index);
      if (property && (/^\s*$/.test(before) || /[{;]\s*$/.test(before))) {
        push(property[1], "property");
        push(property[2], "operator");
        index += property[0].length;
        continue;
      }
      const selector = /^[.#][A-Za-z_][\w-]*/.exec(rest);
      if (selector) {
        push(selector[0], "tag");
        index += selector[0].length;
        continue;
      }
    }

    // Numbers.
    const number = NUMBER.exec(rest);
    if (number && (index === 0 || !/[\w$]/.test(line[index - 1]))) {
      push(number[0], "number");
      index += number[0].length;
      continue;
    }

    // Words.
    const word = WORD.exec(rest);
    if (word) {
      const text = word[0];
      const lookup = spec.caseInsensitiveKeywords ? text.toLowerCase() : text;
      const after = rest.slice(text.length);
      const previous = significant();
      let token: CodeToken = "plain";
      if (spec.constants.has(lookup)) token = "constant";
      else if (spec.keywords.has(lookup)) token = "keyword";
      else if (spec.types.has(lookup)) token = "type";
      else if (
        previous?.token === "keyword" &&
        /^(?:func|fn|def|function|class|struct|enum|interface|type|trait|impl|module|namespace)$/.test(previous.text)
      )
        token = /^(?:class|struct|enum|interface|type|trait|impl|module|namespace)$/.test(previous.text) ? "type" : "function";
      else if (/^\s*\(/.test(after) || /^\s*[<]\s*[A-Za-z].*>\s*\(/.test(after)) token = "function";
      else if (previous?.token === "keyword" && previous.text === "new") token = "type";
      else if (/^[A-Z][A-Z0-9_]{2,}$/.test(text)) token = "constant";
      else if (/^[A-Z][A-Za-z0-9_]*$/.test(text) && text.length > 1) token = "type";
      else if (previous?.text.endsWith(".") && !/^\s*\(/.test(after)) token = "property";
      else if (previous?.token === "operator" && /:$/.test(previous.text) && filetype !== "yaml")
        token = "type";
      else if ((filetype === "yaml" || filetype === "json") && /^\s*:/.test(after)) token = "property";
      else if (filetype === "python" && previous?.token === "keyword" && /^(?:import|from)$/.test(previous.text))
        token = "type";
      push(text, token);
      lastSignificant = spans[spans.length - 1];
      index += text.length;
      continue;
    }

    // Operators and punctuation.
    const operator = OPERATOR.exec(rest);
    if (operator) {
      push(operator[0], "operator");
      index += operator[0].length;
      continue;
    }
    const punctuation = PUNCTUATION.exec(rest);
    if (punctuation) {
      push(punctuation[0], "punctuation");
      index += punctuation[0].length;
      continue;
    }
    const whitespace = /^\s+/.exec(rest);
    if (whitespace) {
      push(whitespace[0], "plain");
      index += whitespace[0].length;
      continue;
    }
    push(rest[0], "plain");
    index++;
  }
  void lastSignificant;

  if (filetype === "json") {
    for (const [position, span] of spans.entries()) {
      const next = spans[position + 1];
      if (span.token === "string" && next?.text.trimStart().startsWith(":")) span.token = "property";
    }
  }
  return { spans, state: mode };
}

function findUnescaped(text: string, close: string): number {
  let cursor = 0;
  while (cursor < text.length) {
    if (text[cursor] === "\\") {
      cursor += 2;
      continue;
    }
    if (text.startsWith(close, cursor)) return cursor;
    cursor++;
  }
  return -1;
}

function matchBrace(text: string): number {
  let depth = 0;
  for (let cursor = 0; cursor < text.length; cursor++) {
    if (text[cursor] === "{") depth++;
    else if (text[cursor] === "}") {
      depth--;
      if (depth === 0) return cursor + 1;
    }
  }
  return text.length;
}

function looksLikeJsx(line: string, index: number): boolean {
  const before = line.slice(0, index).trimEnd();
  const next = line[index + 1] ?? "";
  if (!/[A-Za-z/]/.test(next)) return false;
  // `a < b` is a comparison; `return <div>` or `(<div>` is markup.
  return before === "" || /(?:return|=>|=|\(|,|\?|:|&&|\|\||\{)$/.test(before);
}

function pushTemplate(
  text: string,
  push: (text: string, token: CodeToken) => void,
  spec: LanguageSpec,
): void {
  void spec;
  let cursor = 0;
  while (cursor < text.length) {
    const open = text.indexOf("${", cursor);
    if (open < 0) {
      push(text.slice(cursor), "string");
      return;
    }
    push(text.slice(cursor, open), "string");
    const end = matchBrace(text.slice(open + 1));
    const inner = text.slice(open + 2, open + end);
    push("${", "punctuation");
    for (const span of highlightLine(inner, "typescript").spans) push(span.text, span.token);
    push("}", "punctuation");
    cursor = open + 1 + end;
  }
}

function highlightMarkdownLine(line: string): CodeSpan[] {
  if (/^#{1,6}\s/.test(line)) return [{ text: line, token: "heading" }];
  if (/^\s*(?:[-*+]|\d+[.)])\s/.test(line)) {
    const marker = /^\s*(?:[-*+]|\d+[.)])\s/.exec(line)![0];
    return [{ text: marker, token: "keyword" }, ...highlightMarkdownInline(line.slice(marker.length))];
  }
  if (/^\s*>/.test(line)) return [{ text: line, token: "comment" }];
  if (/^\s*(`{3,}|~{3,})/.test(line)) return [{ text: line, token: "punctuation" }];
  return highlightMarkdownInline(line);
}

function highlightMarkdownInline(text: string): CodeSpan[] {
  const spans: CodeSpan[] = [];
  const pattern = /`[^`]+`|\*\*[^*]+\*\*|\[[^\]]+\]\([^)]+\)/g;
  let cursor = 0;
  for (const match of text.matchAll(pattern)) {
    if (match.index > cursor) spans.push({ text: text.slice(cursor, match.index), token: "plain" });
    const token: CodeToken = match[0].startsWith("`")
      ? "string"
      : match[0].startsWith("**")
        ? "constant"
        : "attribute";
    spans.push({ text: match[0], token });
    cursor = match.index + match[0].length;
  }
  if (cursor < text.length) spans.push({ text: text.slice(cursor), token: "plain" });
  return spans;
}

// ---------------------------------------------------------------------------
// Markdown for agent messages. Block structure plus a small inline grammar.

export type InlineStyle = "plain" | "code" | "bold" | "italic" | "link";

export interface InlineSpan {
  text: string;
  style: InlineStyle;
}

export type MarkdownLineKind =
  | "heading"
  | "bullet"
  | "numbered"
  | "quote"
  | "code"
  | "fence"
  | "rule"
  | "blank"
  | "paragraph";

export interface MarkdownLine {
  kind: MarkdownLineKind;
  spans: InlineSpan[];
  level?: number;
  language?: string;
  marker?: string;
  indent?: number;
}

export function parseInline(text: string): InlineSpan[] {
  const spans: InlineSpan[] = [];
  const push = (value: string, style: InlineStyle) => {
    if (!value) return;
    const last = spans[spans.length - 1];
    if (last && last.style === style) last.text += value;
    else spans.push({ text: value, style });
  };
  let index = 0;
  while (index < text.length) {
    const rest = text.slice(index);
    const code = /^`([^`]+)`/.exec(rest);
    if (code) {
      push(code[1], "code");
      index += code[0].length;
      continue;
    }
    const bold = /^\*\*([^*]+)\*\*|^__([^_]+)__/.exec(rest);
    if (bold) {
      push(bold[1] ?? bold[2], "bold");
      index += bold[0].length;
      continue;
    }
    const italic = /^\*([^*\s][^*]*)\*|^_([^_\s][^_]*)_(?![\w])/.exec(rest);
    if (italic && (index === 0 || !/\w/.test(text[index - 1]))) {
      push(italic[1] ?? italic[2], "italic");
      index += italic[0].length;
      continue;
    }
    const link = /^\[([^\]]+)\]\(([^)]+)\)/.exec(rest);
    if (link) {
      push(link[1], "link");
      index += link[0].length;
      continue;
    }
    const plain = /^[^`*_\[]+|^./.exec(rest)!;
    push(plain[0], "plain");
    index += plain[0].length;
  }
  return spans;
}

export function parseMarkdown(text: string): MarkdownLine[] {
  const lines: MarkdownLine[] = [];
  let fence: { language: string; marker: string } | undefined;
  for (const raw of text.replace(/\r\n/g, "\n").split("\n")) {
    if (fence) {
      if (raw.trim().startsWith(fence.marker)) {
        fence = undefined;
        continue;
      }
      lines.push({
        kind: "code",
        spans: [{ text: raw, style: "plain" }],
        language: fence.language,
      });
      continue;
    }
    const open = /^\s*(`{3,}|~{3,})\s*(\S*)/.exec(raw);
    if (open) {
      fence = { language: filetypeForFence(open[2]), marker: open[1] };
      lines.push({ kind: "fence", spans: [], language: open[2] || undefined });
      continue;
    }
    if (raw.trim() === "") {
      lines.push({ kind: "blank", spans: [] });
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(raw);
    if (heading) {
      lines.push({
        kind: "heading",
        level: heading[1].length,
        spans: parseInline(heading[2].replace(/\s#+$/, "")),
      });
      continue;
    }
    if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(raw)) {
      lines.push({ kind: "rule", spans: [] });
      continue;
    }
    const bullet = /^(\s*)[-*+]\s+(.*)$/.exec(raw);
    if (bullet) {
      lines.push({
        kind: "bullet",
        indent: Math.floor(bullet[1].length / 2),
        spans: parseInline(bullet[2]),
      });
      continue;
    }
    const numbered = /^(\s*)(\d+)[.)]\s+(.*)$/.exec(raw);
    if (numbered) {
      lines.push({
        kind: "numbered",
        indent: Math.floor(numbered[1].length / 2),
        marker: `${numbered[2]}.`,
        spans: parseInline(numbered[3]),
      });
      continue;
    }
    const quote = /^\s*>\s?(.*)$/.exec(raw);
    if (quote) {
      lines.push({ kind: "quote", spans: parseInline(quote[1]) });
      continue;
    }
    lines.push({ kind: "paragraph", spans: parseInline(raw) });
  }
  return lines;
}

// ---------------------------------------------------------------------------
// Context-window meter, palette commands, adaptive polling, sidebar ratio.

export interface ContextMeter {
  ratio: number;
  filled: number;
  total: number;
  label: string;
}

export function contextMeter(
  usage: TokenUsage | undefined,
  cells = 8,
): ContextMeter | undefined {
  if (!usage || !Number.isFinite(usage.contextWindow) || usage.contextWindow! <= 0 ||
      !Number.isFinite(usage.contextTokens) || usage.contextTokens! < 0) return;
  const ratio = Math.max(0, Math.min(1, usage.contextTokens! / usage.contextWindow!));
  const filled = Math.round(ratio * cells);
  return {
    ratio,
    filled,
    total: cells,
    label: `${formatTokenCount(usage.contextTokens!)} · ${Math.round(ratio * 100)}%`,
  };
}

export function renderMeter(meter: ContextMeter): string {
  return `${"▰".repeat(meter.filled)}${"▱".repeat(Math.max(0, meter.total - meter.filled))}`;
}

// Older controllers saved only cumulative totals. Recover the latest context
// snapshot from their private event log without rewriting their state.
export function contextUsageFromEvents(content: string, threadId?: string): Partial<TokenUsage> | undefined {
  const lines = content.split("\n");
  for (let index = lines.length - 1; index >= 0; index--) {
    try {
      const event = JSON.parse(lines[index]);
      if (event?.method !== "thread/tokenUsage/updated") continue;
      const params = event.params;
      if (threadId && params?.threadId && params.threadId !== threadId) continue;
      const usage = params?.tokenUsage;
      const tokens = usage?.last?.totalTokens;
      if (typeof tokens !== "number" || !Number.isFinite(tokens) || tokens < 0) return;
      const window = usage.modelContextWindow ?? usage.contextWindow;
      return {
        contextTokens: tokens,
        ...(typeof window === "number" && Number.isFinite(window) && window > 0 ? { contextWindow: window } : {}),
      };
    } catch {
      // A bounded tail can contain a partial event; use complete records only.
    }
  }
}

export interface PaletteCommand {
  id: string;
  label: string;
  key?: string;
  hint?: string;
  /** Commands that make no sense right now stay listed but disabled. */
  disabled?: string;
}

export function filterPaletteCommands(
  commands: readonly PaletteCommand[],
  query: string,
): PaletteCommand[] {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return [...commands];
  const terms = needle.split(/\s+/);
  const score = (command: PaletteCommand): number => {
    const haystack = `${command.label} ${command.key ?? ""} ${command.hint ?? ""}`.toLocaleLowerCase();
    let total = 0;
    for (const term of terms) {
      if (command.label.toLocaleLowerCase().startsWith(term)) total += 3;
      else if (command.label.toLocaleLowerCase().includes(term)) total += 2;
      else if (haystack.includes(term)) total += 1;
      else return 0;
    }
    return total;
  };
  return commands
    .map((command, index) => ({ command, index, score: score(command) }))
    .filter((entry) => entry.score > 0)
    .sort((a, b) => b.score - a.score || a.index - b.index)
    .map((entry) => entry.command);
}

/**
 * Backs off diff polling while nothing changes and snaps back to the base
 * interval as soon as the diff moves, so an idle repo costs one git spawn
 * every few seconds instead of one per second.
 */
export function nextDiffPollDelay(
  previousDelay: number,
  changed: boolean,
  base = 1_000,
  maximum = 8_000,
): number {
  if (changed) return base;
  return Math.min(maximum, Math.max(base, previousDelay * 2));
}

export function diffTreeWidthForRatio(
  ratio: number | undefined,
  containerWidth: number,
  fallback = 30,
): number {
  const minimum = 20;
  const maximum = Math.max(minimum, Math.min(60, containerWidth - 40));
  const wanted =
    ratio !== undefined && Number.isFinite(ratio) && ratio > 0
      ? Math.round(ratio * containerWidth)
      : fallback;
  return Math.max(minimum, Math.min(maximum, wanted));
}

export function typewriterReveal(
  revealed: number,
  targetLength: number,
  step = 24,
): number {
  if (revealed >= targetLength) return targetLength;
  return Math.min(targetLength, revealed + step);
}

/** Clamps a list scroll offset so the viewport never runs past the end. */
export function clampScrollOffset(
  offset: number,
  itemCount: number,
  visibleItems: number,
): number {
  const maximum = Math.max(0, itemCount - Math.max(1, visibleItems));
  return Math.max(0, Math.min(maximum, Math.round(offset)));
}

// ---------------------------------------------------------------------------
// Session deletion. Only finished or stale runs may be removed, and only when
// the directory really holds that run's state file.

export function sessionIsDeletable(session: Pick<Session, "status">): boolean {
  return isTerminalStatus(session.status) || session.status === "stale";
}

export interface DeleteSessionResult {
  removedStateDir: boolean;
  removedRegistryEntries: number;
}

export async function deleteSessionArtifacts(
  session: Pick<Session, "stateDir" | "status">,
  registryDirs: string[] = defaultRegistryDirectories(),
): Promise<DeleteSessionResult> {
  if (!sessionIsDeletable(session))
    throw new Error(`Session is ${session.status}; stop it before deleting`);
  const stateDir = resolve(session.stateDir);
  const stateFile = join(stateDir, "state.json");
  let persisted: Partial<RunState> | undefined;
  try {
    persisted = JSON.parse(await readFile(stateFile, "utf8")) as Partial<RunState>;
  } catch {
    persisted = undefined;
  }
  const { rm } = await import("node:fs/promises");
  let removedStateDir = false;
  if (persisted?.stateDir && resolve(persisted.stateDir) === stateDir) {
    if (typeof persisted.status !== "string" || !Number.isInteger(persisted.pid))
      throw new Error("Cannot verify the session state; refresh before deleting");
    if (!isTerminalStatus(persisted.status) && defaultProcessAlive(persisted.pid!))
      throw new Error(`Session is now ${persisted.status}; stop it before deleting`);
    await rm(stateDir, { recursive: true, force: true });
    removedStateDir = true;
  }
  let removedRegistryEntries = 0;
  for (const registryDir of registryDirs) {
    let entries;
    try {
      entries = await readdir(registryDir, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      if (!entry.isFile() || !entry.name.endsWith(".run")) continue;
      const entryPath = join(registryDir, entry.name);
      try {
        const target = (await readFile(entryPath, "utf8")).trim();
        if (resolve(target) !== stateDir) continue;
        await rm(entryPath, { force: true });
        removedRegistryEntries++;
      } catch {
        // Unreadable entries are left for the owner to inspect.
      }
    }
  }
  return { removedStateDir, removedRegistryEntries };
}
