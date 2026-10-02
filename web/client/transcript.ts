// Incremental transcript model for the web UI. It folds events.jsonl records
// into chat entries one line at a time, so a live stream only touches the
// entries a new event changes. It keeps no DOM and no I/O, which keeps it
// testable under `bun test`.

/** `stopped`: still running when its turn was interrupted or failed. */
export type EntryStatus = "running" | "completed" | "failed" | "stopped";

export interface FileEdit {
  path: string;
  /** add, update, delete, or move, as the provider reported it. */
  kind: string;
  movePath?: string;
  /** Codex: unified hunks for an update, full content for an add or delete. */
  diff?: string;
  /** Adapter edits carry the old and new text instead of a patch. */
  oldText?: string;
  newText?: string;
  /** Replacement snippets have no known file offset or EOF position. */
  fragment?: boolean;
}

interface EntryBase {
  id: string;
  /** Bumped on every change so the renderer can skip untouched entries. */
  version: number;
  timestampMs?: number;
}

export type Entry = EntryBase &
  (
    | { kind: "user"; text: string }
    | { kind: "agent"; text: string; streaming: boolean; phase?: string }
    | { kind: "thought"; text: string; streaming: boolean }
    | {
        kind: "command";
        command: string;
        cwd?: string;
        status: EntryStatus;
        output: string;
        exitCode?: number;
        durationMs?: number;
      }
    | { kind: "files"; label: string; status: EntryStatus; edits: FileEdit[]; durationMs?: number }
    | { kind: "search"; query: string; status: EntryStatus }
    | {
        kind: "tool";
        name: string;
        label: string;
        status: EntryStatus;
        input?: Record<string, unknown>;
        output?: string;
        durationMs?: number;
      }
    | { kind: "agentRun"; label: string; status: EntryStatus; threadId?: string; output?: string; durationMs?: number }
    | { kind: "turn"; status: string; durationMs?: number }
    | { kind: "notice"; level: "warning" | "error"; text: string }
  );

export interface TranscriptUsage {
  contextTokens?: number;
  contextWindow?: number;
  totalTokens?: number;
}

interface RawItem {
  id?: string;
  type?: string;
  text?: string;
  phase?: string;
  command?: string;
  cwd?: string;
  status?: string;
  aggregatedOutput?: string;
  output?: string;
  exitCode?: number;
  durationMs?: number;
  query?: string;
  toolName?: string;
  input?: Record<string, unknown>;
  changes?: Array<{ path?: string; kind?: { type?: string; move_path?: string | null } | string; diff?: string }>;
  summary?: unknown;
  content?: unknown;
  origin?: string;
  agentThreadId?: string;
  agentPath?: string;
}

interface RawEvent {
  method?: string;
  emittedAtMs?: number;
  params?: {
    threadId?: string;
    itemId?: string;
    delta?: string;
    promptId?: string;
    item?: RawItem;
    turn?: { id?: string; status?: string; durationMs?: number; error?: { message?: string } };
    tokenUsage?: {
      total?: { totalTokens?: number };
      last?: { totalTokens?: number; inputTokens?: number };
      modelContextWindow?: number;
    };
    message?: string;
    error?: { message?: string };
  };
}

const MAX_OUTPUT_CHARS = 200_000;

export class Transcript {
  readonly entries: Entry[] = [];
  usage: TranscriptUsage = {};
  /** Entries changed since the last `takeDirty`. */
  private dirty = new Set<string>();
  private structureChanged = false;
  private byId = new Map<string, Entry>();
  private rootThreads = new Set<string>();
  private rejectedPrompts = new Set<string>();
  private subAgentMessages = new Map<string, string>();
  private pendingEcho?: { id: string; text: string };
  private completedItems = new Set<string>();
  private sequence = 0;

  constructor(rootThreadId?: string) {
    if (rootThreadId) this.rootThreads.add(rootThreadId);
  }

