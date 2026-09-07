// Pure renderers: styled text for chat, activity, tool cards, code, and
// session details. They read the live palette and hold no UI state.
import {
  bg,
  bold,
  fg,
  italic,
  SelectRenderable,
  StyledText,
  t,
  underline,
} from "@opentui/core";
import {
  blendHex,
  clampScrollOffset,
  contextMeter,
  formatTokenUsage,
  renderMeter,
  highlightCode,
  highlightLines,
  parseMarkdown,
  statusGlyph,
  type ChatEntry,
  type CodeSpan,
  type CodeToken,
  type InlineSpan,
  type MarkdownLine,
  type Session,
  type ToolEventDetail,
  type TraceActivity,
} from "./core";
import { palette } from "./palette";

export const TOOL_OUTPUT_LINES = 40;

export function tokenColor(token: CodeToken, muted: boolean): string {
  let base: string;
  switch (token) {
    case "keyword":
      base = palette.accent;
      break;
    case "string":
      base = palette.success;
      break;
    case "regex":
      base = blendHex(palette.success, palette.warning, 0.4);
      break;
    case "number":
    case "constant":
      base = palette.warning;
      break;
    case "comment":
      base = palette.dim;
      break;
    case "type":
      base = blendHex(palette.text, palette.warning, 0.45);
      break;
    case "function":
      base = blendHex(palette.text, palette.accent, 0.55);
      break;
    case "tag":
      base = palette.danger;
      break;
    case "attribute":
      base = blendHex(palette.warning, palette.text, 0.35);
      break;
    case "property":
      base = blendHex(palette.text, palette.accent, 0.35);
      break;
    case "operator":
      base = blendHex(palette.text, palette.danger, 0.3);
      break;
    case "punctuation":
      base = blendHex(palette.text, palette.dim, 0.5);
      break;
    case "heading":
      base = palette.accent;
      break;
    default:
      base = palette.text;
  }
  return muted ? blendHex(base, palette.dim, 0.55) : base;
}

export function spanChunks(
  spans: readonly CodeSpan[],
  muted: boolean,
  rowBg?: string,
): StyledText["chunks"] {
  const chunks: StyledText["chunks"] = [];
  for (const span of spans) {
    let chunk = fg(tokenColor(span.token, muted))(span.text);
    if (span.token === "comment") chunk = italic(chunk);
    else if (span.token === "keyword" || span.token === "function" || span.token === "heading")
      chunk = bold(chunk);
    else if (span.token === "attribute") chunk = italic(chunk);
    if (rowBg) chunk = bg(rowBg)(chunk);
    chunks.push(...t`${chunk}`.chunks);
  }
  return chunks;
}

export function codeChunks(
  line: string,
  filetype: string,
  muted: boolean,
  rowBg?: string,
): StyledText["chunks"] {
  return spanChunks(highlightCode(line, filetype), muted, rowBg);
}

// SelectRenderable only scrolls to follow its selection; wheel scrolling has
// to move the viewport without touching the selected item, so read and write
// its offset directly. The list re-centers on the next selection change.
export interface ScrollableList {
  scrollOffset: number;
}

export function listScrollOffset(list: SelectRenderable): number {
  return (list as unknown as ScrollableList).scrollOffset ?? 0;
}

export function scrollListBy(
  list: SelectRenderable,
  delta: number,
  linesPerItem: number,
): number {
  const visible = Math.max(1, Math.floor(list.height / linesPerItem));
  const next = clampScrollOffset(
    listScrollOffset(list) + delta,
    list.options.length,
    visible,
  );
  (list as unknown as ScrollableList).scrollOffset = next;
  list.requestRender();
  return next;
}

