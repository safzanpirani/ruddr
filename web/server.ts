// `ruddr web`: the TUI's sessions dashboard served to a browser. It reuses the
// TUI's discovery, diff, prompt, and launch helpers, so both front ends route
// prompts and control runs the same way. Every /api route requires the
// per-machine token; mutations also require a same-origin custom header.
import { randomBytes, timingSafeEqual } from "node:crypto";
import { constants } from "node:fs";
import { mkdir, open, readdir, realpath, stat } from "node:fs/promises";
import { homedir, hostname } from "node:os";
import { dirname, join, resolve } from "node:path";
import {
  attachToolDetails,
  continuationRunArguments,
  deleteSessionArtifacts,
  discoverSessions,
  FALLBACK_MODELS,
  idlePromptControlArguments,
  newSessionRunArguments,
  parseDejaHits,
  parseModelCatalog,
  parseToolEventDetails,
  parseTraceActivities,
  promptModeForSession,
  type Session,
  steerControlArguments,
} from "../tui/core";
import { readWorkspaceDiff, touchedSince } from "../tui/git";
import { errorMessage, runControl } from "../tui/process";
import { sendControlPrompt } from "../tui/prompt";
import { launchSession } from "../tui/session-launch";
import { defaultThemeName, findTheme, persistTheme, readTUIConfig, themes } from "../tui/themes";

export interface WebArguments {
  ruddr: string;
  host: string;
  port: number;
  roots: string[];
  stateDirs: string[];
  interval: number;
  open: boolean;
  tokenFile?: string;
  updateAvailable?: string;
}

const PROVIDERS = ["codex", "claude", "opencode", "pi", "droid"] as const;
const EVENTS_INITIAL_BYTES = 6 * 1024 * 1024;
const COOKIE = "ruddr_web";
export class WebUsageError extends Error {}

export function parseWebArguments(argv: string[], environment = process.env): WebArguments {
  const args: WebArguments = {
    ruddr: "ruddr",
    host: environment.RUDDR_WEB_HOST || "127.0.0.1",
    port: Number(environment.RUDDR_WEB_PORT || 4519),
    roots: [],
    stateDirs: [],
    interval: 1000,
    open: false,
    updateAvailable: environment.RUDDR_UPDATE_AVAILABLE || undefined,
  };
  for (let index = 0; index < argv.length; index++) {
    const flag = argv[index];
    const value = () => {
      const next = argv[++index];
      if (next === undefined || next.startsWith("--")) throw new WebUsageError(`${flag} requires a value`);
      return next;
    };
    switch (flag) {
      case "--ruddr":
        args.ruddr = value();
        break;
      case "--host":
        args.host = value();
        break;
      case "--port": {
        const port = Number(value());
        if (!Number.isInteger(port) || port < 0 || port > 65535) throw new WebUsageError("--port must be 0-65535");
        args.port = port;
        break;
      }
      case "--root":
        args.roots.push(resolve(value()));
        break;
      case "--state-dir":
        args.stateDirs.push(resolve(value()));
        break;
      case "--interval": {
        const raw = value();
        const match = /^(\d+(?:\.\d+)?)(ms|s)$/.exec(raw);
        if (!match) throw new WebUsageError("--interval must look like 500ms or 2s");
        args.interval = Number(match[1]) * (match[2] === "s" ? 1000 : 1);
        if (!Number.isFinite(args.interval) || args.interval < 100) throw new WebUsageError("--interval must be at least 100ms");
        break;
      }
      case "--token-file":
        args.tokenFile = resolve(value());
        break;
      case "--open":
        args.open = true;
        break;
      default:
        throw new WebUsageError(`unknown web flag ${flag}`);
    }
  }
  if (args.roots.length === 0) args.roots.push(join(process.cwd(), ".scratch"));
  if (!Number.isInteger(args.port) || args.port < 0 || args.port > 65535) throw new WebUsageError("web port must be 0-65535");
  return args;
}

export function defaultTokenFile(environment = process.env): string {
  const configHome = environment.XDG_CONFIG_HOME || join(homedir(), ".config");
  return join(configHome, "ruddr", "web-token");
}

