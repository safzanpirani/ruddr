// The Chat pane: a keyed, incremental renderer over the Transcript model.
// Only entries whose version changed are rebuilt, so a streaming message
// costs one element update per animation frame.
import { ansiNodes } from "./ansi";
import { renderEdit, type DiffPreferences } from "./diffs";
import { clear, copyText, h } from "./dom";
import { formatDuration } from "./format";
import { escapeHTML, renderMarkdown } from "./markdown";
import { editStats, type Entry, type FileEdit, Transcript } from "./transcript";

interface Rendered {
  element: HTMLElement;
  version: number;
  cleanups: Array<() => void>;
  /** Survives rebuilds so a streaming row keeps the user's open state. */
  open?: boolean;
}

const INLINE_DIFF_LINE_LIMIT = 160;
const LIVE_OUTPUT_LINES = 8;

export interface ChatCallbacks {
  openFileInDiff(path: string): void;
  preferences(): DiffPreferences;
  toast(message: string, kind?: "info" | "success" | "warning" | "error"): void;
}

export function codeBlock(language: string, code: string): string {
  return `<div class="md-code"><div class="md-code-head"><span>${escapeHTML(language || "text")}</span><button class="md-copy" type="button" data-copy>copy</button></div><pre><code>${escapeHTML(code)}</code></pre></div>`;
}

export function markdownHTML(text: string): string {
  return renderMarkdown(text, { codeBlock });
}

/** Delegated copy buttons inside rendered markdown. */
export function wireCopyButtons(root: HTMLElement): void {
  root.addEventListener("click", (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>("[data-copy]");
    if (!button) return;
    const code = button.closest(".md-code")?.querySelector("code")?.textContent ?? "";
    void copyText(code).then((ok) => {
      button.textContent = ok ? "copied" : "failed";
      setTimeout(() => (button.textContent = "copy"), 1200);
    });
  });
}

function statusIcon(status: string): HTMLElement {
  const glyph = status === "running" ? "" : status === "failed" ? "✗" : status === "stopped" ? "■" : "✓";
  return h("span", { class: `status-icon ${status}` }, glyph);
}

function tail(text: string, lines: number): string {
  const all = text.replace(/\n$/, "").split("\n");
  return all.slice(-lines).join("\n");
}

export class ChatView {
  readonly element: HTMLElement;
  readonly scroller: HTMLElement;
  private list: HTMLElement;
  private livePill: HTMLButtonElement;
  private transcript = new Transcript();
  private rendered = new Map<string, Rendered>();
  private animateNew = false;
  private folds = new Map<string, HTMLButtonElement>();
  private openFolds = new Set<string>();
  private openState = new Map<string, boolean>();
  private frame = 0;
  private following = true;
  private empty: HTMLElement;
  private truncated = false;

  constructor(private readonly callbacks: ChatCallbacks) {
    this.list = h("div", { class: "chat-list" });
    this.empty = h("div", { class: "empty-state chat-empty" }, h("div", { class: "empty-glyph" }, "◌"), "Waiting for the first event…");
    this.scroller = h("div", { class: "chat-scroller" }, this.empty, this.list);
    this.livePill = h("button", { class: "live-pill hidden", onclick: () => this.follow() }, "↓ Follow live");
    this.element = h("div", { class: "chat-view" }, this.scroller, this.livePill);
    wireCopyButtons(this.list);
    this.scroller.addEventListener(
      "scroll",
      () => {
        const atBottom = this.scroller.scrollHeight - this.scroller.scrollTop - this.scroller.clientHeight < 48;
        this.following = atBottom;
        this.livePill.classList.toggle("hidden", atBottom);
      },
      { passive: true },
    );
  }

  get usage() {
    return this.transcript.usage;
  }

  get entries(): readonly Entry[] {
    return this.transcript.entries;
  }

  isFollowing(): boolean {
    return this.following;
  }

  reset(rootThreadId: string | undefined, text: string, truncated: boolean): void {
    for (const rendered of this.rendered.values()) for (const cleanup of rendered.cleanups) cleanup();
    this.rendered.clear();
    this.openState.clear();
    this.animateNew = false;
    this.folds.clear();
    this.openFolds.clear();
    clear(this.list);
    this.transcript = new Transcript(rootThreadId);
    this.truncated = truncated;
    this.following = true;
    this.livePill.classList.add("hidden");
    this.append(text);
  }