export function renderSessionDetails(
  session: Session | undefined,
  content: string,
): string | StyledText {
  if (!session) return content;
  const chunks: StyledText["chunks"] = [];
  const lines = content.split("\n");
  for (const [index, line] of lines.entries()) {
    const match = /^(\S+)(\s+)(.*)$/.exec(line);
    if (!match) chunks.push(...t`${fg(palette.text)(line)}`.chunks);
    else {
      const [, label, spacing, value] = match;
      const labelChunk = fg(palette.dim)(`${label}${spacing}`);
      if (label === "status")
        chunks.push(
          ...t`${labelChunk}${bold(fg(sessionStatusColor(session.status))(value))}`
            .chunks,
        );
      else if (label === "provider" || label === "model" || label === "cwd")
        chunks.push(...t`${labelChunk}${fg(palette.accent)(value)}`.chunks);
      else chunks.push(...t`${labelChunk}${fg(palette.text)(value)}`.chunks);
    }
    if (index < lines.length - 1) chunks.push(...t`\n`.chunks);
  }
  return new StyledText(chunks);
}

export function sessionStatusColor(status: string): string {
  if (status === "active" || status === "completed") return palette.success;
  if (status === "starting") return palette.warning;
  if (status === "failed" || status === "interrupted" || status === "stale")
    return palette.danger;
  return palette.text;
}

export function renderChatEntry(entry: ChatEntry, revealed?: number): StyledText {
  if (entry.kind === "user")
    return t`${bold(fg(palette.accent)("❯ "))}${bold(fg(palette.text)(entry.text))}`;
  if (entry.kind === "agent") {
    const text =
      revealed !== undefined && revealed < entry.text.length
        ? entry.text.slice(0, revealed)
        : entry.text;
    const chunks = renderMarkdown(text);
    if (revealed !== undefined && revealed < entry.text.length)
      chunks.push(...t`${fg(palette.accent)("▍")}`.chunks);
    return new StyledText(chunks);
  }
  if (entry.kind === "thought")
    return t`${italic(fg(palette.dim)(entry.text))}`;
  const status = entry.status ?? "running";
  const glyph = status === "completed" ? "•" : status === "failed" ? "×" : "◐";
  const color =
    status === "completed"
      ? palette.dim
      : status === "failed"
        ? palette.danger
        : palette.warning;
  return t`  ${fg(color)(glyph)} ${fg(palette.dim)(entry.text)}`;
}

export function renderInline(spans: InlineSpan[], baseColor = palette.text): StyledText["chunks"] {
  const chunks: StyledText["chunks"] = [];
  for (const span of spans) {
    const chunk =
      span.style === "code"
        ? bg(palette.panel)(fg(palette.accent)(` ${span.text} `))
        : span.style === "bold"
          ? bold(fg(baseColor)(span.text))
          : span.style === "italic"
            ? italic(fg(baseColor)(span.text))
            : span.style === "link"
              ? underline(fg(palette.accent)(span.text))
              : fg(baseColor)(span.text);
    chunks.push(...t`${chunk}`.chunks);
  }
  return chunks;
}

export function renderMarkdownLine(line: MarkdownLine): StyledText["chunks"] {
  switch (line.kind) {
    case "heading": {
      const color = (line.level ?? 1) <= 2 ? palette.accent : palette.text;
      const prefix = (line.level ?? 1) <= 2 ? "" : `${"#".repeat(line.level ?? 3)} `;
      return t`${bold(fg(color)(prefix))}${bold(fg(color)(line.spans.map((span) => span.text).join("")))}`.chunks;
    }
    case "bullet":
      return [
        ...t`${"  ".repeat(line.indent ?? 0)}${fg(palette.accent)("•")} `.chunks,
        ...renderInline(line.spans),
      ];
    case "numbered":
      return [
        ...t`${"  ".repeat(line.indent ?? 0)}${fg(palette.accent)(line.marker ?? "1.")} `.chunks,
        ...renderInline(line.spans),
      ];
    case "quote":
      return [
        ...t`${fg(palette.border)("▎ ")}`.chunks,
        ...renderInline(line.spans, palette.dim),
      ];
    case "fence":
      return t`${fg(palette.dim)(`  ${line.language ? `▎ ${line.language}` : "▎"}`)}`.chunks;
    case "code": {
      const text = line.spans.map((span) => span.text).join("");
      return [
        ...t`${bg(palette.panel)("  ")}`.chunks,
        ...codeChunks(text, line.language ?? "plain", false, palette.panel),
        ...t`${bg(palette.panel)(" ")}`.chunks,
      ];
    }
    case "rule":
      return t`${fg(palette.border)("─".repeat(24))}`.chunks;
    case "blank":
      return [];
    default:
      return renderInline(line.spans);
  }
}