/** Reads the stable access token, creating it on first use. */
export async function loadToken(file: string): Promise<string> {
  await mkdir(dirname(file), { recursive: true, mode: 0o700 });
  let handle;
  try {
    handle = await open(file, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL, 0o600);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
  }
  if (handle) {
    try {
      const token = randomBytes(24).toString("base64url");
      await handle.writeFile(`${token}\n`);
      return token;
    } finally {
      await handle.close();
    }
  }
  const existing = await open(file, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    if (!(await existing.stat()).isFile()) throw new Error("The web token must be a regular file");
    await existing.chmod(0o600);
    // Another server may have exclusively created the file and still be writing it.
    for (let attempt = 0; attempt < 20; attempt++) {
      const buffer = Buffer.alloc(4096);
      const { bytesRead } = await existing.read(buffer, 0, buffer.length, 0);
      const token = buffer.subarray(0, bytesRead).toString("utf8").trim();
      if (/^[A-Za-z0-9_-]{32,256}$/.test(token)) return token;
      if (bytesRead !== 0) break;
      await Bun.sleep(10);
    }
    throw new Error("The existing web token is invalid; replace the token file explicitly");
  } finally {
    await existing.close();
  }
}

function tokenMatches(expected: string, candidate: string | undefined | null): boolean {
  if (!candidate) return false;
  const left = Buffer.from(expected);
  const right = Buffer.from(candidate);
  return left.length === right.length && timingSafeEqual(left, right);
}

function cookieValue(request: Request, name: string): string | undefined {
  const header = request.headers.get("cookie");
  if (!header) return undefined;
  for (const part of header.split(";")) {
    const [key, ...rest] = part.trim().split("=");
    if (key === name) {
      try { return decodeURIComponent(rest.join("=")); } catch { return undefined; }
    }
  }
  return undefined;
}

export function isAuthorized(request: Request, token: string): boolean {
  const bearer = request.headers.get("authorization")?.replace(/^Bearer\s+/i, "");
  return tokenMatches(token, cookieValue(request, COOKIE)) || tokenMatches(token, bearer);
}

/**
 * Mutations need the custom header, which a cross-site form cannot send, and
 * an Origin, when present, that matches the Host the browser used.
 */
export function isSameOriginMutation(request: Request): boolean {
  if (request.headers.get("x-ruddr-request") !== "1") return false;
  const origin = request.headers.get("origin");
  if (!origin) return true;
  try {
    const target = new URL(request.url);
    return new URL(origin).origin === target.origin && target.host === request.headers.get("host");
  } catch {
    return false;
  }
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" },
  });
}

function failure(message: string, status = 400): Response {
  return json({ error: message }, status);
}

/** A bounded SSE queue closes slow readers; EventSource reconnects with a reset. */
export function eventStream(
  request: Request,
  start: (send: (event: string, data: unknown) => void, close: () => void) => () => void,
): Response {
  let cleanup: (() => void) | undefined;
  let heartbeat: ReturnType<typeof setInterval> | undefined;
  let closed = false;
  let close = () => {};
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      const encoder = new TextEncoder();
      close = () => {
        if (closed) return;
        closed = true;
        clearInterval(heartbeat);
        request.signal.removeEventListener("abort", close);
        cleanup?.();
        try { controller.close(); } catch { /* Already canceled. */ }
      };
      const enqueue = (text: string) => {
        if (closed) return;
        const bytes = encoder.encode(text);
        if (bytes.byteLength > (controller.desiredSize ?? 0)) { close(); return; }
        try { controller.enqueue(bytes); } catch { close(); }
      };
      const send = (event: string, data: unknown) => enqueue(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
      request.signal.addEventListener("abort", close);
      if (request.signal.aborted) { close(); return; }
      enqueue("retry: 1500\n\n");
      heartbeat = setInterval(() => enqueue(": ping\n\n"), 15_000);
      cleanup = start(send, close);
      // A synchronous producer can close before returning its cleanup.
      if (closed) cleanup();
    },
    cancel() { close(); },
  }, { highWaterMark: 48 * 1024 * 1024, size: (chunk) => chunk?.byteLength ?? 0 });
  return new Response(stream, {
    headers: {
      "content-type": "text/event-stream; charset=utf-8",
      "cache-control": "no-store",
      "x-accel-buffering": "no",
      connection: "keep-alive",
    },
  });
}