  /** Applies a chunk of JSONL text. Partial or malformed lines are skipped. */
  applyText(text: string): void {
    for (const line of text.split("\n")) {
      if (!line.trim()) continue;
      let event: RawEvent;
      try {
        event = JSON.parse(line) as RawEvent;
      } catch {
        // A bounded tail may begin in the middle of a JSONL record.
        continue;
      }
      this.apply(event);
    }
  }

  takeDirty(): { ids: Set<string>; structure: boolean } {
    const result = { ids: this.dirty, structure: this.structureChanged };
    this.dirty = new Set();
    this.structureChanged = false;
    return result;
  }

  apply(event: RawEvent): void {
    const method = event.method ?? "";
    const params = event.params ?? {};
    const at = event.emittedAtMs;
    if (method === "ruddr/prompt/rejected" && params.promptId) {
      this.rejectedPrompts.add(params.promptId);
      this.remove(params.promptId);
      if (this.pendingEcho?.id === params.promptId) this.pendingEcho = undefined;
      return;
    }
    const item = params.item;
    const threadId = params.threadId;
    if (this.rootThreads.size === 0 && item?.type === "userMessage" && item.origin === "ruddr" && threadId)
      this.rootThreads.add(threadId);
    const foreign = threadId !== undefined && this.rootThreads.size > 0 && !this.rootThreads.has(threadId);
    if (foreign) {
      if (item?.type === "agentMessage" && method === "item/completed" && item.text) this.subAgentMessages.set(threadId!, item.text);
      if (method === "turn/completed") this.finishSubAgent(threadId!, params.turn);
      return;
    }
    if (method === "thread/tokenUsage/updated" && params.tokenUsage) {
      const usage = params.tokenUsage;
      this.usage = {
        totalTokens: usage.total?.totalTokens ?? this.usage.totalTokens,
        contextTokens: usage.last?.totalTokens ?? usage.last?.inputTokens ?? this.usage.contextTokens,
        contextWindow: usage.modelContextWindow ?? this.usage.contextWindow,
      };
      return;
    }
    const itemId = item?.id ?? params.itemId;
    if (itemId && this.completedItems.has(itemId) && method !== "item/completed") return;
    if (itemId && method === "item/completed") this.completedItems.add(itemId);
    if (method === "turn/completed") {
      if (foreign) {
        this.finishSubAgent(threadId!, params.turn);
        return;
      }
      const turn = params.turn ?? {};
      this.pendingEcho = undefined;
      this.finishStreaming(turn.status ?? "completed");
      this.push({
        id: `turn:${turn.id ?? this.sequence}`,
        kind: "turn",
        status: turn.status ?? "completed",
        durationMs: turn.durationMs,
        version: 0,
        timestampMs: at,
      });
      if (turn.error?.message)
        this.push({ id: `turn-error:${turn.id ?? this.sequence}`, kind: "notice", level: "error", text: turn.error.message, version: 0 });
      return;
    }
    if (method === "error" && !foreign) {
      const message = params.error?.message ?? params.message;
      if (message) this.push({ id: `error:${this.sequence}`, kind: "notice", level: "error", text: message, version: 0, timestampMs: at });
      return;
    }
    if (!method.startsWith("item/") || foreign) return;

    if (method === "item/agentMessage/delta") {
      const id = params.itemId;
      if (!id || !params.delta) return;
      const entry = this.byId.get(id);
      if (entry?.kind === "agent") {
        entry.text += params.delta;
        this.touch(entry);
      } else this.push({ id, kind: "agent", text: params.delta, streaming: true, version: 0, timestampMs: at });
      return;
    }
    if (method === "item/reasoning/summaryTextDelta" || method === "item/reasoning/textDelta") {
      const id = params.itemId;
      if (!id || !params.delta) return;
      const key = `thought:${id}`;
      const entry = this.byId.get(key);
      if (entry?.kind === "thought") {
        entry.text += params.delta;
        this.touch(entry);
      } else this.push({ id: key, kind: "thought", text: params.delta, streaming: true, version: 0, timestampMs: at });
      return;
    }
    if (method === "item/reasoning/summaryPartAdded") {
      const entry = params.itemId ? this.byId.get(`thought:${params.itemId}`) : undefined;
      if (entry?.kind === "thought" && entry.text && !entry.text.endsWith("\n\n")) {
        entry.text += "\n\n";
        this.touch(entry);
      }
      return;
    }
    if (method === "item/commandExecution/outputDelta") {
      const entry = params.itemId ? this.byId.get(params.itemId) : undefined;
      if (entry?.kind === "command" && params.delta) {
        entry.output = clip(entry.output + params.delta);
        this.touch(entry);
      }
      return;
    }
    if (!item?.type) return;
    const id = item.id;
    const status = itemStatus(method, item);

    switch (item.type) {
      case "userMessage": {
        const text = (item.text ?? flatten(item.content)).trim();
        if (method !== "item/completed" || !text) return;
        if (id && this.rejectedPrompts.has(id)) return;
        // Consume one provider echo of a Ruddr prompt, even if other items intervened.
        if (item.origin !== "ruddr" && this.pendingEcho?.text === text) {
          this.pendingEcho = undefined;
          return;
        }
        this.pendingEcho = item.origin === "ruddr" ? { id: id ?? "", text } : undefined;
        this.push({ id: id ?? `user:${this.sequence}`, kind: "user", text, version: 0, timestampMs: at });
        return;
      }
      case "agentMessage": {
        const key = id ?? `agent:${this.sequence}`;
        const entry = this.byId.get(key);
        const text = item.text ?? "";
        if (entry?.kind === "agent") {
          if (item.text !== undefined && (method === "item/completed" || text)) entry.text = text;
          entry.streaming = method !== "item/completed";
          entry.phase = item.phase ?? entry.phase;
          this.touch(entry);
        } else if (text.trim())
          this.push({ id: key, kind: "agent", text, streaming: method !== "item/completed", phase: item.phase, version: 0, timestampMs: at });
        return;
      }
      case "reasoning": {
        const key = `thought:${id ?? this.sequence}`;
        const summary = flatten(item.summary).trim();
        const entry = this.byId.get(key);
        if (entry?.kind === "thought") {
          if (method === "item/completed") {
            if (summary) entry.text = summary;
            entry.streaming = false;
            this.touch(entry);
          }
        } else if (method === "item/completed" && summary)
          this.push({ id: key, kind: "thought", text: summary, streaming: false, version: 0, timestampMs: at });
        return;
      }
      case "commandExecution": {
        if (!id) return;
        const entry = this.byId.get(id);
        if (entry?.kind === "command") {
          entry.status = status;
          entry.command = item.command ? displayCommand(item.command) : entry.command;
          entry.cwd = item.cwd ?? entry.cwd;
          if (item.aggregatedOutput !== undefined && item.aggregatedOutput !== null)
            entry.output = clip(item.aggregatedOutput);
          entry.exitCode = item.exitCode ?? entry.exitCode;
          entry.durationMs = item.durationMs ?? entry.durationMs;
          this.touch(entry);
        } else
          this.push({
            id,
            kind: "command",
            command: displayCommand(item.command ?? "command"),
            cwd: item.cwd,
            status,
            output: clip(item.aggregatedOutput ?? ""),
            exitCode: item.exitCode,
            durationMs: item.durationMs,
            version: 0,
            timestampMs: at,
          });
        return;
      }
      case "fileChange": {
        if (!id) return;
        const edits = fileEditsFromItem(item);
        const entry = this.byId.get(id);
        if (entry?.kind === "files") {
          entry.status = status;
          if (edits.length) entry.edits = edits;
          entry.durationMs = item.durationMs ?? entry.durationMs;
          this.touch(entry);
        } else
          this.push({
            id,
            kind: "files",
            label: item.command ?? "file changes",
            status,
            edits,
            durationMs: item.durationMs,
            version: 0,
            timestampMs: at,
          });
        return;
      }
      case "webSearch": {
        if (!id) return;
        const entry = this.byId.get(id);
        const query = item.query ?? item.command ?? "web search";
        if (entry?.kind === "search") {
          entry.status = status;
          entry.query = item.query ?? entry.query;
          this.touch(entry);
        } else this.push({ id, kind: "search", query, status, version: 0, timestampMs: at });
        return;
      }
      case "toolCall": {
        if (!id) return;
        const edits = fileEditsFromItem(item);
        if (edits.some((edit) => edit.diff !== undefined || edit.oldText !== undefined || edit.newText !== undefined)) {
          // Pi and OpenCode keep edit tools as toolCall items.
          this.apply({ ...event, params: { ...params, item: { ...item, type: "fileChange" } } });
          return;
        }
        const entry = this.byId.get(id);
        if (entry?.kind === "tool") {
          entry.status = status;
          entry.input = item.input ?? entry.input;
          entry.output = item.aggregatedOutput ?? item.output ?? entry.output;
          entry.durationMs = item.durationMs ?? entry.durationMs;
          this.touch(entry);
        } else
          this.push({
            id,
            kind: "tool",
            name: item.toolName ?? "tool",
            label: item.command ?? item.toolName ?? "tool",
            status,
            input: item.input,
            output: item.aggregatedOutput ?? item.output,
            durationMs: item.durationMs,
            version: 0,
            timestampMs: at,
          });
        return;
      }
      case "subAgentActivity": {
        if (!id) return;
        const entry = this.byId.get(id);
        if (entry?.kind === "agentRun") {
          entry.threadId = item.agentThreadId ?? entry.threadId;
          if (entry.status === "running") entry.status = status;
          this.touch(entry);
        } else
          this.push({
            id,
            kind: "agentRun",
            label: item.agentPath ?? item.command ?? "sub-agent",
            status,
            threadId: item.agentThreadId,
            version: 0,
            timestampMs: at,
          });
        return;
      }
    }
  }

