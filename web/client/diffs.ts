// Pierre diff and tree wrappers. Chat rows render one edit each; the Diff tab
// renders the whole working tree against HEAD with a file tree beside it.
import { FileDiff, parseDiffFromFile, parsePatchFiles, type FileDiffMetadata } from "@pierre/diffs";
import { FileTree, type GitStatusEntry } from "@pierre/trees";
import { append, clear, h } from "./dom";
import { type FileEdit, unifiedPatchForEdit } from "./transcript";

export type DiffStyle = "unified" | "split";

export interface DiffPreferences {
  style: DiffStyle;
  wrap: boolean;
  themeType: "dark" | "light";
}

let diffRevision = 0;
const THEMES = { dark: "pierre-dark", light: "pierre-light" } as const;

function baseOptions(preferences: DiffPreferences, extra: Record<string, unknown> = {}) {
  return {
    theme: THEMES,
    themeType: preferences.themeType,
    diffStyle: preferences.style,
    overflow: preferences.wrap ? ("wrap" as const) : ("scroll" as const),
    hunkSeparators: "line-info" as const,
    lineDiffType: "word" as const,
    ...extra,
  };
}

export function fileDiffForEdit(edit: FileEdit, key: string): FileDiffMetadata | undefined {
  const name = edit.movePath ?? edit.path;
  key = `${key}:${++diffRevision}`;
  try {
    if (edit.oldText !== undefined || edit.newText !== undefined) {
      const metadata = parseDiffFromFile(
        { name: edit.path, contents: edit.oldText ?? "", cacheKey: `${key}:old` },
        { name, contents: edit.newText ?? "", cacheKey: `${key}:new` },
      );
      if (edit.fragment) for (const hunk of metadata.hunks) {
        hunk.noEOFCRAdditions = false;
        hunk.noEOFCRDeletions = false;
      }
      return metadata;
    }
    if (edit.diff === undefined) return undefined;
    if (edit.kind === "add" && !/^@@ /m.test(edit.diff))
      return parseDiffFromFile({ name, contents: "", cacheKey: `${key}:old` }, { name, contents: edit.diff, cacheKey: `${key}:new` });
    if (edit.kind === "delete" && !/^@@ /m.test(edit.diff))
      return parseDiffFromFile({ name: edit.path, contents: edit.diff, cacheKey: `${key}:old` }, { name, contents: "", cacheKey: `${key}:new` });
    const patch = unifiedPatchForEdit(edit);
    if (!patch) return undefined;
    return parsePatchFiles(patch, key)[0]?.files[0];
  } catch {
    return undefined;
  }
}

/** Renders one edit into `host`. Returns a cleanup for when the row leaves. */
export function renderEdit(host: HTMLElement, edit: FileEdit, key: string, preferences: DiffPreferences): () => void {
  const metadata = fileDiffForEdit(edit, key);
  if (!metadata) {
    host.append(h("div", { class: "diff-empty" }, edit.diff ? "This change could not be parsed as a diff." : "The provider sent no patch for this edit."));
    return () => {};
  }
  const instance = new FileDiff(baseOptions(preferences, {
    disableFileHeader: true,
    ...(edit.fragment ? { disableLineNumbers: true, hunkSeparators: "simple" } : {}),
  }));
  instance.render({ fileDiff: metadata, containerWrapper: host });
  return () => instance.cleanUp();
}

export interface WorkspaceDiffData {
  content: string;
  error?: string;
  branch?: string;
  cwd?: string;
  untracked: string[];
  touched: string[];
}

interface RenderedFile {
  name: string;
  section: HTMLElement;
  body: HTMLElement;
  instance?: FileDiff<undefined, undefined>;
  metadata: FileDiffMetadata;
  collapsed: boolean;
  /** This file's raw patch text; an unchanged patch keeps its rendered card. */
  patch: string;
}

/** Files above this many lines render only when scrolled to. */
const BACKGROUND_LINE_LIMIT = 4000;