/** Align bytes before decoding UTF-8, and retain the unfinished record as bytes. */
export async function readAlignedTail(path: string, maxBytes: number): Promise<{
  text: string; offset: number; truncated: boolean; pending: Buffer; skipping: boolean; identity: string;
}> {
  const handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const info = await handle.stat();
    if (!info.isFile()) throw new Error("The event log must be a regular file");
    const start = Math.max(0, info.size - maxBytes);
    const buffer = Buffer.alloc(info.size - start);
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, start);
    const bytes = buffer.subarray(0, bytesRead);
    const preceding = Buffer.alloc(1);
    if (start > 0) await handle.read(preceding, 0, 1, start - 1);
    const partialStart = start > 0 && preceding[0] !== 0x0a;
    const first = partialStart ? bytes.indexOf(0x0a) : -1;
    const skipping = partialStart && first < 0;
    const aligned = skipping ? bytes.subarray(bytes.length) : bytes.subarray(first + 1);
    const last = aligned.lastIndexOf(0x0a);
    return {
      text: aligned.subarray(0, last + 1).toString("utf8"),
      offset: start + bytesRead,
      truncated: start > 0,
      pending: aligned.subarray(last + 1),
      skipping,
      identity: `${info.ino}:${info.dev}`,
    };
  } finally { await handle.close(); }
}

export async function readRange(path: string, from: number, to: number, identity: string): Promise<Buffer | undefined> {
  const handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const info = await handle.stat();
    if (!info.isFile()) throw new Error("The event log must be a regular file");
    if (`${info.ino}:${info.dev}` !== identity || info.size < from) return undefined;
    const buffer = Buffer.alloc(to - from);
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, from);
    return buffer.subarray(0, bytesRead);
  } finally { await handle.close(); }
}

/** Retain only bounded partial records and skip oversized complete records too. */
export function consumeEventBytes(pending: Buffer, skipping: boolean, chunk: Buffer, maxBytes: number): {
  text: string; pending: Buffer; skipping: boolean; oversized: boolean;
} {
  if (skipping) {
    const first = chunk.indexOf(0x0a);
    if (first < 0) return { text: "", pending: Buffer.alloc(0), skipping: true, oversized: false };
    chunk = chunk.subarray(first + 1);
  }
  const combined = Buffer.concat([pending, chunk]);
  const records: Buffer[] = [];
  let oversized = false;
  let start = 0;
  let keptStart = 0;
  for (let newline = combined.indexOf(0x0a); newline >= 0; newline = combined.indexOf(0x0a, start)) {
    const end = newline + 1;
    if (end - start > maxBytes) {
      if (start > keptStart) records.push(combined.subarray(keptStart, start));
      keptStart = end;
      oversized = true;
    }
    start = end;
  }
  if (start > keptStart) records.push(combined.subarray(keptStart, start));
  pending = Buffer.from(combined.subarray(start));
  skipping = pending.length > maxBytes;
  if (skipping) { pending = Buffer.alloc(0); oversized = true; }
  return { text: Buffer.concat(records).toString("utf8"), pending, skipping, oversized };
}

async function readArtifactTail(path: string, maxBytes: number): Promise<string> {
  try {
    const handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    try {
      const info = await handle.stat();
      if (!info.isFile()) throw new Error("The run artifact must be a regular file");
      const { size } = info;
      const start = Math.max(0, size - maxBytes);
      const buffer = Buffer.alloc(size - start);
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, start);
      return buffer.subarray(0, bytesRead).toString("utf8");
    } finally { await handle.close(); }
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return "";
    throw error;
  }
}

async function gitBranch(cwd: string): Promise<string | undefined> {
  try {
    const child = Bun.spawn(["git", "-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"], { stdout: "pipe", stderr: "ignore" });
    const [name, code] = await Promise.all([new Response(child.stdout).text(), child.exited]);
    return code === 0 && name.trim() ? name.trim() : undefined;
  } catch {
    return undefined;
  }
}

async function untrackedFiles(cwd: string): Promise<string[]> {
  try {
    const child = Bun.spawn(["git", "-C", cwd, "ls-files", "--others", "--exclude-standard", "-z"], { stdout: "pipe", stderr: "ignore" });
    const [out, code] = await Promise.all([new Response(child.stdout).text(), child.exited]);
    return code === 0 ? out.split("\0").filter(Boolean).slice(0, 500) : [];
  } catch {
    return [];
  }
}

export class WebApp {
  sessions: Session[] = [];
  private sessionsJSON = "[]";
  private sessionListeners = new Set<(sessions: Session[]) => void>();
  private pollTimer?: ReturnType<typeof setInterval>;
  private branches = new Map<string, { name?: string; readAt: number }>();
  private static?: Map<string, { body: Uint8Array; type: string }>;
  private indexPath = "/index.html";
  private verifiedDirectories = new Map<string, { path: string; identity: string }>();