export function renderMarkdown(text: string): StyledText["chunks"] {
  const lines = parseMarkdown(text);
  const chunks: StyledText["chunks"] = [];
  let block: { start: number; language: string } | undefined;
  let blockSpans: CodeSpan[][] = [];
  for (const [index, line] of lines.entries()) {
    if (index > 0) chunks.push(...t`\n`.chunks);
    if (line.kind === "code") {
      if (!block || block.language !== (line.language ?? "plain")) {
        block = { start: index, language: line.language ?? "plain" };
        const run: string[] = [];
        for (let cursor = index; cursor < lines.length && lines[cursor].kind === "code"; cursor++)
          run.push(lines[cursor].spans.map((span) => span.text).join(""));
        blockSpans = highlightLines(run, block.language);
      }
      chunks.push(
        ...t`${bg(palette.panel)("  ")}`.chunks,
        ...spanChunks(blockSpans[index - block.start] ?? [], false, palette.panel),
        ...t`${bg(palette.panel)(" ")}`.chunks,
      );
      continue;
    }
    block = undefined;
    chunks.push(...renderMarkdownLine(line));
  }
  return chunks;
}

export function renderTraceActivity(
  activity: TraceActivity,
  detail?: ToolEventDetail,
  expanded = false,
): StyledText {
  if (activity.kind === "thought")
    return t`${italic(fg(palette.dim)(activity.text))}`;
  if (activity.kind === "tool") {
    const state = detail?.status ?? activity.toolStatus ?? "running";
    const glyph = state === "completed" ? "✓" : state === "failed" ? "×" : "◐";
    const color =
      state === "completed"
        ? palette.success
        : state === "failed"
          ? palette.danger
          : palette.warning;
    const durationMs = detail?.durationMs ?? activity.durationMs;
    const duration =
      durationMs === undefined ? "" : `  ${formatDuration(durationMs)}`;
    const subAgent = detail?.type === "subAgentActivity";
    const label = subAgent ? "agent" : (activity.label ?? "tool");
    const text = subAgent
      ? `${detail.activityKind ?? "activity"} ${detail.agentPath ?? detail.agentThreadId ?? "sub-agent"}`
      : activity.text;
    const caret = expanded ? "▾" : "▸";
    const exit =
      detail?.exitCode !== undefined && detail.exitCode !== 0
        ? `  exit ${detail.exitCode}`
        : "";
    return t`${fg(palette.dim)(caret)} ${fg(color)(glyph)} ${bold(fg(color)(label))}  ${fg(palette.text)(text)}${fg(palette.dim)(duration)}${fg(palette.danger)(exit)}`;
  }
  if (activity.kind === "message")
    return t`${fg(palette.accent)("›")} ${fg(palette.text)(activity.text)}`;
  if (activity.kind === "warning")
    return t`${fg(palette.warning)("!")} ${fg(palette.warning)(activity.text)}`;
  if (activity.kind === "error")
    return t`${fg(palette.danger)("×")} ${fg(palette.danger)(activity.text)}`;
  const label = activity.label ? `${activity.label} ` : "";
  return t`${fg(palette.dim)(`• ${label}${activity.text}`)}`;
}

