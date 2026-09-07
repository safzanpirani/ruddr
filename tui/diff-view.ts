import {
  type CodeSpan,
  filetypeForPath,
  highlightCode,
  highlightLines
} from "./syntax";
// The Diff tab: changed-file tree, draggable divider, folding, hunk and file
// navigation, and the styled patch rows. It owns every piece of diff state
// and talks to the surrounding dashboard through DiffHost.
import {
  bg,
  bold,
  BoxRenderable,
  fg,
  italic,
  parseColor,
  ScrollBoxRenderable,
  SelectRenderable,
  SelectRenderableEvents,
  StyledText,
  t,
  TextRenderable,
  type CliRenderer,
  type KeyEvent,
} from "@opentui/core";
import {
  diffTreeWidthForPointer,
  diffTreeWidthForRatio,
  gitDiffFileStats,
  gitDiffGutterWidth,
  gitDiffSummary,
  gitDiffTree,
  nextGitDiffBoundary,
  parseGitDiff,
  parseGitDiffHunkHeader,
  visibleGitDiffLineIndices,
  type GitDiffFileStats,
  type GitDiffLine,
  type GitDiffSummary,
  type GitDiffTreeEntry,
  type Session,
  type StatusKind,
} from "./core";
import { diffCache, touchedSince } from "./git";
import type { MenuItem } from "./menu";
import { diffTints, palette } from "./palette";
import { listScrollOffset, scrollListBy, spanChunks } from "./render";
import type { ActivityRow } from "./rows";

/** What the diff view needs from the dashboard around it. */
export interface DiffHost {
  renderer: CliRenderer;
  artifactScroll: ScrollBoxRenderable;
  artifactBody: BoxRenderable;
  isActive(): boolean;
  rows(): ActivityRow[];
  selectedRow(): number;
  setSelectedRow(index: number): void;
  setRows(rows: ActivityRow[]): void;
  renderRows(preserveScroll: boolean): void;
  refreshRow(index: number): void;
  stopFollowing(): void;
  invalidateRows(): void;
  reloadSelected(): void;
  activateSelectedRow(): void;
  selectedSession(): Session | undefined;
  setStatus(message: string, kind?: boolean | StatusKind): void;
  updateChrome(): void;
  copyText(value: string): void;
  persistSidebar(width: number, ratio: number | undefined): void;
  initialTreeWidth: number;
  initialTreeRatio: number | undefined;
}

export interface DiffView {
  sidebar: BoxRenderable;
  divider: BoxRenderable;
  readonly summary: GitDiffSummary | undefined;
  readonly treeWidth: number;
  load(
    session: Session,
    result: { content: string; error?: string } | undefined,
    stillCurrent: () => boolean,
  ): Promise<boolean>;
  buildRows(): ActivityRow[];
  renderRow(row: ActivityRow, match: boolean, selected: boolean): StyledText;
  renderSpacerRow(row: ActivityRow, selected: boolean): StyledText;
  rowWidth(row: ActivityRow): number;
  handleKey(key: KeyEvent): boolean;
  menuItems(row: ActivityRow): MenuItem[];
  toggleFile(path: string): void;
  toggleAll(): void;
  syncTreeToRow(rowIndex: number): void;
  /** Rebuild the file tree from the current lines and fold state. */
  refreshTree(): void;
  updateChrome(): void;
  applyTheme(): void;
  dispose(): void;
}