  constructor(
    readonly args: WebArguments,
    readonly token: string,
  ) {}

  async refreshSessions(): Promise<void> {
    const discovered = await discoverSessions({ roots: this.args.roots, stateDirs: this.args.stateDirs });
    const sessions = (await Promise.all(discovered.map(async (session) => {
      // Discovery must authorize the directory containing state.json, not a path supplied by that JSON.
      if (resolve(session.stateDir) !== dirname(resolve(session.stateFile))) return undefined;
      try {
        const path = await realpath(session.stateDir);
        const handle = await open(session.stateDir, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
        let identity: string;
        try {
          const info = await handle.stat();
          if (!info.isDirectory()) return undefined;
          identity = `${info.ino}:${info.dev}`;
        } finally { await handle.close(); }
        if (await realpath(session.stateFile) !== join(path, "state.json")) return undefined;
        const key = resolve(session.stateDir);
        const previous = this.verifiedDirectories.get(key);
        if (previous && (previous.path !== path || previous.identity !== identity)) return undefined;
        this.verifiedDirectories.set(key, { path, identity });
        return session;
      } catch { return undefined; }
    }))).filter((session): session is Session => session !== undefined);
    const serialized = JSON.stringify(sessions);
    this.sessions = sessions;
    if (serialized === this.sessionsJSON) return;
    this.sessionsJSON = serialized;
    for (const listener of this.sessionListeners) listener(sessions);
  }

  private ensurePolling(): void {
    if (this.pollTimer) return;
    this.pollTimer = setInterval(() => {
      void this.refreshSessions().catch(() => {});
    }, this.args.interval);
  }

  private stopPollingIfIdle(): void {
    if (this.sessionListeners.size === 0 && this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = undefined;
    }
  }

  /** Only directories of discovered sessions may be read or controlled. */
  sessionFor(stateDir: string | null | undefined): Session | undefined {
    if (!stateDir) return undefined;
    const wanted = resolve(stateDir);
    return this.sessions.find((session) => resolve(session.stateDir) === wanted);
  }

  private async knownSession(stateDir: string | null | undefined): Promise<Session | undefined> {
    let session = this.sessionFor(stateDir);
    if (!session) {
      await this.refreshSessions();
      session = this.sessionFor(stateDir);
    }
    if (session) {
      try { await this.verifyDirectory(session); } catch { return undefined; }
    }
    return session;
  }

  private async verifyDirectory(session: Session): Promise<string> {
    const verified = this.verifiedDirectories.get(resolve(session.stateDir));
    if (!verified || await realpath(session.stateDir) !== verified.path)
      throw new Error("The verified session directory changed");
    const handle = await open(verified.path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    try {
      const info = await handle.stat();
      if (!info.isDirectory() || `${info.ino}:${info.dev}` !== verified.identity)
        throw new Error("The verified session directory changed");
    } finally { await handle.close(); }
    return verified.path;
  }

  async branchFor(cwd: string): Promise<string | undefined> {
    const cached = this.branches.get(cwd);
    if (cached && Date.now() - cached.readAt < 10_000) return cached.name;
    const name = await gitBranch(cwd);
    this.branches.set(cwd, { name, readAt: Date.now() });
    return name;
  }

  async buildClient(): Promise<void> {
    const result = await Bun.build({
      entrypoints: [join(import.meta.dir, "index.html")],
      minify: true,
      splitting: true,
      target: "browser",
      naming: { entry: "[name].[ext]", chunk: "chunks/[name]-[hash].[ext]", asset: "assets/[name]-[hash].[ext]" },
    });
    if (!result.success) throw new Error(result.logs.map((log) => String(log)).join("\n") || "web client build failed");
    const files = new Map<string, { body: Uint8Array; type: string }>();
    for (const output of result.outputs) {
      const path = `/${output.path.replace(/^\.\//, "")}`;
      files.set(path, { body: new Uint8Array(await output.arrayBuffer()), type: output.type || "application/octet-stream" });
      if (output.kind === "entry-point" && path.endsWith(".html")) this.indexPath = path;
    }
    this.static = files;
  }

  private serveStatic(pathname: string): Response | undefined {
    const files = this.static;
    if (!files) return undefined;
    const file = files.get(pathname) ?? (pathname === "/" || !pathname.includes(".") ? files.get(this.indexPath) : undefined);
    if (!file) return undefined;
    const immutable = /\/(chunks|assets)\//.test(pathname);
    return new Response(file.body as BodyInit, {
      headers: {
        "content-type": file.type,
        "cache-control": immutable ? "public, max-age=31536000, immutable" : "no-cache",
        "x-content-type-options": "nosniff",
        "referrer-policy": "no-referrer",
        ...(file.type.startsWith("text/html")
          ? {
              "content-security-policy":
                "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            }
          : {}),
      },
    });
  }

  async fetch(request: Request): Promise<Response> {
    // TODO(review): Define allowed Host aliases for wildcard binds and reverse proxies before adding a DNS-rebinding Host allowlist.
    const url = new URL(request.url);
    const { pathname } = url;
    // Opening the printed link trades the query token for an HttpOnly cookie.
    const queryToken = url.searchParams.get("token");
    if (queryToken !== null && !pathname.startsWith("/api/")) {
      if (!tokenMatches(this.token, queryToken)) return new Response("Invalid Ruddr web token", { status: 401 });
      const secure = url.protocol === "https:" ? "; Secure" : "";
      return new Response(null, {
        status: 303,
        headers: {
          location: "/",
          "set-cookie": `${COOKIE}=${encodeURIComponent(this.token)}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000${secure}`,
          "cache-control": "no-store",
        },
      });
    }
    if (!pathname.startsWith("/api/")) return this.serveStatic(pathname) ?? new Response("Not found", { status: 404 });
    if (pathname === "/api/login" && request.method === "POST") {
      if (!isSameOriginMutation(request)) return failure("Cross-origin request rejected", 403);
      const body = (await request.json().catch(() => ({}))) as { token?: unknown } | null;
      if (!tokenMatches(this.token, typeof body?.token === "string" ? body.token.trim() : undefined)) return failure("That token is not valid", 401);
      const secure = url.protocol === "https:" ? "; Secure" : "";
      return new Response(JSON.stringify({ ok: true }), {
        headers: {
          "content-type": "application/json",
          "cache-control": "no-store",
          "set-cookie": `${COOKIE}=${encodeURIComponent(this.token)}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000${secure}`,
        },
      });
    }
    if (!isAuthorized(request, this.token)) return failure("Unauthorized", 401);
    if (request.method !== "GET" && !isSameOriginMutation(request)) return failure("Cross-origin request rejected", 403);
    try {
      return await this.route(request, url);
    } catch (error) {
      return failure(errorMessage(error), 500);
    }
  }

  private async route(request: Request, url: URL): Promise<Response> {
    const { pathname } = url;
    const method = request.method;
    const body = async <T>() => (await request.json().catch(() => ({}))) as T;
    if (method === "GET" && pathname === "/api/meta") return this.meta();
    if (method === "GET" && pathname === "/api/sessions") {
      await this.refreshSessions();
      return json(this.sessions);
    }
    if (method === "GET" && pathname === "/api/sessions/stream") return this.sessionStream(request);
    if (method === "GET" && pathname === "/api/run/events") return this.runEventStream(request, url.searchParams.get("dir"));
    if (method === "GET" && pathname === "/api/run/output") return this.runOutput(url.searchParams.get("dir"));
    if (method === "GET" && pathname === "/api/run/activity") return this.runActivity(url.searchParams.get("dir"));
    if (method === "GET" && pathname === "/api/run/diff") return this.runDiff(url.searchParams.get("dir"), url.searchParams.has("force"));
    if (method === "GET" && pathname === "/api/models") return this.models();
    if (method === "GET" && pathname === "/api/deja") return this.deja(url.searchParams.get("q") ?? "");
    if (method === "GET" && pathname === "/api/dirs") return this.directories(url.searchParams.get("path") ?? "");
    if (method === "POST" && pathname === "/api/prompt") return this.prompt(await body());
    if (method === "POST" && pathname === "/api/new") return this.newSession(await body());
    if (method === "POST" && pathname === "/api/stop") return this.stop(await body());
    if (method === "POST" && pathname === "/api/delete") return this.delete(await body());
    if (method === "POST" && pathname === "/api/theme") return this.theme(await body());
    if (method === "POST" && pathname === "/api/update") return this.update();
    return failure("Not found", 404);
  }

  private async meta(): Promise<Response> {
    const config = await readTUIConfig().catch(() => ({}) as { theme?: string });
    return json({
      hostname: hostname(),
      cwd: process.cwd(),
      home: homedir(),
      providers: PROVIDERS,
      theme: findTheme(config.theme)?.name ?? defaultThemeName,
      themes: themes.map((theme) => ({ name: theme.name, label: theme.label, source: theme.source, palette: theme.palette })),
      dejaAvailable: Boolean(Bun.which("deja")),
      updateAvailable: this.args.updateAvailable,
    });
  }

  private sessionStream(request: Request): Response {
    return eventStream(request, (send) => {
      const listener = (sessions: Session[]) => send("sessions", sessions);
      this.sessionListeners.add(listener);
      this.ensurePolling();
      void this.refreshSessions()
        .catch(() => {})
        .finally(() => send("sessions", this.sessions));
      return () => {
        this.sessionListeners.delete(listener);
        this.stopPollingIfIdle();
      };
    });
  }

  private async runEventStream(request: Request, stateDir: string | null): Promise<Response> {
    const session = await this.knownSession(stateDir);
    if (!session) return failure("Unknown session", 404);
    const eventsPath = join(await this.verifyDirectory(session), "events.jsonl");
    return eventStream(request, (send, close) => {
      let offset = 0;
      let identity = "";
      let pending: Buffer = Buffer.alloc(0);
      let skipping = false;
      let stopped = false;
      let reading = false;
      // TODO(review): Share one event-log reader per session if concurrent dashboard clients make per-client polling costly.
      const tick = async () => {
        if (stopped || reading) return;
        reading = true;
        try {
          try { await this.verifyDirectory(session); } catch { close(); return; }
          let info;
          try {
            info = await stat(eventsPath);
          } catch {
            if (offset === 0 && !identity) {
              identity = "missing";
              send("reset", { text: "", truncated: false });
            }
            return;
          }
          const nextIdentity = `${info.ino}:${info.dev}`;
          if (nextIdentity !== identity || info.size < offset) {
            const tail = await readAlignedTail(eventsPath, EVENTS_INITIAL_BYTES);
            try { await this.verifyDirectory(session); } catch { close(); return; }
            identity = tail.identity;
            offset = tail.offset;
            pending = tail.pending;
            skipping = tail.skipping;
            send("reset", { text: tail.text, truncated: tail.truncated });
            return;
          }
          if (info.size === offset) return;
          const chunk = await readRange(eventsPath, offset, Math.min(info.size, offset + 4 * 1024 * 1024), identity);
          if (!chunk) { identity = ""; return; }
          try { await this.verifyDirectory(session); } catch { close(); return; }
          offset += chunk.length;
          const next = consumeEventBytes(pending, skipping, chunk, EVENTS_INITIAL_BYTES);
          pending = next.pending;
          skipping = next.skipping;
          if (next.text) send("append", { text: next.text });
          if (next.oversized) {
            send("problem", { error: "An event exceeded the 6 MiB record limit and was skipped" });
          }
        } catch (error) {
          send("problem", { error: errorMessage(error) });
        } finally {
          reading = false;
        }
      };
      void tick();
      const timer = setInterval(() => void tick(), 150);
      return () => {
        stopped = true;
        clearInterval(timer);
      };
    });
  }

  private async runOutput(stateDir: string | null): Promise<Response> {
    const session = await this.knownSession(stateDir);
    if (!session) return failure("Unknown session", 404);
    const text = await readArtifactTail(join(await this.verifyDirectory(session), "output.md"), 1024 * 1024);
    await this.verifyDirectory(session);
    return json({ text });
  }

  private async runActivity(stateDir: string | null): Promise<Response> {
    const session = await this.knownSession(stateDir);
    if (!session) return failure("Unknown session", 404);
    const directory = await this.verifyDirectory(session);
    const [trace, events] = await Promise.all([
      readArtifactTail(join(directory, "trace.log"), 512 * 1024),
      readArtifactTail(join(directory, "events.jsonl"), 2 * 1024 * 1024),
    ]);
    await this.verifyDirectory(session);
    const activities = parseTraceActivities(trace);
    const details = attachToolDetails(activities, parseToolEventDetails(events));
    return json({ activities: activities.map((activity, index) => ({ ...activity, detail: details[index] })) });
  }

  private async runDiff(stateDir: string | null, force: boolean): Promise<Response> {
    const session = await this.knownSession(stateDir);
    if (!session) return failure("Unknown session", 404);
    if (!session.cwd) return json({ content: "", error: "This session has no working directory." });
    const [result, branch, untracked] = await Promise.all([
      readWorkspaceDiff(session.cwd, force),
      this.branchFor(session.cwd),
      untrackedFiles(session.cwd),
    ]);
    const paths = new Set<string>();
    for (const match of result.content.matchAll(/^diff --git a\/.+? b\/(.+)$/gm)) paths.add(match[1]);
    for (const path of untracked) paths.add(path);
    const touched = await touchedSince(session.cwd, paths, session.startedAt);
    await this.verifyDirectory(session);
    return json({ ...result, branch, cwd: session.cwd, untracked, touched: [...touched].sort() });
  }

  private async models(): Promise<Response> {
    try {
      return json(parseModelCatalog(await runControl(this.args.ruddr, ["models", "--json"])));
    } catch {
      return json(FALLBACK_MODELS);
    }
  }

  private async deja(query: string): Promise<Response> {
    const terms = query.trim();
    if (!terms) return json([]);
    if (!Bun.which("deja")) return failure("deja is not on PATH", 404);
    const child = Bun.spawn(["deja", "find", ...terms.split(/\s+/), "--json", "--quiet"], { stdout: "pipe", stderr: "pipe" });
    const [stdout, code] = await Promise.all([new Response(child.stdout).text(), child.exited]);
    if (code !== 0) return failure(`deja find exited ${code}`, 500);
    return json(parseDejaHits(stdout));
  }

  /** Directory completion for the new-session working directory field. */
  private async directories(raw: string): Promise<Response> {
    const home = homedir();
    const expanded = raw.startsWith("~") ? join(home, raw.slice(1)) : raw || process.cwd();
    const absolute = resolve(expanded);
    let base = absolute;
    let prefix = "";
    try {
      if (!(await stat(absolute)).isDirectory()) throw new Error("not a directory");
    } catch {
      base = dirname(absolute);
      prefix = absolute.slice(base.length + 1);
    }
    try {
      const entries = await readdir(base, { withFileTypes: true });
      const names = entries
        .filter((entry) => entry.isDirectory() && !entry.name.startsWith(".") && entry.name.startsWith(prefix))
        .map((entry) => join(base, entry.name))
        .sort()
        .slice(0, 40);
      return json({ base, entries: names });
    } catch {
      return json({ base, entries: [] });
    }
  }

  private async prompt(input: { stateDir?: string; route?: string; turnId?: string; message?: string; model?: string }): Promise<Response> {
    const message = input.message?.trim();
    if (!message) return failure("The prompt is empty");
    await this.refreshSessions();
    const session = await this.knownSession(input.stateDir);
    if (!session) return failure("The prompt session is no longer available; the prompt was not sent", 409);
    const route = promptModeForSession(session);
    // Never convert one route into another: a stale page must not turn a
    // steer into a new turn or a continuation.
    if (!route || route !== input.route)
      return failure(`Session is now ${session.status}; the prompt was not sent`, 409);
    if (route === "steer" && (!session.turnId || session.turnId !== input.turnId))
      return failure("The active turn changed; the prompt was not sent", 409);
    if (route === "continue") {
      const overrides = input.model ? { model: input.model } : {};
      const stateDirectory = await launchSession({
        ruddr: this.args.ruddr,
        cwd: session.cwd!,
        message,
        argumentsForFiles: (promptFile, stateDirectory) => continuationRunArguments(session, promptFile, stateDirectory, overrides),
        onSpawn: (directory) => this.args.stateDirs.push(directory),
      });
      await this.refreshSessions();
      return json({ status: `Started a new run for thread ${session.threadId!.slice(0, 12)}`, stateDir: stateDirectory });
    }
    const result = await sendControlPrompt(this.args.ruddr, message, (file) =>
      route === "steer"
        ? steerControlArguments(session.stateDir, session.turnId!, file)
        : idlePromptControlArguments(session.stateDir, file),
    );
    await this.refreshSessions();
    return json({ status: result || (route === "steer" ? "Steer accepted" : "Prompt accepted") });
  }

  private async newSession(input: {
    provider?: string;
    model?: string;
    effort?: string;
    cwd?: string;
    message?: string;
    resumeThreadId?: string;
  }): Promise<Response> {
    const message = input.message?.trim();
    if (!message) return failure("The first prompt is empty");
    const provider = input.provider ?? "codex";
    if (!PROVIDERS.includes(provider as (typeof PROVIDERS)[number])) return failure(`Unknown provider ${provider}`);
    const rawCwd = input.cwd?.trim() || process.cwd();
    const cwd = resolve(rawCwd.startsWith("~") ? join(homedir(), rawCwd.slice(1)) : rawCwd);
    try {
      if (!(await stat(cwd)).isDirectory()) return failure(`${cwd} is not a directory`);
    } catch {
      return failure(`${cwd} does not exist`);
    }
    const stateDirectory = await launchSession({
      ruddr: this.args.ruddr,
      cwd,
      message,
      argumentsForFiles: (promptFile, stateDirectory) =>
        newSessionRunArguments({
          provider,
          model: input.model || undefined,
          effort: input.effort || undefined,
          cwd,
          promptFile,
          stateDirectory,
          ...(input.resumeThreadId ? { resumeThreadId: input.resumeThreadId } : {}),
        }),
      onSpawn: (directory) => this.args.stateDirs.push(directory),
    });
    await this.refreshSessions();
    return json({ status: `Started ${provider} session`, stateDir: stateDirectory });
  }

  private async stop(input: { stateDir?: string }): Promise<Response> {
    await this.refreshSessions();
    const session = await this.knownSession(input.stateDir);
    if (!session || (session.status !== "active" && session.status !== "idle"))
      return failure("Only an active or idle session can be stopped", 409);
    const idle = session.status === "idle";
    const result = await runControl(this.args.ruddr, [idle ? "stop" : "interrupt", "--state-dir", session.stateDir]);
    await this.refreshSessions();
    return json({ status: result || (idle ? "Shutdown requested" : "Interrupt requested") });
  }

  private async delete(input: { stateDir?: string }): Promise<Response> {
    await this.refreshSessions();
    const session = await this.knownSession(input.stateDir);
    if (!session) return failure("Unknown session", 404);
    const result = await deleteSessionArtifacts(session);
    this.args.stateDirs = this.args.stateDirs.filter((dir) => resolve(dir) !== resolve(session.stateDir));
    await this.refreshSessions();
    return json({ status: result.removedStateDir ? "Session deleted" : "Removed the session from the registry" });
  }

  private async theme(input: { name?: string }): Promise<Response> {
    if (!input.name || !findTheme(input.name)) return failure("Unknown theme");
    await persistTheme(input.name);
    return json({ status: `Theme ${findTheme(input.name)!.label} saved` });
  }

  private async update(): Promise<Response> {
    if (!this.args.updateAvailable) return failure("No newer release was found on the last daily check", 409);
    await runControl(this.args.ruddr, ["update"]);
    const target = this.args.updateAvailable;
    this.args.updateAvailable = undefined;
    return json({ status: `Ruddr ${target} installed; restart ruddr web to use it` });
  }
}

function isLoopback(host: string): boolean {
  return host === "127.0.0.1" || host === "::1" || host === "localhost";
}

export async function main(argv = process.argv.slice(2)): Promise<void> {
  const args = parseWebArguments(argv);
  const token = await loadToken(args.tokenFile ?? defaultTokenFile());
  const app = new WebApp(args, token);
  await app.buildClient();
  await app.refreshSessions();
  const server = Bun.serve({
    hostname: args.host,
    port: args.port,
    idleTimeout: 0,
    fetch: (request) => app.fetch(request),
  });
  const shownHost = args.host === "0.0.0.0" || args.host === "::" ? hostname() : args.host;
  const url = `http://${shownHost.includes(":") ? `[${shownHost}]` : shownHost}:${server.port}/?token=${token}`;
  console.log(`Ruddr web is serving ${app.sessions.length} sessions`);
  console.log(`Open ${url}`);
  if (!isLoopback(args.host))
    console.log("Warning: this address is reachable from other machines. Anyone with the token can steer your agents. Prefer a Tailscale address.");
  if (args.open) {
    const opener = process.platform === "darwin" ? "open" : process.platform === "win32" ? "explorer" : "xdg-open";
    try {
      Bun.spawn([opener, url], { stdout: "ignore", stderr: "ignore" }).unref();
    } catch {
      // The printed link is enough.
    }
  }
  const shutdown = () => {
    server.stop(true);
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

if (import.meta.main) {
  main().catch((error) => {
    console.error(`ruddr web: ${errorMessage(error)}`);
    process.exit(error instanceof WebUsageError ? 2 : 1);
  });
}