  private finishSubAgent(threadId: string, turn: { status?: string; durationMs?: number } | undefined): void {
    for (const entry of this.entries) {
      if (entry.kind !== "agentRun" || entry.threadId !== threadId) continue;
      entry.status = turn?.status === "completed" || !turn?.status ? "completed" : "failed";
      entry.durationMs = turn?.durationMs ?? entry.durationMs;
      entry.output = this.subAgentMessages.get(threadId) ?? entry.output;
      this.touch(entry);
    }
  }

  private finishStreaming(turnStatus = "completed"): void {
    for (const entry of this.entries) {
      if ((entry.kind === "agent" || entry.kind === "thought") && entry.streaming) {
        entry.streaming = false;
        this.completedItems.add(entry.kind === "thought" ? entry.id.slice("thought:".length) : entry.id);
        this.touch(entry);
      }
      if ((entry.kind === "command" || entry.kind === "files" || entry.kind === "tool" || entry.kind === "search") && entry.status === "running") {
        // A finished turn cannot leave a tool running; its item event was lost
        // to the tail window or the provider never sent one.
        // A clean turn implies the tool finished; otherwise nobody knows.
        entry.status = turnStatus === "completed" ? "completed" : "stopped";
        this.touch(entry);
      }
    }
  }

  private lastOf(kind: Entry["kind"]): Entry | undefined {
    for (let index = this.entries.length - 1; index >= 0; index--) {
      const entry = this.entries[index];
      if (entry.kind === kind) return entry;
      if (entry.kind !== "thought") return undefined;
    }
    return undefined;
  }