export function createDiffView(host: DiffHost): DiffView {
  const { renderer, artifactScroll, artifactBody } = host;
  let treeWidth = host.initialTreeWidth;
  let treeRatio = host.initialTreeRatio;
  let dividerDragging = false;
  let dividerHovered = false;
  let navigationPrefix: "[" | "]" | undefined;
  let navigationTimer: ReturnType<typeof setTimeout> | undefined;
  const collapsedDirectories = new Set<string>();
  const collapsedFiles = new Set<string>();
  let lines: GitDiffLine[] = [];
  let summary: GitDiffSummary | undefined;
  let fileStats = new Map<string, GitDiffFileStats>();
  let gutterWidth = 2;
  let error: string | undefined;
  let touchedPaths = new Set<string>();
  let touchedSignature = "";
  // Highlight spans per parsed diff line, scanned hunk by hunk so block
  // comments and template strings keep their color across lines.
  let spans: Array<CodeSpan[] | undefined> = [];

  const summaryLine = new TextRenderable(renderer, {
    id: "diff-summary",
    content: "",
    fg: palette.dim,
    height: 1,
    width: "100%",
    paddingLeft: 1,
    wrapMode: "none",
    truncate: true,
  });
  const fileList = new SelectRenderable(renderer, {
    id: "diff-files",
    width: "100%",
    flexGrow: 1,
    options: [],
    showDescription: false,
    showScrollIndicator: true,
    showSelectionIndicator: true,
    backgroundColor: palette.panel,
    focusedBackgroundColor: palette.panel,
    textColor: palette.dim,
    focusedTextColor: palette.text,
    selectedBackgroundColor: palette.selected,
    selectedTextColor: palette.accent,
    renderAfter(buffer) {
      const options = this.options;
      const visibleItems = Math.max(1, Math.floor(this.height));
      const scrollOffset = listScrollOffset(this);
      const addition = parseColor(palette.success);
      const deletion = parseColor(palette.danger);
      const labelX = 3;
      for (const [visibleIndex, option] of options
        .slice(scrollOffset, scrollOffset + visibleItems)
        .entries()) {
        const entry = option.value as GitDiffTreeEntry | undefined;
        const match = entry ? /(\+\d+) (−\d+)$/.exec(entry.label) : null;
        if (!match) continue;
        if (entry && touchedPaths.has(entry.path)) {
          const indent = /^\s*/.exec(entry.label)?.[0].length ?? 0;
          buffer.drawText("●", labelX + indent, visibleIndex, parseColor(palette.accent));
        }
        const countWidth = match[1].length + 1 + match[2].length;
        const countX = Math.max(labelX, this.width - countWidth - 2);
        if (entry?.status) {
          const statusColor =
            entry.status === "A"
              ? addition
              : entry.status === "D"
                ? deletion
                : parseColor(
                    entry.status === "R" ? palette.warning : palette.accent,
                  );
          buffer.drawText(entry.status, Math.max(labelX, countX - 3), visibleIndex, statusColor);
        }
        buffer.drawText(match[1], countX, visibleIndex, addition);
        buffer.drawText(
          match[2],
          countX + match[1].length + 1,
          visibleIndex,
          deletion,
        );
      }
    },
  });
  const sidebar = new BoxRenderable(renderer, {
    id: "diff-sidebar",
    width: treeWidth,
    height: "100%",
    flexShrink: 0,
    flexDirection: "column",
    backgroundColor: palette.panel,
    visible: false,
  });
  sidebar.add(summaryLine);
  sidebar.add(fileList);
  const divider = new BoxRenderable(renderer, {
    id: "diff-divider",
    width: 1,
    height: "100%",
    flexShrink: 0,
    backgroundColor: palette.background,
    visible: false,
    renderAfter(buffer) {
      const active = dividerDragging || dividerHovered;
      const color = parseColor(active ? palette.accent : palette.border);
      const gripStart = Math.max(0, Math.floor(this.height / 2) - 1);
      for (let y = 0; y < this.height; y++) {
        const grip = y >= gripStart && y < gripStart + 3;
        buffer.drawText(
          grip ? "┃" : "┆",
          0,
          y,
          grip && !active ? parseColor(palette.dim) : color,
        );
      }
    },
  });
  artifactBody.add(sidebar);
  artifactBody.add(divider);

  // --- mouse -----------------------------------------------------------
  fileList.onMouseScroll = (event) => {
    const steps = Math.max(1, Math.round(event.scroll?.delta ?? 1));
    const direction =
      event.scroll?.direction === "up" ? -1 : event.scroll?.direction === "down" ? 1 : 0;
    if (direction !== 0) scrollListBy(fileList, direction * steps, 1);
  };
  divider.onMouseOver = () => {
    dividerHovered = true;
    divider.requestRender();
  };
  divider.onMouseOut = () => {
    dividerHovered = false;
    divider.requestRender();
  };
  const resizeTree = (pointerX: number) => {
    treeWidth = diffTreeWidthForPointer(pointerX, artifactBody.x, artifactBody.width);
    sidebar.width = treeWidth;
    divider.requestRender();
    updateTree();
  };
  const finishDrag = () => {
    if (!dividerDragging) return;
    dividerDragging = false;
    divider.requestRender();
    if (artifactBody.width > 0) treeRatio = treeWidth / artifactBody.width;
    host.persistSidebar(treeWidth, treeRatio);
  };
  divider.onMouseDown = (event) => {
    dividerDragging = true;
    divider.requestRender();
    resizeTree(event.x);
    event.preventDefault();
  };
  artifactBody.onMouseDrag = (event) => {
    if (dividerDragging) resizeTree(event.x);
  };
  artifactBody.onMouseDragEnd = finishDrag;
  artifactBody.onMouseUp = finishDrag;
  fileList.onMouseDown = (event) => {
    fileList.focus();
    const options = fileList.options;
    const scrollOffset = listScrollOffset(fileList);
    const clickedIndex = scrollOffset + Math.floor(event.y - fileList.y);
    if (clickedIndex >= 0 && clickedIndex < options.length) {
      fileList.setSelectedIndex(clickedIndex);
      activateTreeEntry(options[clickedIndex]?.value, true);
    }
  };
  fileList.on(SelectRenderableEvents.SELECTION_CHANGED, (_index, option) => {
    activateTreeEntry(option?.value, false);
  });
  fileList.on(SelectRenderableEvents.ITEM_SELECTED, (_index, option) => {
    activateTreeEntry(option?.value, true);
  });

  // --- navigation ------------------------------------------------------
  function activateTreeEntry(value: unknown, toggleDirectory: boolean): void {
    const entry = value as GitDiffTreeEntry | undefined;
    if (!entry || !host.isActive()) return;
    if (entry.kind === "directory") {
      if (!toggleDirectory) return;
      if (collapsedDirectories.has(entry.path)) collapsedDirectories.delete(entry.path);
      else collapsedDirectories.add(entry.path);
      updateTree(entry.path);
      return;
    }
    jumpToLine(entry.rowIndex);
  }

  /** Jump to a row by its index in the full parsed diff (tree row index). */
  function jumpToLine(lineIndex: unknown): void {
    if (!host.isActive() || typeof lineIndex !== "number") return;
    const rowIndex = host.rows().findIndex((row) => row.lineIndex === lineIndex);
    if (rowIndex < 0) return;
    jumpToRow(rowIndex);
  }

  function jumpToRow(rowIndex: number): void {
    if (!host.isActive()) return;
    const previousRow = host.selectedRow();
    host.setSelectedRow(rowIndex);
    host.stopFollowing();
    host.refreshRow(previousRow);
    host.refreshRow(rowIndex);
    artifactScroll.scrollTo({ x: 0, y: Math.max(0, rowIndex - 1) });
    host.updateChrome();
  }

  // Diff rows carry placeholder context lines for the blank spacer rows so
  // boundary search keeps working on the visible row list.
  function rowLines(): GitDiffLine[] {
    return host.rows().map((row) => row.diff ?? { kind: "context", text: "" });
  }

  function moveToBoundary(kind: "hunk" | "file", direction: number): void {
    const visible = rowLines();
    const target = nextGitDiffBoundary(visible, host.selectedRow(), kind, direction);
    if (target === undefined) {
      host.setStatus(`No diff ${kind}s`, true);
      return;
    }
    jumpToRow(target);
    syncTreeToRow(target);
    const boundaries = visible.flatMap((line, index) => (line.kind === kind ? [index] : []));
    host.setStatus(
      `${kind === "hunk" ? "Hunk" : "File"} ${boundaries.indexOf(target) + 1} of ${boundaries.length}`,
    );
  }

  function syncTreeToRow(rowIndex: number): void {
    const path = host.rows()[rowIndex]?.diff?.path;
    if (!path) return;
    const optionIndex = fileList.options.findIndex(
      (option) =>
        (option.value as GitDiffTreeEntry | undefined)?.kind === "file" &&
        (option.value as GitDiffTreeEntry).path === path,
    );
    if (optionIndex >= 0 && optionIndex !== fileList.getSelectedIndex())
      fileList.setSelectedIndex(optionIndex);
  }

  function toggleFile(path: string): void {
    if (!host.isActive()) return;
    const folded = !collapsedFiles.has(path);
    if (folded) collapsedFiles.add(path);
    else collapsedFiles.delete(path);
    host.setRows(buildRows());
    const headerRow = host
      .rows()
      .findIndex((row) => row.diff?.kind === "file" && row.diff.path === path);
    if (headerRow >= 0) jumpToRow(headerRow);
    updateTree(path);
    host.setStatus(`${folded ? "Folded" : "Unfolded"} ${path}`);
  }

  function toggleAll(): void {
    if (!host.isActive() || lines.length === 0) return;
    const paths = [...fileStats.keys()];
    const foldAll = collapsedFiles.size < paths.length;
    collapsedFiles.clear();
    if (foldAll) for (const path of paths) collapsedFiles.add(path);
    host.setRows(buildRows());
    if (host.selectedRow() >= host.rows().length) host.setSelectedRow(0);
    host.renderRows(true);
    updateTree();
    host.setStatus(foldAll ? "Folded every file" : "Unfolded every file");
  }

  function handleKey(key: KeyEvent): boolean {
    if (key.name === "[" || key.name === "]") {
      navigationPrefix = key.name;
      if (navigationTimer) clearTimeout(navigationTimer);
      navigationTimer = setTimeout(() => {
        navigationPrefix = undefined;
      }, 1_200);
      host.setStatus(`${key.name}c hunk · ${key.name}f file`);
      return true;
    }
    if (navigationPrefix && (key.name === "c" || key.name === "f")) {
      const direction = navigationPrefix === "]" ? 1 : -1;
      const kind = key.name === "c" ? "hunk" : "file";
      navigationPrefix = undefined;
      if (navigationTimer) clearTimeout(navigationTimer);
      moveToBoundary(kind, direction);
      return true;
    }
    navigationPrefix = undefined;
    if (navigationTimer) clearTimeout(navigationTimer);
    if (key.name === "z" && key.shift) {
      toggleAll();
      return true;
    }
    if (key.name === "z" || key.name === "space") {
      host.activateSelectedRow();
      return true;
    }
    return false;
  }

  // --- data ------------------------------------------------------------
  function highlightDiffLines(source: readonly GitDiffLine[]): Array<CodeSpan[] | undefined> {
    const result: Array<CodeSpan[] | undefined> = new Array(source.length);
    let hunkStart = -1;
    const flush = (end: number) => {
      if (hunkStart < 0) return;
      const filetype = filetypeForPath(source[hunkStart].path);
      const contentLines = source.slice(hunkStart, end).map((line) => line.text.slice(1));
      const scanned = highlightLines(contentLines, filetype);
      for (const [offset, lineSpans] of scanned.entries()) result[hunkStart + offset] = lineSpans;
      hunkStart = -1;
    };
    for (const [index, line] of source.entries()) {
      const isContent =
        line.kind === "addition" || line.kind === "deletion" || line.kind === "context";
      if (isContent) {
        if (hunkStart < 0) hunkStart = index;
      } else flush(index);
    }
    flush(source.length);
    return result;
  }

  async function load(
    session: Session,
    result: { content: string; error?: string } | undefined,
    stillCurrent: () => boolean,
  ): Promise<boolean> {
    if (!stillCurrent()) return false;
    const nextLines = parseGitDiff(result?.content ?? "");
    const nextFileStats = gitDiffFileStats(nextLines);
    const signature = `${session.stateDir}:${session.startedAt}:${[...nextFileStats.keys()].join("\0")}:${result?.content.length ?? 0}`;
    let nextTouchedPaths = session.cwd ? touchedPaths : new Set<string>();
    if (signature !== touchedSignature && session.cwd) {
      nextTouchedPaths = await touchedSince(session.cwd, nextFileStats.keys(), session.startedAt);
      if (!stillCurrent()) return false;
    }
    lines = nextLines;
    summary = gitDiffSummary(lines);
    fileStats = nextFileStats;
    gutterWidth = gitDiffGutterWidth(lines);
    spans = highlightDiffLines(lines);
    error = result?.error ?? (session.cwd ? undefined : "This session has no working directory.");
    for (const path of collapsedFiles) if (!fileStats.has(path)) collapsedFiles.delete(path);
    if (signature !== touchedSignature) {
      touchedSignature = signature;
      touchedPaths = nextTouchedPaths;
      host.invalidateRows();
    }
    return true;
  }

  function buildRows(): ActivityRow[] {
    if (lines.length === 0) {
      if (error)
        return [
          { id: "empty", text: `× ${error}`, copyText: "" },
          {
            id: "empty-action",
            text: "  Click here or press Enter to retry.",
            copyText: "",
            action: () => {
              const cwd = host.selectedSession()?.cwd;
              if (cwd) diffCache.delete(cwd);
              host.invalidateRows();
              host.reloadSelected();
            },
          },
        ];
      return [
        { id: "empty", text: "✓ Working tree matches HEAD", copyText: "" },
        {
          id: "empty-hint",
          text: "  Tracked changes appear here as the session edits files. Untracked files are not shown.",
          copyText: "",
        },
      ];
    }
    const rows: ActivityRow[] = [];
    for (const index of visibleGitDiffLineIndices(lines, collapsedFiles)) {
      const diff = lines[index];
      if (diff.kind === "file" && rows.length > 0)
        rows.push({ id: `diff-gap:${diff.path}`, text: "", copyText: "" });
      rows.push({
        id: `diff:${index}:${diff.text}`,
        diff,
        lineIndex: index,
        text: diff.text,
        copyText: diff.text,
      });
    }
    return rows;
  }

  function menuItems(row: ActivityRow): MenuItem[] {
    if (!row.diff?.path) return [];
    const path = row.diff.path;
    return [
      {
        label: collapsedFiles.has(path) ? "Unfold file" : "Fold file",
        hint: path,
        run: () => toggleFile(path),
      },
      {
        label: collapsedFiles.size < fileStats.size ? "Fold every file" : "Unfold every file",
        run: () => toggleAll(),
      },
      { label: "Copy file path", hint: path, run: () => host.copyText(path) },
    ];
  }

  // --- tree and chrome -------------------------------------------------
  function updateTree(selectedPath?: string): void {
    const entries = gitDiffTree(lines, collapsedDirectories, collapsedFiles);
    fileList.options = entries.map((entry) => ({
      name: entry.kind === "file" ? treeFileName(entry) : entry.label,
      description: "",
      value: entry,
    }));
    updateSummaryLine();
    if (selectedPath) {
      const selectedIndex = entries.findIndex((entry) => entry.path === selectedPath);
      if (selectedIndex >= 0) fileList.setSelectedIndex(selectedIndex);
    }
  }

  // Leaves a fixed column free on the right for "M  +12 −3" so the counts
  // drawn by renderAfter never overlap a long file name.
  function treeFileName(entry: GitDiffTreeEntry): string {
    const match = /^(\s*)  󰈔 (.+?)  [MADR]  (\+\d+ −\d+)$/.exec(entry.label);
    if (!match) return entry.label;
    const [, indent, name, counts] = match;
    const icon = entry.collapsed ? "▸" : "󰈔";
    const reserved = counts.length + 4 + 4; // status letter, gaps, indicator
    const available = Math.max(6, treeWidth - 3 - indent.length - 4 - reserved);
    const shown =
      name.length <= available
        ? name
        : `${name.slice(0, Math.max(1, Math.ceil((available - 1) / 2)))}…${name.slice(-(Math.floor((available - 1) / 2)))}`;
    return `${indent}  ${icon} ${shown}`;
  }

  function updateSummaryLine(): void {
    if (!summary || summary.files === 0) {
      summaryLine.content = t`${fg(palette.dim)("no changes")}`;
      return;
    }
    const folded = collapsedFiles.size > 0 ? ` · ${collapsedFiles.size} folded` : "";
    summaryLine.content = t`${fg(palette.text)(`${summary.files} ${summary.files === 1 ? "file" : "files"}`)} ${fg(palette.success)(`+${summary.additions}`)} ${fg(palette.danger)(`−${summary.deletions}`)}${fg(palette.dim)(folded)}`;
  }

  function updateChrome(): void {
    const visible = host.isActive() && renderer.width >= 100;
    sidebar.visible = visible;
    divider.visible = visible;
    if (!dividerDragging) {
      const container = artifactBody.width || Math.max(60, renderer.width - 46);
      const next = diffTreeWidthForRatio(treeRatio, container, treeWidth);
      if (next !== treeWidth) {
        treeWidth = next;
        updateTree();
      }
    }
    sidebar.width = treeWidth;
    updateSummaryLine();
  }

  function applyTheme(): void {
    fileList.backgroundColor = palette.panel;
    fileList.focusedBackgroundColor = palette.panel;
    fileList.textColor = palette.dim;
    fileList.focusedTextColor = palette.text;
    fileList.selectedBackgroundColor = palette.selected;
    fileList.selectedTextColor = palette.accent;
    divider.backgroundColor = palette.background;
    divider.requestRender();
    sidebar.backgroundColor = palette.panel;
    summaryLine.fg = palette.dim;
  }

  // --- rows ------------------------------------------------------------
  function gutterCells(): number {
    return gutterWidth * 2 + 3;
  }

  function rowWidth(row: ActivityRow): number {
    const textWidth =
      row.diff?.kind === "file"
        ? (row.diff.path?.length ?? row.text.length) + 24
        : row.text.length;
    return Math.max(artifactScroll.width, gutterCells() + textWidth + 3);
  }

  function padCells(text: string, width: number): string {
    const missing = width - [...text].length;
    return missing > 0 ? text + " ".repeat(missing) : text;
  }

  function renderSpacerRow(row: ActivityRow, selected: boolean): StyledText {
    const text = " ".repeat(rowWidth(row));
    return selected ? t`${bg(palette.selected)(text)}` : t`${text}`;
  }

  function renderRow(row: ActivityRow, match: boolean, selected: boolean): StyledText {
    const diff = row.diff!;
    const width = rowWidth(row);
    const blankGutter = " ".repeat(gutterCells());
    const markerText = match ? "◆" : " ";
    const markerColor = match ? palette.warning : palette.dim;

    if (diff.kind === "file") {
      const path = diff.path ?? diff.text;
      const stats = fileStats.get(path);
      const folded = collapsedFiles.has(path);
      const touched = touchedPaths.has(path);
      const rowBg = selected ? palette.selected : palette.panel;
      const statusColor =
        stats?.status === "A"
          ? palette.success
          : stats?.status === "D"
            ? palette.danger
            : stats?.status === "R"
              ? palette.warning
              : palette.accent;
      const head = ` ${folded ? "▸" : "▾"} ${path}`;
      const counts = stats ? `  ${stats.status}  +${stats.additions} −${stats.deletions}` : "";
      const tail = padCells(
        "",
        Math.max(
          0,
          width - [...head].length - [...counts].length - (folded ? 9 : 0) - (touched ? 16 : 0),
        ),
      );
      const chunks = [
        ...t`${bg(rowBg)(fg(markerColor)(markerText))}${bg(rowBg)(bold(fg(palette.accent)(head)))}`.chunks,
      ];
      if (stats)
        chunks.push(
          ...t`${bg(rowBg)(fg(palette.dim)("  "))}${bg(rowBg)(bold(fg(statusColor)(stats.status)))}${bg(rowBg)(fg(palette.dim)("  "))}${bg(rowBg)(fg(palette.success)(`+${stats.additions}`))}${bg(rowBg)(fg(palette.dim)(" "))}${bg(rowBg)(fg(palette.danger)(`−${stats.deletions}`))}`
            .chunks,
        );
      if (folded)
        chunks.push(...t`${bg(rowBg)(italic(fg(palette.dim)("  folded")))}`.chunks);
      if (touched)
        chunks.push(
          ...t`${bg(rowBg)(fg(palette.dim)("  "))}${bg(rowBg)(fg(palette.accent)("●"))}${bg(rowBg)(fg(palette.dim)(" this session"))}`
            .chunks,
        );
      chunks.push(...t`${bg(rowBg)(tail)}`.chunks);
      return new StyledText(chunks);
    }

    if (diff.kind === "hunk") {
      const header = parseGitDiffHunkHeader(diff.text);
      const rowBg = selected ? palette.selected : diffTints.hunkBg;
      const range = header
        ? `@@ -${header.oldStart},${header.oldCount} +${header.newStart},${header.newCount} @@`
        : diff.text;
      const context = header?.context ? ` ${header.context}` : "";
      const body = ` ${range}${context}`;
      const tail = padCells("", Math.max(0, width - gutterCells() - 1 - [...body].length));
      return t`${bg(rowBg)(fg(markerColor)(markerText))}${bg(rowBg)(fg(palette.dim)(blankGutter.slice(1)))}${bg(rowBg)(fg(palette.accent)(` ${range}`))}${bg(rowBg)(italic(fg(palette.dim)(context)))}${bg(rowBg)(tail)}`;
    }

    if (diff.kind === "metadata") {
      const rowBg = selected ? palette.selected : palette.background;
      const body = ` ${diff.text}`;
      const tail = padCells("", Math.max(0, width - gutterCells() - 1 - [...body].length));
      return t`${bg(rowBg)(fg(markerColor)(markerText))}${bg(rowBg)(blankGutter.slice(1))}${bg(rowBg)(fg(palette.dim)(body))}${bg(rowBg)(tail)}`;
    }

    const oldNumber = diff.oldLine === undefined ? "" : String(diff.oldLine);
    const newNumber = diff.newLine === undefined ? "" : String(diff.newLine);
    const gutter = `${oldNumber.padStart(gutterWidth)} ${newNumber.padStart(gutterWidth)} `;
    const sign = diff.text[0] ?? " ";
    const content = diff.text.slice(1);
    const lineBg = selected
      ? palette.selected
      : diff.kind === "addition"
        ? diffTints.additionBg
        : diff.kind === "deletion"
          ? diffTints.deletionBg
          : palette.background;
    const gutterBg = selected
      ? palette.selected
      : diff.kind === "addition"
        ? diffTints.additionGutterBg
        : diff.kind === "deletion"
          ? diffTints.deletionGutterBg
          : palette.background;
    const signColor =
      diff.kind === "addition"
        ? palette.success
        : diff.kind === "deletion"
          ? palette.danger
          : palette.dim;
    const tail = padCells("", Math.max(0, width - gutterCells() - 1 - [...content].length));
    return new StyledText([
      ...t`${bg(gutterBg)(fg(markerColor)(markerText))}${bg(gutterBg)(fg(palette.dim)(gutter))}${bg(lineBg)(bold(fg(signColor)(sign)))}`
        .chunks,
      ...spanChunks(
        (row.lineIndex !== undefined ? spans[row.lineIndex] : undefined) ??
          highlightCode(content, filetypeForPath(diff.path)),
        diff.kind === "context",
        lineBg,
      ),
      ...t`${bg(lineBg)(tail)}`.chunks,
    ]);
  }

  return {
    sidebar,
    divider,
    get summary() {
      return summary;
    },
    get treeWidth() {
      return treeWidth;
    },
    load,
    buildRows,
    renderRow,
    renderSpacerRow,
    rowWidth,
    handleKey,
    menuItems,
    toggleFile,
    toggleAll,
    syncTreeToRow,
    refreshTree: () => updateTree(),
    updateChrome,
    applyTheme,
    dispose() {
      if (navigationTimer) clearTimeout(navigationTimer);
    },
  };
}