export function renderToolDetail(
  detail: ToolEventDetail | undefined,
  activity: TraceActivity,
): StyledText {
  const rail = fg(palette.border)("  │ ");
  const chunks: StyledText["chunks"] = [];
  const line = (...parts: StyledText["chunks"][]) => {
    chunks.push(...t`\n${rail}`.chunks);
    for (const part of parts) chunks.push(...part);
  };
  const field = (label: string, value: string, color = palette.text) =>
    line(t`${fg(palette.dim)(label.padEnd(8))}${fg(color)(value)}`.chunks);
  if (!detail) {
    line(t`${italic(fg(palette.dim)("No additional tool detail was captured."))}`.chunks);
    return new StyledText(chunks);
  }
  const command = detail.command || detail.query || detail.toolName || activity.text;
  const status = `${detail.status}${detail.exitCode === undefined ? "" : ` · exit ${detail.exitCode}`}${
    detail.durationMs === undefined
      ? ""
      : ` · ${formatDuration(detail.durationMs)}`
  }`;
  field("command", command);
  field("status", status, sessionStatusColor(detail.status));
  if (detail.cwd) field("cwd", detail.cwd, palette.accent);
  if (detail.agentThreadId) field("thread", detail.agentThreadId, palette.accent);
  if (detail.input && Object.keys(detail.input).length > 0) {
    line(t`${fg(palette.dim)("input")}`.chunks);
    for (const inputLine of JSON.stringify(detail.input, null, 2).split("\n"))
      line(codeChunks(inputLine, "json", false));
  }
  if (detail.output) {
    const outputLines = detail.output.trimEnd().split("\n");
    const clipped = outputLines.length > TOOL_OUTPUT_LINES;
    line(
      t`${fg(palette.dim)(clipped ? `output · last ${TOOL_OUTPUT_LINES} of ${outputLines.length} lines` : "output")}`
        .chunks,
    );
    for (const outputLine of outputLines.slice(-TOOL_OUTPUT_LINES))
      line(t`${fg(palette.text)(outputLine)}`.chunks);
  }
  chunks.push(...t`\n${fg(palette.border)("  ╰")}`.chunks);
  return new StyledText(chunks);
}

export function activitySearchText(
  activity: TraceActivity,
  detail?: ToolEventDetail,
): string {
  return [
    activity.label,
    activity.text,
    activity.toolStatus,
    detail?.command,
    detail?.cwd,
    detail?.status,
    detail?.output,
    detail?.query,
    detail?.toolName,
    detail?.agentThreadId,
    detail?.agentPath,
    detail?.activityKind,
    detail?.input ? JSON.stringify(detail.input) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
}

export function activityCopyText(
  activity: TraceActivity,
  detail?: ToolEventDetail,
): string {
  if (!detail) return activity.text;
  return [
    detail.command || detail.query || activity.text,
    `status: ${detail.status}${detail.exitCode === undefined ? "" : ` (exit ${detail.exitCode})`}`,
    detail.cwd ? `cwd: ${detail.cwd}` : "",
    detail.agentThreadId ? `thread: ${detail.agentThreadId}` : "",
    detail.output || "",
  ]
    .filter(Boolean)
    .join("\n");
}

export function isLive(session: Session): boolean {
  return (
    session.status === "active" ||
    session.status === "idle" ||
    session.status === "starting"
  );
}

export function formatDuration(milliseconds: number): string {
  if (milliseconds < 1_000) return `${milliseconds}ms`;
  return `${(milliseconds / 1_000).toFixed(milliseconds < 10_000 ? 1 : 0)}s`;
}

export function renderUsage(session: Session | undefined): StyledText {
  if (!session) return t``;
  const chunks: StyledText["chunks"] = [];
  const meter = contextMeter(session.tokenUsage, 8);
  if (meter) {
    const color =
      meter.ratio >= 0.85
        ? palette.danger
        : meter.ratio >= 0.6
          ? palette.warning
          : palette.success;
    chunks.push(
      ...t`${fg(color)(renderMeter(meter))} ${fg(palette.text)(meter.label)}`.chunks,
    );
    const cost = session.tokenUsage?.costUsd;
    if (cost !== undefined)
      chunks.push(...t`${fg(palette.dim)(` · $${cost.toFixed(cost < 1 ? 3 : 2)}`)}`.chunks);
  } else {
    const usage = formatTokenUsage(session.tokenUsage);
    if (usage) chunks.push(...t`${fg(palette.text)(usage)}`.chunks);
  }
  if (chunks.length > 0) chunks.push(...t`${fg(palette.dim)(" · ")}`.chunks);
  chunks.push(
    ...t`${fg(sessionStatusColor(session.status))(session.status)}`.chunks,
  );
  return new StyledText(chunks);
}