/** A placeholder card: the file name and shimmering bars sized to the diff. */
function skeleton(name: string, lines: number): HTMLElement {
  const bars = Math.max(3, Math.min(14, Math.round(lines / 3)));
  return h(
    "div",
    { class: "diff-skel", "aria-hidden": "true" },
    h("div", { class: "skel-head" }, name ? h("span", { class: "skel-name" }, name) : h("span", { class: "skel-line short" })),
    ...Array.from({ length: bars }, (_, index) => h("span", { class: "skel-line", style: `width:${35 + ((index * 37) % 55)}%` })),
  );
}

/** Splits a multi-file git diff into each file's own patch text, by path. */
export function patchesByPath(content: string): Map<string, string> {
  const result = new Map<string, string>();
  for (const chunk of content.split(/^(?=diff --git )/m)) {
    const path = /^\+\+\+ b\/(.+)$/m.exec(chunk)?.[1] ?? /^diff --git a\/.+? b\/(.+)$/m.exec(chunk)?.[1];
    if (path) result.set(path, chunk);
  }
  return result;
}

/**
 * The Diff tab: a Pierre file tree on the left and one lazily rendered
 * Pierre FileDiff per changed file on the right.
 */
export class WorkspaceDiffView {
  readonly element: HTMLElement;
  private treeHost: HTMLElement;
  private filesHost: HTMLElement;
  private summary: HTMLElement;
  private tree?: FileTree;
  private files: RenderedFile[] = [];
  private lastContent?: string;
  private backgroundQueued = false;
  private lastUntracked = "";
  private lastMetadata = "";
  private observer: IntersectionObserver;
  private touchedOnly = false;
  private touched = new Set<string>();
  private data?: WorkspaceDiffData;