  append(text: string): void {
    this.transcript.applyText(text);
    this.schedule();
  }

  follow(): void {
    this.following = true;
    this.livePill.classList.add("hidden");
    this.scroller.scrollTo({ top: this.scroller.scrollHeight, behavior: "smooth" });
  }

  private schedule(): void {
    if (this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.render();
    });
  }

  rerenderAll(): void {
    for (const rendered of this.rendered.values()) rendered.version = -1;
    this.render();
  }

  private render(): void {
    const wasFollowing = this.following;
    const { ids } = this.transcript.takeDirty();
    const entries = this.transcript.entries;
    const live = new Set(entries.map((entry) => entry.id));
    for (const [id, rendered] of this.rendered) {
      if (live.has(id)) continue;
      for (const cleanup of rendered.cleanups) cleanup();
      rendered.element.remove();
      this.rendered.delete(id);
      this.openState.delete(id);
    }
    let previous: Element | null = this.truncated ? this.ensureTruncatedNotice() : null;
    for (const entry of entries) {
      let rendered = this.rendered.get(entry.id);
      if (!rendered || rendered.version !== entry.version || ids.has(entry.id)) {
        const replacement = this.build(entry);
        // Only entries that arrive live slide in; rebuilds and history do not.
        if (!rendered && this.animateNew) replacement.element.classList.add("fresh");
        if (rendered) {
          for (const cleanup of rendered.cleanups) cleanup();
          rendered.element.replaceWith(replacement.element);
        }
        rendered = replacement;
        rendered.version = entry.version;
        this.rendered.set(entry.id, rendered);
      }
      let expected = previous ? previous.nextElementSibling : this.list.firstElementChild;
      while (expected?.classList.contains("fold")) expected = expected.nextElementSibling;
      if (expected !== rendered.element) {
        if (previous) previous.after(rendered.element);
        else this.list.prepend(rendered.element);
      }
      previous = rendered.element;
    }
    this.foldRuns(entries);
    this.empty.classList.toggle("hidden", entries.length > 0);
    if (wasFollowing) this.scroller.scrollTop = this.scroller.scrollHeight;
    this.animateNew = true;
  }

  /**
   * Collapses each finished run of three or more commands, searches, tool
   * calls, or thoughts into one summary row. File edits and messages stay
   * visible, and so does the run at the end of the transcript, which is
   * the work happening now.
   */
  private foldRuns(entries: readonly Entry[]): void {
    const foldable = (entry: Entry) =>
      ((entry.kind === "command" || entry.kind === "search" || entry.kind === "tool") && entry.status === "completed") ||
      (entry.kind === "thought" && !entry.streaming);
    const runs: Entry[][] = [];
    let run: Entry[] = [];
    for (const entry of entries) {
      if (foldable(entry)) {
        run.push(entry);
        continue;
      }
      if (run.length >= 3) runs.push(run);
      run = [];
    }
    const wanted = new Set<string>();
    for (const members of runs) {
      const key = members[0].id;
      wanted.add(key);
      const open = this.openFolds.has(key);
      let fold = this.folds.get(key);
      if (!fold) {
        fold = h("button", { class: "fold", type: "button" });
        fold.addEventListener("click", () => {
          if (this.openFolds.has(key)) this.openFolds.delete(key);
          else this.openFolds.add(key);
          this.foldRuns(this.transcript.entries);
        });
        this.folds.set(key, fold);
      }
      const counts = new Map<string, number>();
      let duration = 0;
      for (const entry of members) {
        const noun = entry.kind === "command" ? "command" : entry.kind === "search" ? "search" : entry.kind === "thought" ? "thought" : "tool call";
        counts.set(noun, (counts.get(noun) ?? 0) + 1);
        if ("durationMs" in entry && entry.durationMs) duration += entry.durationMs;
      }
      const summary = [...counts].map(([noun, count]) => `${count} ${noun}${count === 1 ? "" : noun === "search" ? "es" : "s"}`).join(" · ");
      const label = `${open ? "▾" : "▸"} ${summary}${duration ? ` · ${formatDuration(duration)}` : ""}`;
      if (fold.textContent !== label) fold.textContent = label;
      fold.classList.toggle("open", open);
      const first = this.rendered.get(key)?.element;
      if (first && fold.nextElementSibling !== first) first.before(fold);
      for (const entry of members) this.rendered.get(entry.id)?.element.classList.toggle("folded", !open);
    }
    for (const [key, fold] of this.folds) {
      if (wanted.has(key)) continue;
      fold.remove();
      this.folds.delete(key);
    }
    // Entries that left a run (a fold reopened by new content) must show again.
    for (const element of this.list.querySelectorAll(".folded")) {
      const id = (element as HTMLElement).dataset.entry;
      if (!id || ![...runs].some((members) => !this.openFolds.has(members[0].id) && members.some((entry) => entry.id === id)))
        element.classList.remove("folded");
    }
  }

  private ensureTruncatedNotice(): Element {
    let notice = this.list.querySelector(".truncated-notice");
    if (!notice) {
      notice = h("div", { class: "truncated-notice" }, "Earlier events are outside the loaded window.");
      this.list.prepend(notice);
    }
    return notice;
  }

  private build(entry: Entry): Rendered {
    const cleanups: Array<() => void> = [];
    const result = (element: HTMLElement): Rendered => {
      element.dataset.entry = entry.id;
      element.dataset.search = searchText(entry);
      return { element, version: entry.version, cleanups };
    };
    switch (entry.kind) {
      case "user": {
        // Long briefs fold to a dozen lines; the toggle reveals the rest.
        const long = entry.text.split("\n").length > 12 || entry.text.length > 1200;
        const bubble = h("div", { class: `bubble md${long && !this.isOpen(entry.id, false) ? " clamped" : ""}`, html: markdownHTML(entry.text) });
        const toggle = long
          ? h("button", { class: "bubble-more", type: "button" }, this.isOpen(entry.id, false) ? "show less" : "show more")
          : null;
        toggle?.addEventListener("click", () => {
          const open = !this.isOpen(entry.id, false);
          this.openState.set(entry.id, open);
          bubble.classList.toggle("clamped", !open);
          toggle.textContent = open ? "show less" : "show more";
        });
        return result(h("div", { class: "msg user" }, h("div", { class: "user-col" }, bubble, toggle)));
      }
      case "agent": {
        const body = h("div", { class: "md", html: markdownHTML(entry.text) });
        if (entry.streaming) body.append(h("span", { class: "caret" }));
        return result(
          h(
            "div",
            { class: `msg agent${entry.phase === "commentary" ? " commentary" : ""}${entry.streaming ? " streaming" : ""}` },
            h("div", { class: "agent-mark" }, "◆"),
            body,
          ),
        );
      }
      case "thought": {
        const firstLine = entry.text.trim().split("\n")[0].replace(/\*\*/g, "");
        return result(
          this.collapsible(entry.id, false, `thought${entry.streaming ? " streaming" : ""}`, [
            h("span", { class: "thought-mark" }, entry.streaming ? "✻" : "∴"),
            h("span", { class: "row-title thought-title" }, firstLine || "Thinking"),
          ], () => h("div", { class: "md thought-body", html: markdownHTML(entry.text) })),
        );
      }
      case "command": {
        const meta = [
          entry.exitCode !== undefined && entry.exitCode !== 0 ? `exit ${entry.exitCode}` : "",
          formatDuration(entry.durationMs),
        ].filter(Boolean);
        const running = entry.status === "running";
        const element = this.collapsible(
          entry.id,
          false,
          `tool command ${entry.status}`,
          [
            statusIcon(entry.status),
            h("span", { class: "tool-kind" }, "$"),
            h("span", { class: "row-title mono" }, entry.command),
            meta.length ? h("span", { class: "row-meta" }, meta.join(" · ")) : null,
          ],
          () =>
            h(
              "div",
              { class: "tool-detail" },
              entry.cwd ? h("div", { class: "kv" }, h("span", null, "cwd"), h("code", null, entry.cwd)) : null,
              entry.output ? h("pre", { class: "term" }, ...ansiNodes(entry.output)) : h("div", { class: "dim small" }, "No output."),
            ),
        );
        if (running && entry.output && !this.isOpen(entry.id, false))
          element.append(h("pre", { class: "term live-tail" }, ...ansiNodes(tail(entry.output, LIVE_OUTPUT_LINES))));
        return result(element);
      }
      case "files": {
        const totals = entry.edits.reduce(
          (sum, edit) => {
            const stats = editStats(edit);
            return { additions: sum.additions + stats.additions, deletions: sum.deletions + stats.deletions };
          },
          { additions: 0, deletions: 0 },
        );
        const small = totals.additions + totals.deletions <= INLINE_DIFF_LINE_LIMIT;
        const title =
          entry.edits.length === 0
            ? entry.label
            : entry.edits.length === 1
              ? displayPath(entry.edits[0].path)
              : `${entry.edits.length} files`;
        return result(
          this.collapsible(
            entry.id,
            small && entry.edits.length > 0,
            `tool files ${entry.status}`,
            [
              statusIcon(entry.status),
              h("span", { class: "tool-kind" }, verbFor(entry.edits)),
              h("span", { class: "row-title mono" }, title),
              entry.edits.every((edit) => edit.kind === "write") ? null : h("span", { class: "row-meta" }, h("span", { class: "add" }, `+${totals.additions}`), " ", h("span", { class: "del" }, `−${totals.deletions}`)),
            ],
            () => {
              const box = h("div", { class: "edits" });
              entry.edits.forEach((edit, index) => box.append(this.editCard(entry.id, edit, index, cleanups)));
              if (!entry.edits.length) box.append(h("div", { class: "dim small" }, "The provider did not include the patch."));
              return box;
            },
          ),
        );
      }
      case "search":
        return result(
          h(
            "div",
            { class: `tool row search ${entry.status}` },
            h("div", { class: "row-head static" }, statusIcon(entry.status), h("span", { class: "tool-kind" }, "web"), h("span", { class: "row-title" }, entry.query)),
          ),
        );
      case "tool":
        return result(
          this.collapsible(
            entry.id,
            false,
            `tool call ${entry.status}`,
            [
              statusIcon(entry.status),
              h("span", { class: "tool-kind" }, entry.name),
              h("span", { class: "row-title mono" }, entry.label.startsWith(entry.name) ? entry.label.slice(entry.name.length).trim() || entry.name : entry.label),
              entry.durationMs !== undefined ? h("span", { class: "row-meta" }, formatDuration(entry.durationMs)) : null,
            ],
            () =>
              h(
                "div",
                { class: "tool-detail" },
                entry.input && Object.keys(entry.input).length ? h("pre", { class: "json" }, JSON.stringify(entry.input, null, 2)) : null,
                entry.output ? h("pre", { class: "term" }, ...ansiNodes(entry.output)) : null,
              ),
          ),
        );
      case "agentRun":
        return result(
          this.collapsible(
            entry.id,
            false,
            `tool subagent ${entry.status}`,
            [
              statusIcon(entry.status),
              h("span", { class: "tool-kind" }, "agent"),
              h("span", { class: "row-title" }, entry.label),
              entry.durationMs !== undefined ? h("span", { class: "row-meta" }, formatDuration(entry.durationMs)) : null,
            ],
            () => h("div", { class: "tool-detail md", html: entry.output ? markdownHTML(entry.output) : "<p class=\"dim\">No result yet.</p>" }),
          ),
        );
      case "turn":
        return result(
          h(
            "div",
            { class: `turn-divider ${entry.status}` },
            h("span", null, `turn ${entry.status}${entry.durationMs !== undefined ? ` · ${formatDuration(entry.durationMs)}` : ""}`),
          ),
        );
      case "notice":
        return result(h("div", { class: `notice ${entry.level}` }, entry.level === "error" ? "× " : "! ", entry.text));
    }
  }

  private isOpen(id: string, fallback: boolean): boolean {
    return this.openState.get(id) ?? fallback;
  }

  /** A row whose body is built only when opened and kept across rebuilds. */
  private collapsible(id: string, defaultOpen: boolean, className: string, head: Array<Node | null>, body: () => HTMLElement): HTMLElement {
    const open = this.isOpen(id, defaultOpen);
    const chevron = h("span", { class: "chevron" }, "›");
    const header = h("button", { class: "row-head", type: "button", "aria-expanded": String(open) }, chevron, ...head);
    const element = h("div", { class: `row ${className}${open ? " open" : ""}` }, header);
    let content: HTMLElement | undefined;
    const ensureBody = () => {
      if (content) return;
      content = h("div", { class: "row-body" }, body());
      element.append(content);
    };
    if (open) ensureBody();
    header.addEventListener("click", () => {
      const next = !element.classList.contains("open");
      this.openState.set(id, next);
      if (next) {
        element.querySelector(".live-tail")?.remove();
        ensureBody();
      }
      element.classList.toggle("open", next);
      header.setAttribute("aria-expanded", String(next));
    });
    return element;
  }

  private editCard(entryId: string, edit: FileEdit, index: number, cleanups: Array<() => void>): HTMLElement {
    const stats = editStats(edit);
    const host = h("div", { class: "edit-diff" });
    const path = h(
      "button",
      { class: "edit-path", type: "button", title: "Open in the Diff tab", onclick: () => this.callbacks.openFileInDiff(edit.movePath ?? edit.path) },
      displayPath(edit.path),
      edit.movePath ? ` → ${displayPath(edit.movePath)}` : "",
    );
    const card = h(
      "div",
      { class: `edit-card ${edit.kind}` },
      h(
        "div",
        { class: "edit-head" },
        h("span", { class: `edit-kind ${edit.kind}` }, edit.kind === "write" ? "write/overwrite" : edit.kind === "add" ? "A" : edit.kind === "delete" ? "D" : edit.movePath ? "R" : "M"),
        path,
        edit.kind === "write" ? h("span", { class: "row-meta" }, "prior content unknown") : h("span", { class: "row-meta" }, h("span", { class: "add" }, `+${stats.additions}`), " ", h("span", { class: "del" }, `−${stats.deletions}`)),
      ),
      host,
    );
    // Shiki highlighting is the costly part; defer it until the card is near view.
    let disposed = false;
    const observer = new IntersectionObserver(
      (records) => {
        if (disposed || !records.some((record) => record.isIntersecting)) return;
        observer.disconnect();
        cleanups.push(renderEdit(host, edit, `${entryId}:${index}`, this.callbacks.preferences()));
      },
      { root: this.scroller, rootMargin: "600px 0px" },
    );
    observer.observe(host);
    cleanups.push(() => { disposed = true; observer.disconnect(); });
    return card;
  }

  lastAgentText(): string | undefined {
    for (let index = this.transcript.entries.length - 1; index >= 0; index--) {
      const entry = this.transcript.entries[index];
      if (entry.kind === "agent") return entry.text;
    }
    return undefined;
  }
}

function verbFor(edits: FileEdit[]): string {
  if (edits.length && edits.every((edit) => edit.kind === "write")) return "wrote";
  if (edits.length && edits.every((edit) => edit.kind === "add")) return "created";
  if (edits.length && edits.every((edit) => edit.kind === "delete")) return "deleted";
  return "edited";
}

function displayPath(path: string): string {
  const parts = path.split("/");
  return parts.length > 4 ? `…/${parts.slice(-3).join("/")}` : path;
}

function searchText(entry: Entry): string {
  switch (entry.kind) {
    case "user":
    case "agent":
    case "thought":
      return entry.text.slice(0, 4000);
    case "command":
      return `${entry.command}\n${entry.output.slice(-2000)}`;
    case "files":
      return entry.edits.map((edit) => edit.path).join("\n") || entry.label;
    case "search":
      return entry.query;
    case "tool":
      return `${entry.name} ${entry.label}`;
    case "agentRun":
      return entry.label;
    case "turn":
      return `turn ${entry.status}`;
    case "notice":
      return entry.text;
  }
}
