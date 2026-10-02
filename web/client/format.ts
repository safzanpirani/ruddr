// Browser-safe copies of the TUI's display formatting. tui/core.ts imports
// node:fs, so the client cannot bundle it directly.

export interface TokenUsage {
  inputTokens?: number;
  cachedInputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  contextTokens?: number;
  contextWindow?: number;
  costUsd?: number;
}

export interface Session {
  version: number;
  provider?: string;
  pid: number;
  status: string;
  threadId?: string;
  turnId?: string;
  model?: string;
  effort?: string;
  cwd?: string;
  sandbox?: string;
  stateDir: string;
  stateFile: string;
  steers?: number;
  idle?: boolean;
  turns?: number;
  tokenUsage?: TokenUsage;
  startedAt?: string;
  updatedAt?: string;
  completedAt?: string;
  error?: string;
}

export type PromptRoute = "steer" | "prompt" | "continue";

export function basename(path: string | undefined): string {
  if (!path) return "";
  const trimmed = path.replace(/[\\/]+$/, "");
  return trimmed.slice(Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\")) + 1);
}

export function projectName(session: Session): string {
  if (session.cwd) return basename(session.cwd);
  return basename(session.stateDir.replace(/[\\/](\.scratch[\\/].*)$/, "")) || session.stateDir;
}

export function isLive(session: Session): boolean {
  return session.status === "active" || session.status === "idle" || session.status === "starting";
}

export function isTerminal(status: string): boolean {
  return status === "completed" || status === "failed" || status === "interrupted";
}

export function promptRoute(session: Session | undefined): PromptRoute | undefined {
  if (!session) return undefined;
  if (session.status === "active") return session.turnId ? "steer" : undefined;
  if (session.status === "idle") return "prompt";
  if (isTerminal(session.status) && session.threadId && session.cwd) return "continue";
  return undefined;
}

export function statusGlyph(status: string): string {
  switch (status) {
    case "active":
      return "●";
    case "idle":
      return "◐";
    case "starting":
      return "◌";
    case "completed":
      return "✓";
    case "failed":
      return "✗";
    case "interrupted":
      return "■";
    case "stale":
      return "?";
    default:
      return "·";
  }
}

export function formatAge(timestamp: string | undefined, now = Date.now()): string {
  const time = Date.parse(timestamp ?? "");
  if (!Number.isFinite(time)) return "";
  const seconds = Math.max(0, Math.round((now - time) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 48) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

export function formatDuration(milliseconds: number | undefined): string {
  if (milliseconds === undefined || !Number.isFinite(milliseconds) || milliseconds <= 0) return "";
  const seconds = Math.round(milliseconds / 1000);
  if (seconds < 1) return `${Math.max(0, Math.round(milliseconds))}ms`;
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${String(seconds % 60).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

export function formatElapsed(startedAt: string | undefined, completedAt: string | undefined, now = Date.now()): string {
  const start = Date.parse(startedAt ?? "");
  if (!Number.isFinite(start)) return "";
  const end = Date.parse(completedAt ?? "");
  return formatDuration((Number.isFinite(end) ? end : now) - start);
}

export function formatTokens(value: number | undefined): string {
  if (!value) return "0";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return String(value);
}

export function shortId(id: string | undefined): string {
  return id ? id.slice(0, 8) : "—";
}

export function shortPath(path: string | undefined, home: string): string {
  if (!path) return "";
  return home && path.startsWith(home) ? `~${path.slice(home.length)}` : path;
}