  constructor(
    private preferences: DiffPreferences,
    private readonly onPreferences: (preferences: DiffPreferences) => void,
  ) {
    this.summary = h("div", { class: "diff-summary" });
    this.treeHost = h("div", { class: "diff-tree" });
    this.filesHost = h("div", { class: "diff-files" });
    const toolbar = h(
      "div",
      { class: "diff-toolbar" },
      this.summary,
      h(
        "div",
        { class: "diff-toolbar-actions" },
        this.toggle("Split", () => this.preferences.style === "split", () => this.setPreferences({ style: this.preferences.style === "split" ? "unified" : "split" })),
        this.toggle("Wrap", () => this.preferences.wrap, () => this.setPreferences({ wrap: !this.preferences.wrap })),
        this.toggle("Session only", () => this.touchedOnly, () => {
          this.touchedOnly = !this.touchedOnly;
          this.lastContent = undefined;
          if (this.data) this.update(this.data);
        }),
        h("button", { class: "chip", title: "Fold or unfold every file (Z)", onclick: () => this.toggleAll() }, "Fold all"),
      ),
    );
    this.element = h("div", { class: "diff-view" }, toolbar, h("div", { class: "diff-body" }, this.treeHost, this.filesHost));
    this.observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          const file = this.files.find((candidate) => candidate.section === entry.target);
          if (file && !file.instance && !file.collapsed) this.mount(file);
        }
      },
      { root: this.filesHost, rootMargin: "800px 0px" },
    );
  }

  private toggle(label: string, active: () => boolean, onClick: () => void): HTMLButtonElement {
    const button = h("button", { class: "chip" }, label);
    const sync = () => button.classList.toggle("on", active());
    button.addEventListener("click", () => {
      onClick();
      sync();
    });
    sync();
    return button;
  }

  setPreferences(patch: Partial<DiffPreferences>): void {
    this.preferences = { ...this.preferences, ...patch };
    this.onPreferences(this.preferences);
    for (const file of this.files) file.instance?.setOptions(this.fileOptions(file));
    for (const file of this.files) file.instance?.rerender();
  }

  private fileOptions(file: RenderedFile) {
    return baseOptions(this.preferences, {
      collapsed: file.collapsed,
      stickyHeader: true,
      renderHeaderMetadata: () =>
        this.touched.has(file.name) ? h("span", { class: "touched-badge", title: "Edited since this session started" }, "session") : null,
    });
  }

  /** Mounts cards already in view; the observer covers the ones scrolled to later. */
  mountVisible(): void {
    const host = this.filesHost.getBoundingClientRect();
    if (!host.height) return;
    for (const file of this.files) {
      if (file.instance || file.collapsed) continue;
      const box = file.section.getBoundingClientRect();
      if (box.bottom >= host.top - 800 && box.top <= host.bottom + 800) this.mount(file);
    }
  }

  private mount(file: RenderedFile): void {
    this.observer.unobserve(file.section);
    file.instance = new FileDiff({
      ...this.fileOptions(file),
      // The skeleton stays until Pierre has painted the highlighted diff.
      onPostRender: () => file.section.querySelector(".diff-skel")?.remove(),
    });
    file.instance.render({ fileDiff: file.metadata, containerWrapper: file.body });
  }

  /**
   * Renders the remaining files a few at a time while the browser is idle,
   * even when the Diff tab is hidden, so opening it shows finished diffs.
   */
  private scheduleBackground(): void {
    if (this.backgroundQueued) return;
    this.backgroundQueued = true;
    const idle = (callback: () => void) =>
      "requestIdleCallback" in window ? requestIdleCallback(callback, { timeout: 400 }) : setTimeout(callback, 30);
    const step = () => {
      const started = performance.now();
      for (const file of this.files) {
        if (file.instance || file.collapsed || file.metadata.unifiedLineCount > BACKGROUND_LINE_LIMIT) continue;
        this.mount(file);
        if (performance.now() - started > 12) break;
      }
      if (this.files.some((file) => !file.instance && !file.collapsed && file.metadata.unifiedLineCount <= BACKGROUND_LINE_LIMIT)) idle(step);
      else this.backgroundQueued = false;
    };
    idle(step);
  }

  toggleAll(): void {
    const collapse = this.files.some((file) => !file.collapsed);
    for (const file of this.files) {
      file.collapsed = collapse;
      if (file.instance) {
        file.instance.setOptions(this.fileOptions(file));
        file.instance.rerender();
      } else if (!collapse) {
        this.observer.unobserve(file.section);
        this.observer.observe(file.section);
      }
    }
  }

  /** Scrolls to the next or previous file header. */
  jumpFile(direction: 1 | -1): void {
    const top = this.filesHost.scrollTop;
    const offsets = this.files.map((file) => file.section.offsetTop - this.filesHost.offsetTop);
    const target =
      direction > 0 ? offsets.find((offset) => offset > top + 4) : [...offsets].reverse().find((offset) => offset < top - 4);
    if (target !== undefined) this.filesHost.scrollTo({ top: target, behavior: "smooth" });
  }

  update(data: WorkspaceDiffData): void {
    this.data = data;
    this.touched = new Set(data.touched);
    const untrackedKey = data.untracked.join("\0");
    const metadataKey = JSON.stringify([data.touched, data.error, data.branch, data.cwd]);
    if (data.content === this.lastContent && untrackedKey === this.lastUntracked && metadataKey === this.lastMetadata) return;
    this.lastMetadata = metadataKey;
    this.lastContent = data.content;
    this.lastUntracked = untrackedKey;
    let parsed: FileDiffMetadata[] = [];
    try {
      parsed = data.content.trim() ? parsePatchFiles(data.content, `workspace:${++diffRevision}`).flatMap((patch) => patch.files) : [];
    } catch {
      parsed = [];
    }
    if (this.touchedOnly) parsed = parsed.filter((file) => this.touched.has(file.name));
    const untracked = this.touchedOnly ? data.untracked.filter((path) => this.touched.has(path)) : data.untracked;
    let additions = 0;
    let deletions = 0;
    for (const file of parsed)
      for (const hunk of file.hunks) {
        additions += hunk.additionLines;
        deletions += hunk.deletionLines;
      }
    clear(this.summary);
    append(this.summary, [
      h("span", { class: "diff-branch" }, data.branch ? `⎇ ${data.branch}` : "working tree"),
      h("span", null, `${parsed.length} file${parsed.length === 1 ? "" : "s"}`),
      h("span", { class: "add" }, `+${additions}`),
      h("span", { class: "del" }, `−${deletions}`),
      untracked.length ? h("span", { class: "dim" }, `${untracked.length} untracked`) : null,
    ]);

    // Keep every card whose patch is unchanged, so reopening the tab or a
    // live edit to one file never re-highlights the others.
    const patches = patchesByPath(data.content);
    const previous = new Map(this.files.map((file) => [file.name, file]));
    const next: RenderedFile[] = [];
    for (const metadata of parsed) {
      const patch = patches.get(metadata.name) ?? "";
      const kept = previous.get(metadata.name);
      if (kept && kept.patch === patch) {
        previous.delete(metadata.name);
        next.push(kept);
        continue;
      }
      const body = h("div", { class: "diff-file-body" });
      const section = h("section", { class: "diff-file", "data-path": metadata.name }, skeleton(metadata.name, metadata.unifiedLineCount), body);
      next.push({ name: metadata.name, section, body, metadata, patch, collapsed: kept?.collapsed ?? false });
    }
    for (const file of previous.values()) {
      this.observer.unobserve(file.section);
      file.instance?.cleanUp();
    }
    this.files = next;
    const children: HTMLElement[] = this.files.map((file) => file.section);
    if (data.error && !parsed.length) children.push(h("div", { class: "empty-state" }, data.error));
    else if (!parsed.length && !untracked.length)
      children.push(h("div", { class: "empty-state" }, h("div", { class: "empty-glyph" }, "∅"), "No tracked changes against HEAD."));
    this.filesHost.replaceChildren(...children);
    for (const file of this.files) if (!file.instance) this.observer.observe(file.section);
    requestAnimationFrame(() => this.mountVisible());
    this.scheduleBackground();
    if (untracked.length)
      this.filesHost.append(
        h(
          "section",
          { class: "diff-untracked" },
          h("div", { class: "diff-untracked-title" }, "Untracked files"),
          ...untracked.map((path) => h("div", { class: "diff-untracked-path", "data-path": path }, path)),
        ),
      );

    const statusFor = (file: FileDiffMetadata): GitStatusEntry["status"] =>
      file.type === "new" ? "added" : file.type === "deleted" ? "deleted" : file.type === "rename-pure" || file.type === "rename-changed" ? "renamed" : "modified";
    const gitStatus: GitStatusEntry[] = [
      ...parsed.map((file) => ({ path: file.name, status: statusFor(file) })),
      ...untracked.map((path) => ({ path, status: "untracked" as const })),
    ];
    const paths = gitStatus.map((entry) => entry.path);
    if (!this.tree) {
      this.tree = new FileTree({
        paths,
        gitStatus,
        initialExpansion: "open",
        flattenEmptyDirectories: true,
        search: paths.length > 12,
        onSelectionChange: (selected) => {
          const path = selected[0];
          if (path) this.reveal(path);
        },
      });
      this.tree.render({ containerWrapper: this.treeHost });
    } else {
      this.tree.resetPaths(paths);
      this.tree.setGitStatus(gitStatus);
    }
    this.treeHost.classList.toggle("hidden", paths.length === 0);
  }

  reveal(path: string): void {
    const target = this.filesHost.querySelector<HTMLElement>(`[data-path="${CSS.escape(path)}"]`);
    if (!target) return;
    const file = this.files.find((candidate) => candidate.section === target);
    if (file && file.collapsed) {
      file.collapsed = false;
      file.instance?.setOptions(this.fileOptions(file));
      file.instance?.rerender();
    }
    if (file && !file.instance) this.mount(file);
    target.scrollIntoView({ behavior: "smooth", block: "start" });
    target.classList.remove("flash");
    void target.offsetWidth;
    target.classList.add("flash");
  }

  setTheme(themeType: "dark" | "light"): void {
    if (this.preferences.themeType === themeType) return;
    this.setPreferences({ themeType });
  }

  reset(): void {
    this.lastContent = undefined;
    this.lastUntracked = "";
    this.lastMetadata = "";
    this.data = undefined;
    this.observer.disconnect();
    for (const file of this.files) file.instance?.cleanUp();
    this.files = [];
    this.filesHost.replaceChildren(...[0, 1, 2].map(() => h("section", { class: "diff-file" }, skeleton("", 12))));
    clear(this.summary);
    this.summary.append(h("span", { class: "skel-line short" }));
    this.tree?.cleanUp();
    this.tree = undefined;
    clear(this.treeHost);
  }
}