  private push(entry: Entry): void {
    this.sequence++;
    const existing = this.byId.get(entry.id);
    if (existing) {
      Object.assign(existing, entry, { version: existing.version + 1 });
      this.dirty.add(existing.id);
      return;
    }
    this.byId.set(entry.id, entry);
    this.entries.push(entry);
    this.dirty.add(entry.id);
    this.structureChanged = true;
  }

  private remove(id: string): void {
    const entry = this.byId.get(id);
    if (!entry) return;
    this.byId.delete(id);
    this.entries.splice(this.entries.indexOf(entry), 1);
    this.structureChanged = true;
  }

  private touch(entry: Entry): void {
    entry.version++;
    this.dirty.add(entry.id);
  }
}

function itemStatus(method: string, item: RawItem): EntryStatus {
  if (item.status === "failed" || item.status === "declined" || (item.exitCode !== undefined && item.exitCode !== null && item.exitCode !== 0))
    return "failed";
  if (method === "item/completed") return "completed";
  if (item.status === "completed") return "completed";
  return "running";
}

/** Strips the `zsh -lc '…'` wrapper Codex puts around shell commands. */
export function displayCommand(command: string): string {
  const shell = /^(?:\/bin\/|\/usr\/bin\/)?(?:zsh|bash|sh) -lc ([\s\S]+)$/.exec(command.trim());
  if (!shell) return command;
  const body = shell[1];
  const quote = body[0];
  if ((quote === "'" || quote === '"') && body.endsWith(quote) && body.length > 1) {
    const inner = body.slice(1, -1);
    return quote === "'" ? inner.replace(/'\\''/g, "'") : inner.replace(/\\(["\\$`])/g, "$1");
  }
  return body;
}

function clip(text: string): string {
  return text.length > MAX_OUTPUT_CHARS ? text.slice(-MAX_OUTPUT_CHARS) : text;
}

export function flatten(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value))
    return value
      .map((part) => flatten(part))
      .filter(Boolean)
      .join("\n\n");
  if (value && typeof value === "object") {
    const record = value as Record<string, unknown>;
    return flatten(record.text ?? record.content ?? "");
  }
  return "";
}

function stringField(input: Record<string, unknown>, ...keys: string[]): string | undefined {
  for (const key of keys) {
    const value = input[key];
    if (typeof value === "string") return value;
  }
  return undefined;
}

/**
 * Normalizes every provider's file-change shape. Codex sends `changes` with
 * per-file patches. The Claude, Droid, OpenCode, and Pi adapters send the
 * edit tool's input, which holds the old and new text.
 */
export function fileEditsFromItem(item: RawItem): FileEdit[] {
  if (Array.isArray(item.changes) && item.changes.length) {
    return item.changes.flatMap((change) => {
      if (!change.path) return [];
      const kind = typeof change.kind === "string" ? change.kind : change.kind?.type ?? "update";
      const movePath = typeof change.kind === "object" ? change.kind?.move_path ?? undefined : undefined;
      return [{ path: change.path, kind, movePath: movePath || undefined, diff: change.diff }];
    });
  }
  const input = item.input;
  if (!input) return [];
  const path = stringField(input, "file_path", "filePath", "path", "notebook_path", "target_file");
  const patch = stringField(input, "patch", "diff", "input");
  if (patch && /^(\*\*\* Begin Patch|diff --git|--- |@@ )/m.test(patch)) return editsFromPatchText(patch, path);
  if (!path) return [];
  const edits = Array.isArray(input.edits) ? (input.edits as Array<Record<string, unknown>>) : undefined;
  if (edits?.length) {
    return edits.map((edit) => ({
      path,
      kind: "update",
      fragment: true,
      oldText: stringField(edit, "old_string", "oldString", "old_str", "oldText") ?? "",
      newText: stringField(edit, "new_string", "newString", "new_str", "newText") ?? "",
    }));
  }
  const content = stringField(input, "content", "file_text", "text", "new_source");
  const oldText = stringField(input, "old_string", "oldString", "old_str", "oldText");
  const newText = stringField(input, "new_string", "newString", "new_str", "newText");
  if (oldText !== undefined || newText !== undefined)
    return [{ path, kind: "update", fragment: true, oldText: oldText ?? "", newText: newText ?? "" }];
  // TODO(review): Distinguish file creation from overwrite when a Write input omits the previous content.
  if (content !== undefined) return [{ path, kind: "add", oldText: "", newText: content }];
  return [{ path, kind: "update" }];
}

/** Splits an apply_patch envelope or a git patch into per-file edits. */
export function editsFromPatchText(patch: string, fallbackPath?: string): FileEdit[] {
  if (patch.includes("*** Begin Patch")) {
    const edits: FileEdit[] = [];
    let current: FileEdit | undefined;
    let lines: string[] = [];
    const flush = () => {
      if (current) {
        current.diff = current.kind === "update" ? lines.join("\n") : lines.map((line) => line.replace(/^[+-]/, "")).join("\n");
        edits.push(current);
      }
      lines = [];
    };
    for (const line of patch.split("\n")) {
      const header = /^\*\*\* (Add|Update|Delete) File: (.+)$/.exec(line);
      if (header) {
        flush();
        current = { path: header[2].trim(), kind: header[1].toLowerCase() };
        continue;
      }
      const move = /^\*\*\* Move to: (.+)$/.exec(line);
      if (move && current) {
        current.movePath = move[1].trim();
        continue;
      }
      if (line.startsWith("*** End Patch") || line.startsWith("*** Begin Patch") || line.startsWith("*** End of File")) continue;
      // TODO(review): Render unnumbered apply_patch hunks as fragments without inventing source line numbers.
      if (current) lines.push(line === "@@" ? "@@ -1 +1 @@" : line);
    }
    flush();
    return edits;
  }
  const files = patch.split(/^(?=diff --git )/m).filter((part) => part.trim());
  return files.map((part) => {
    const path = /^\+\+\+ (?:b\/)?(.+)$/m.exec(part)?.[1] ?? /^diff --git a\/\S+ b\/(.+)$/m.exec(part)?.[1] ?? fallbackPath ?? "patch";
    return { path, kind: "update", diff: part };
  });
}

/** Makes a full unified patch for one Codex-style edit so a diff viewer can parse it. */
export function unifiedPatchForEdit(edit: FileEdit): string | undefined {
  if (!edit.diff) return undefined;
  if (/^diff --git |^--- /m.test(edit.diff)) return edit.diff.endsWith("\n") ? edit.diff : `${edit.diff}\n`;
  if (edit.kind !== "update" && edit.kind !== "move") return undefined;
  const target = edit.movePath ?? edit.path;
  const body = edit.diff.endsWith("\n") ? edit.diff : `${edit.diff}\n`;
  return `--- a/${edit.path}\n+++ b/${target}\n${body}`;
}

/** Counts added and removed lines for the chat row summary. */
export function editStats(edit: FileEdit): { additions: number; deletions: number } {
  if (edit.oldText !== undefined || edit.newText !== undefined) {
    const count = (text: string | undefined) => (text ? text.replace(/\n$/, "").split("\n").length : 0);
    return { additions: count(edit.newText), deletions: count(edit.oldText) };
  }
  if (!edit.diff) return { additions: 0, deletions: 0 };
  if (edit.kind === "add") return { additions: edit.diff.replace(/\n$/, "").split("\n").length, deletions: 0 };
  if (edit.kind === "delete") return { additions: 0, deletions: edit.diff.replace(/\n$/, "").split("\n").length };
  let additions = 0;
  let deletions = 0;
  for (const line of edit.diff.split("\n")) {
    if (line.startsWith("+") && !line.startsWith("+++")) additions++;
    else if (line.startsWith("-") && !line.startsWith("---")) deletions++;
  }
  return { additions, deletions };
}
