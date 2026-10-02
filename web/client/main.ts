import { ansiNodes } from "./ansi";
import { ApiError, get, post } from "./api";
import { Suggestions, ThemedSelect } from "./select";
import { ChatView, markdownHTML, wireCopyButtons } from "./chat";
import { type DiffPreferences, type WorkspaceDiffData, WorkspaceDiffView } from "./diffs";
import { append, clear, copyText, h, transition } from "./dom";
import { draftTarget, type DraftTarget } from "./prompt";
import {
  basename,
  formatAge,
  formatDuration,
  formatElapsed,
  formatTokens,
  isLive,
  isTerminal,
  projectName,
  promptRoute,
  type Session,
  shortId,
  shortPath,
  statusGlyph,
} from "./format";

// ---------------------------------------------------------------------------
// Types and state

type Tab = "chat" | "activity" | "output" | "diff";
const TABS: Tab[] = ["chat", "activity", "output", "diff"];
const TAB_LABELS: Record<Tab, string> = { chat: "Chat", activity: "Activity", output: "Output", diff: "Diff" };

interface Palette {
  background: string;
  panel: string;
  border: string;
  text: string;
  dim: string;
  accent: string;
  selected: string;
  danger: string;
  success: string;
  warning: string;
}

interface Meta {
  hostname: string;
  cwd: string;
  home: string;
  providers: string[];
  theme: string;
  themes: Array<{ name: string; label: string; source: string; palette: Palette }>;
  dejaAvailable: boolean;
  updateAvailable?: string;
}

interface ModelInfo {
  provider: string;
  id?: string;
  label?: string;
  efforts?: string[];
  default?: boolean;
  available?: boolean;
}

interface Activity {
  timestamp: string;
  kind: string;
  text: string;
  label?: string;
  toolStatus?: string;
  durationMs?: number;
  detail?: {
    command?: string;
    cwd?: string;
    status: string;
    output?: string;
    exitCode?: number;
    durationMs?: number;
    input?: Record<string, unknown>;
    agentThreadId?: string;
  };
}

interface DejaHit {
  provider: string;
  sessionId: string;
  project: string;
  date: string;
  openingPrompt: string;
}

type ToastKind = "info" | "success" | "warning" | "error";

const storage = {
  get<T>(key: string, fallback: T): T {
    try {
      const raw = localStorage.getItem(`ruddr.${key}`);
      return raw === null ? fallback : (JSON.parse(raw) as T);
    } catch {
      return fallback;
    }
  },
  set(key: string, value: unknown): void {
    try {
      localStorage.setItem(`ruddr.${key}`, JSON.stringify(value));
    } catch {
      // Private windows and blocked storage only lose conveniences.
    }
  },
};

const state = {
  meta: undefined as Meta | undefined,
  sessions: [] as Session[],
  selected: storage.get<string | undefined>("selected", undefined),
  tab: storage.get<Tab>("tab", "chat"),
  filter: "",
  details: false,
  interruptArmedUntil: 0,
  connected: false,
  models: [] as ModelInfo[],
  continueModel: undefined as string | undefined,
  mobileView: "list" as "list" | "session",
  busy: false,
};

const diffPreferences: DiffPreferences = {
  style: storage.get<"split" | "unified">("diffStyle", "unified"),
  wrap: storage.get("diffWrap", true),
  themeType: "dark",
};

// ---------------------------------------------------------------------------
// Layout

const root = document.getElementById("app")!;
const toasts = h("div", { class: "toasts", "aria-live": "polite" });
const connection = h("span", { class: "conn", title: "Live connection" });
const updateBadge = h("button", { class: "chip accent hidden", onclick: () => void runUpdate() });

const filterInput = h("input", {
  class: "filter",
  type: "search",
  placeholder: "/ filter",
  "aria-label": "Filter sessions",
  autocomplete: "off",
  spellcheck: "false",
});
const glide = h("div", { class: "glide", "aria-hidden": "true" });
const sessionList = h("div", { class: "session-list", role: "listbox", "aria-label": "Sessions" }, glide);
const sidebarCount = h("span", { class: "side-title" });
const sidebar = h(
  "aside",
  { class: "sidebar" },
  h("div", { class: "side-head" }, sidebarCount, filterInput),
  sessionList,
);

const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const headerProject = h("button", { class: "hdr-project", title: "Session details (i)", onclick: () => toggleDetails() });
const headerBranch = h("span", { class: "hdr-branch" });
const headerModel = h("span", { class: "hdr-model" });
const headerWork = h("span", { class: "hdr-work" });
const contextMeter = h("span", { class: "ctx hidden", title: "Context window" });
const liveCount = h("span", { class: "live-count hidden" });
const stopButton = h("button", { class: "stop hidden", title: "Interrupt (x x)", onclick: () => void requestStop() }, "■ stop");
const backButton = h("button", { class: "hdr-btn back", title: "Back to sessions", onclick: () => showMobileList() }, "‹");
const detailsPanel = h("div", { class: "details-panel" });
const followHint = h("button", { class: "follow", title: "Follow the live chat (End)", onclick: () => chat.follow() });

const tabIndicator = h("span", { class: "tab-indicator" });
const tabButtons = new Map<Tab, HTMLButtonElement>();
const tabBar = h("nav", { class: "tabs", role: "tablist" });
for (const tab of TABS) {
  const button = h(
    "button",
    { class: "tab", role: "tab", "data-tab": tab, onclick: () => setTab(tab) },
    h("span", { class: "tab-num" }, String(TABS.indexOf(tab) + 1)),
    TAB_LABELS[tab].toLowerCase(),
    h("span", { class: "tab-badge" }),
  );
  tabButtons.set(tab, button);
  tabBar.append(button);
}
tabBar.append(tabIndicator, h("span", { class: "spacer" }), followHint, stopButton);

const searchInput = h("input", { class: "pane-search-input", placeholder: "Search this pane", autocomplete: "off", spellcheck: "false" });
const searchCount = h("span", { class: "dim small" });
const searchBar = h(
  "div",
  { class: "pane-search hidden" },
  searchInput,
  searchCount,
  h("button", { class: "icon-btn", title: "Previous (Shift+Enter)", onclick: () => moveSearch(-1) }, "↑"),
  h("button", { class: "icon-btn", title: "Next (Enter)", onclick: () => moveSearch(1) }, "↓"),
  h("button", { class: "icon-btn", title: "Close (Esc)", onclick: () => closeSearch() }, "×"),
);

function toast(message: string, kind: ToastKind = "info"): void {
  const element = h("div", { class: `toast ${kind}` }, h("span", { class: "toast-glyph" }, kind === "error" ? "×" : kind === "success" ? "✓" : kind === "warning" ? "!" : "•"), message);
  toasts.append(element);
  const timeout = kind === "error" ? 7000 : kind === "warning" ? 5000 : 3200;
  setTimeout(() => {
    element.classList.add("leaving");
    setTimeout(() => element.remove(), 300);
  }, timeout);
}

const chat = new ChatView({
  openFileInDiff: (path) => {
    setTab("diff");
    const session = selectedSession();
    const relative = session?.cwd && path.startsWith(`${session.cwd}/`) ? path.slice(session.cwd.length + 1) : path;
    void refreshDiff(true).then(() => diffView.reveal(relative));
  },
  preferences: () => diffPreferences,
  toast,
});

const diffView = new WorkspaceDiffView(diffPreferences, (preferences) => {
  Object.assign(diffPreferences, preferences);
  storage.set("diffStyle", preferences.style);
  storage.set("diffWrap", preferences.wrap);
});

const activityList = h("div", { class: "activity-list" });
const activityPane = h("div", { class: "scroll-pane" }, activityList);
const outputBody = h("div", { class: "md output-md" });
let outputRaw = storage.get("outputRaw", false);
let outputText = "";
const outputRawToggle = h("button", { class: "chip", onclick: () => {
  outputRaw = !outputRaw;
  storage.set("outputRaw", outputRaw);
  outputRawToggle.classList.toggle("on", outputRaw);
  renderOutput(true);
} }, "Raw");
outputRawToggle.classList.toggle("on", outputRaw);
const outputPane = h(
  "div",
  { class: "scroll-pane output-pane" },
  h("div", { class: "output-toolbar" }, h("span", { class: "dim small" }, "output.md · every completed agent message"), h("span", { class: "spacer" }), outputRawToggle, h("button", { class: "chip", onclick: () => void copyText(outputText).then(() => toast("Output copied", "success")) }, "Copy")),
  outputBody,
);
wireCopyButtons(outputBody);

const panes: Record<Tab, HTMLElement> = {
  chat: h("section", { class: "pane", "data-pane": "chat" }, chat.element),
  activity: h("section", { class: "pane", "data-pane": "activity" }, activityPane),
  output: h("section", { class: "pane", "data-pane": "output" }, outputPane),
  diff: h("section", { class: "pane", "data-pane": "diff" }, diffView.element),
};
const paneHost = h("div", { class: "panes" }, ...TABS.map((tab) => panes[tab]));

const composerInput = h("textarea", { class: "composer-input", rows: 1, placeholder: "Select a session", spellcheck: "true" });
const composerRoute = h("span", { class: "route" });
const composerModel = new ThemedSelect("composer-model hidden", "Model for the continuation run (m)");
const sendButton = h("button", { class: "send", type: "submit", title: "Send (Enter)" }, "↑");
const composer = h(
  "form",
  { class: "composer", onsubmit: (event: Event) => {
    event.preventDefault();
    void submitPrompt();
  } },
  composerRoute,
  composerInput,
  composerModel.element,
  sendButton,
);

const emptyMain = h(
  "div",
  { class: "empty-main" },
  h("div", { class: "empty-logo" }, "⎈"),
  h("h2", null, "No session selected"),
  h("p", { class: "dim" }, "Pick a session on the left, or start a new one."),
  h("button", { class: "btn primary", onclick: () => openNewSession() }, "New session"),
);

const main = h("main", { class: "main" }, tabBar, detailsPanel, searchBar, paneHost, composer, emptyMain);

const mobileBar = h("nav", { class: "mobile-bar" });
const mobileButtons = new Map<string, HTMLButtonElement>();
for (const [id, label, action] of [
  ["chat", "Chat", () => setTab("chat")],
  ["activity", "Activity", () => setTab("activity")],
  ["output", "Output", () => setTab("output")],
  ["diff", "Diff", () => setTab("diff")],
  ["more", "More", () => openPalette()],
] as Array<[string, string, () => void]>) {
  const button = h("button", { class: "mobile-btn", "data-id": id, onclick: action }, h("span", { class: "mobile-glyph" }, { chat: "◆", activity: "≡", output: "¶", diff: "±", more: "⋯" }[id] ?? "•"), label);
  mobileButtons.set(id, button);
  mobileBar.append(button);
}

const topbar = h(
  "header",
  { class: "hdr" },
  backButton,
  h("span", { class: "brand" }, h("span", { class: "brand-text" }, "◆ ruddr"), connection),
  h("span", { class: "hdr-sep" }, "│"),
  headerProject,
  headerBranch,
  headerModel,
  headerWork,
  h("span", { class: "spacer" }),
  contextMeter,
  liveCount,
  updateBadge,
  h("button", { class: "hdr-btn", title: "Commands (⌘K)", onclick: () => openPalette() }, navigator.platform.includes("Mac") ? "⌘K" : "^K"),
  h("button", { class: "hdr-btn", title: "Theme (t)", onclick: () => openThemePicker() }, "◐"),
  h("button", { class: "hdr-btn accent", title: "New session (n)", onclick: () => openNewSession() }, "+ new"),
);

const footer = h("footer", { class: "keys" });

const shell = h("div", { class: "shell" }, sidebar, main);
root.append(topbar, shell, footer, mobileBar, toasts);

// ---------------------------------------------------------------------------
// Theme

function luminance(hex: string): number {
  const value = hex.replace("#", "");
  const [r, g, b] = [0, 2, 4].map((offset) => Number.parseInt(value.slice(offset, offset + 2), 16) / 255);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function applyTheme(name: string): void {
  const theme = state.meta?.themes.find((candidate) => candidate.name === name) ?? state.meta?.themes[0];
  if (!theme) return;
  const style = document.documentElement.style;
  for (const [key, value] of Object.entries(theme.palette)) style.setProperty(`--${key}`, value);
  const light = luminance(theme.palette.background) > 0.5;
  document.documentElement.dataset.mode = light ? "light" : "dark";
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", theme.palette.background);
  const themeType = light ? "light" : "dark";
  // Colors are CSS variables, so a palette change repaints for free. Only the
  // Pierre highlighter needs a rebuild, and only when dark and light swap.
  if (diffPreferences.themeType === themeType) return;
  diffPreferences.themeType = themeType;
  diffView.setTheme(themeType);
  chat.rerenderAll();
}

// ---------------------------------------------------------------------------
// Sessions

function selectedSession(): Session | undefined {
  return state.sessions.find((session) => session.stateDir === state.selected);
}

function visibleSessions(): Session[] {
  const needle = state.filter.trim().toLowerCase();
  if (!needle) return state.sessions;
  return state.sessions.filter((session) =>
    [session.status, session.provider, session.cwd, projectName(session), session.threadId, session.turnId, session.model, session.effort]
      .filter(Boolean)
      .some((value) => value!.toLowerCase().includes(needle)),
  );
}

const sessionRows = new Map<string, HTMLElement>();

const lastStatus = new Map<string, string>();

function sessionRow(session: Session): HTMLElement {
  let row = sessionRows.get(session.stateDir);
  if (!row) {
    row = h("button", { class: "session", role: "option", type: "button" });
    row.addEventListener("click", () => selectSession(session.stateDir, true));
    sessionRows.set(session.stateDir, row);
  }
  // A status change flashes the row in its new status color, once.
  const before = lastStatus.get(session.stateDir);
  if (before && before !== session.status) {
    row.classList.remove("flash");
    void row.offsetWidth;
    row.classList.add("flash");
  }
  lastStatus.set(session.stateDir, session.status);
  const usage = session.tokenUsage;
  const selected = session.stateDir === state.selected;
  const signature = JSON.stringify([session.status, session.updatedAt, session.model, usage?.totalTokens, selected, Math.floor(Date.now() / 10_000)]);
  if (row.dataset.signature === signature) return row;
  row.dataset.signature = signature;
  row.dataset.status = session.status;
  row.classList.toggle("selected", selected);
  row.setAttribute("aria-selected", String(selected));
  row.title = shortPath(session.cwd, state.meta?.home ?? "") || session.stateDir;
  clear(row);
  const spinning = session.status === "active" || session.status === "starting";
  row.append(
    h("span", { class: "s-line1" },
      h("span", { class: `glyph ${session.status}${spinning ? " spin" : ""}` }, spinning ? SPINNER[0] : statusGlyph(session.status)),
      h("span", { class: "s-name" }, projectName(session)),
      h("span", { class: "s-age" }, formatAge(session.updatedAt))),
    h("span", { class: "s-line2" },
      `${session.provider ?? "codex"} · ${session.model || "default"}${usage?.totalTokens ? ` · ${formatTokens(usage.totalTokens)}` : ""}`),
  );
  return row;
}

/** Moves the selection highlight to the selected row; CSS eases the move. */
function placeGlide(): void {
  const row = state.selected ? sessionRows.get(state.selected) : undefined;
  if (!row || !row.isConnected) {
    glide.classList.add("hidden");
    return;
  }
  glide.classList.remove("hidden");
  glide.style.transform = `translateY(${row.offsetTop}px)`;
  glide.style.height = `${row.offsetHeight}px`;
}

function hash(text: string): string {
  let value = 0;
  for (let index = 0; index < text.length; index++) value = (value * 31 + text.charCodeAt(index)) | 0;
  return (value >>> 0).toString(36);
}

function renderSessions(): void {
  const sessions = visibleSessions();
  const ordered = [...sessions.filter(isLive), ...sessions.filter((session) => !isLive(session))];
  const wanted: HTMLElement[] = [glide, ...ordered.map(sessionRow)];
  if (!sessions.length)
    wanted.push(h("div", { class: "empty-state small" }, state.filter ? "Nothing matches the filter." : "No sessions yet. Press n to start one."));
  const current = [...sessionList.children];
  if (current.length !== wanted.length || current.some((element, index) => element !== wanted[index])) sessionList.replaceChildren(...wanted);
  for (const key of sessionRows.keys()) if (!state.sessions.some((session) => session.stateDir === key)) sessionRows.delete(key);
  const live = state.sessions.filter(isLive).length;
  sidebarCount.textContent = `sessions · ${live} live · ${state.sessions.length}`;
  liveCount.classList.toggle("hidden", live === 0);
  liveCount.textContent = `● ${live} live`;
  placeGlide();
}

let streamedSession: string | undefined;
let runStream: EventSource | undefined;
let streamedThread: string | undefined;
let selectionVersion = 0;

function selectSession(stateDir: string | undefined, fromUser = false): void {
  const changed = stateDir !== state.selected;
  const apply = () => {
    state.selected = stateDir;
    storage.set("selected", stateDir);
    state.interruptArmedUntil = 0;
    state.continueModel = undefined;
    if (changed) {
      selectionVersion++;
      branchLabel = "";
      loadDraft();
      diffView.reset();
      activityList.replaceChildren();
      outputText = "";
      outputBody.replaceChildren();
    }
    renderSessions();
    renderHeader();
    renderTitle();
    connectRun();
    refreshTabData();
    if (fromUser && isMobile()) state.mobileView = "session";
    applyMobileView();
  };
  if (changed && fromUser) transition(apply, isMobile() ? "push" : "session");
  else apply();
}

function connectRun(): void {
  const session = selectedSession();
  if (!session) {
    runStream?.close();
    runStream = undefined;
    streamedSession = undefined;
    streamedThread = undefined;
    chat.reset(undefined, "", false);
    return;
  }
  if (streamedSession === session.stateDir && streamedThread === session.threadId && runStream && runStream.readyState !== EventSource.CLOSED) return;
  runStream?.close();
  streamedSession = session.stateDir;
  streamedThread = session.threadId;
  chat.reset(session.threadId, "", false);
  const stream = new EventSource(`/api/run/events?dir=${encodeURIComponent(session.stateDir)}`);
  runStream = stream;
  stream.addEventListener("reset", (event) => {
    if (stream !== runStream) return;
    const data = JSON.parse((event as MessageEvent).data) as { text: string; truncated: boolean };
    chat.reset(session.threadId, data.text, data.truncated);
    renderMeter();
  });
  stream.addEventListener("append", (event) => {
    if (stream !== runStream) return;
    const data = JSON.parse((event as MessageEvent).data) as { text: string };
    chat.append(data.text);
    renderMeter();
    if (state.tab !== "chat") markTab("chat");
  });
  stream.addEventListener("problem", (event) => {
    if (stream !== runStream) return;
    toast(JSON.parse((event as MessageEvent).data).error, "error");
  });
}

function markTab(tab: Tab): void {
  tabButtons.get(tab)?.classList.add("has-news");
}

function renderMeter(): void {
  const session = selectedSession();
  const usage = { ...session?.tokenUsage, ...chat.usage };
  const window = usage.contextWindow;
  const used = usage.contextTokens;
  if (!window || !used) {
    contextMeter.classList.add("hidden");
    return;
  }
  const ratio = Math.min(1, used / window);
  const cells = 10;
  const filled = Math.round(ratio * cells);
  const signature = `${filled}:${Math.round(ratio * 100)}`;
  contextMeter.classList.remove("hidden");
  contextMeter.dataset.level = ratio > 0.85 ? "high" : ratio > 0.6 ? "mid" : "low";
  contextMeter.title = `Context ${formatTokens(used)} of ${formatTokens(window)}`;
  if (contextMeter.dataset.signature === signature) return;
  contextMeter.dataset.signature = signature;
  contextMeter.replaceChildren(
    h("span", { class: "dim" }, "ctx "),
    ...Array.from({ length: cells }, (_, index) => h("span", { class: index < filled ? "cell on" : "cell", style: `--i:${index}` }, index < filled ? "▰" : "▱")),
    h("span", { class: "dim" }, ` ${Math.round(ratio * 100)}%`),
  );
}

function renderHeader(): void {
  const session = selectedSession();
  main.classList.toggle("no-session", !session);
  topbar.classList.toggle("no-session", !session);
  if (!session) {
    renderComposer();
    renderFooter();
    return;
  }
  headerProject.textContent = projectName(session);
  headerBranch.textContent = branchLabel ? `:${branchLabel}` : "";
  headerModel.textContent = `${session.provider ?? "codex"} ${session.model || "default"}${session.effort ? ` · ${session.effort}` : ""}`;
  const working = session.status === "active";
  const workSignature = working ? "working" : session.status;
  if (headerWork.dataset.state !== workSignature) {
    headerWork.dataset.state = workSignature;
    headerWork.className = `hdr-work ${session.status}`;
    headerWork.replaceChildren(
      ...(working
        ? [h("span", { class: "spin" }, SPINNER[0]), " ", h("span", { class: "shimmer" }, "working"), " ", h("span", { class: "elapsed dim" })]
        : [h("span", null, `${statusGlyph(session.status)} ${session.status}`), " ", h("span", { class: "elapsed dim" })]),
    );
  }
  const elapsed = headerWork.querySelector(".elapsed");
  if (elapsed) elapsed.textContent = formatElapsed(session.startedAt, session.completedAt);
  const stoppable = session.status === "active" || session.status === "idle";
  stopButton.classList.toggle("hidden", !stoppable);
  stopButton.textContent = session.status === "idle" ? "■ end" : "■ stop";
  renderDetails();
  renderMeter();
  renderComposer();
  renderFooter();
}

/** The key-hint bar, or the stop countdown while x is armed. */
function renderFooter(): void {
  const armed = state.interruptArmedUntil > Date.now();
  const signature = armed ? "armed" : `${state.tab}:${promptRoute(selectedSession()) ?? ""}`;
  if (footer.dataset.signature === signature) return;
  footer.dataset.signature = signature;
  footer.classList.toggle("armed", armed);
  if (armed) {
    footer.replaceChildren(h("span", { class: "armed-label" }, "■ press x again to stop"), h("span", { class: "armed-bar" }));
    return;
  }
  const route = promptRoute(selectedSession());
  const hints: Array<[string, string]> = [
    ["j/k", "select"],
    ["/", "filter"],
    ["1-4", "tabs"],
    ...(route ? [["s", route] as [string, string]] : []),
    ["n", "new"],
    ["m", "model"],
    ["f", "find"],
    ...(state.tab === "diff" ? [["Z", "fold"], ["[ ]", "files"]] as Array<[string, string]> : []),
    ...(selectedSession()?.status === "active" || selectedSession()?.status === "idle" ? [["x x", "stop"]] as Array<[string, string]> : []),
    ["⌘K", "commands"],
    ["t", "theme"],
    ["?", "help"],
  ];
  footer.replaceChildren(
    ...hints.flatMap(([key, label], index) => [
      index ? h("span", { class: "sep" }, "·") : null,
      h("kbd", null, key),
      h("span", { class: "label" }, label),
    ]).filter((node): node is HTMLElement => node !== null),
  );
}

let branchLabel = "";

function renderDetails(): void {
  const session = selectedSession();
  detailsPanel.classList.toggle("open", state.details && Boolean(session));
  headerProject.classList.toggle("on", state.details);
  if (!session || !state.details) return;
  const row = (label: string, value: string | undefined, copy = false) =>
    value
      ? h(
          "div",
          { class: "kv" },
          h("span", null, label),
          h("code", null, value),
          copy ? h("button", { class: "icon-btn tiny", title: `Copy ${label}`, onclick: () => void copyText(value).then(() => toast(`${label} copied`, "success")) }, "⧉") : null,
        )
      : null;
  const usage = session.tokenUsage;
  clear(detailsPanel);
  append(detailsPanel, [
    h(
      "div",
      { class: "details-grid" },
      row("status", `${statusGlyph(session.status)} ${session.status}`),
      row("provider", session.provider ?? "codex"),
      row("model", `${session.model || "—"}${session.effort ? ` / ${session.effort}` : ""}`),
      row("thread", session.threadId, true),
      row("turn", session.turnId, true),
      row("cwd", session.cwd, true),
      row("state", session.stateDir, true),
      row("sandbox", session.sandbox),
      row("runtime", formatElapsed(session.startedAt, session.completedAt)),
      row("pid", String(session.pid)),
      row(
        "tokens",
        usage
          ? [
              usage.inputTokens ? `in ${formatTokens(usage.inputTokens)}` : "",
              usage.cachedInputTokens ? `cached ${formatTokens(usage.cachedInputTokens)}` : "",
              usage.outputTokens ? `out ${formatTokens(usage.outputTokens)}` : "",
              usage.totalTokens ? `total ${formatTokens(usage.totalTokens)}` : "",
              usage.costUsd ? `$${usage.costUsd.toFixed(4)}` : "",
            ]
              .filter(Boolean)
              .join(" · ")
          : undefined,
      ),
      row("started", session.startedAt ? new Date(session.startedAt).toLocaleString() : undefined),
      row("error", session.error),
    ),
    isTerminal(session.status) || session.status === "stale"
      ? h("div", { class: "details-actions" }, h("button", { class: "btn danger-ghost small", onclick: () => void deleteSession() }, "Delete session files"))
      : null,
  ]);
}

function toggleDetails(): void {
  state.details = !state.details;
  renderDetails();
}

// ---------------------------------------------------------------------------
// Tabs and pane data

function setTab(tab: Tab): void {
  const previous = state.tab;
  if (previous === tab) {
    if (isMobile() && state.mobileView === "list" && selectedSession()) {
      state.mobileView = "session";
      transition(applyMobileView, "push");
    }
    return;
  }
  state.tab = tab;
  storage.set("tab", tab);
  const direction = TABS.indexOf(tab) > TABS.indexOf(previous) ? 1 : -1;
  paneHost.style.setProperty("--dir", String(direction));
  for (const name of TABS) {
    const pane = panes[name];
    pane.classList.remove("entering", "leaving");
    if (name === tab) {
      pane.classList.add("active", "entering");
      pane.addEventListener("animationend", () => pane.classList.remove("entering"), { once: true });
    } else if (name === previous) {
      pane.classList.add("leaving");
      pane.classList.remove("active");
      pane.addEventListener("animationend", () => pane.classList.remove("leaving"), { once: true });
    } else pane.classList.remove("active");
  }
  tabButtons.get(tab)?.classList.remove("has-news");
  renderTabs();
  closeSearch();
  refreshTabData();
  if (isMobile() && state.mobileView === "list" && selectedSession()) {
    state.mobileView = "session";
    transition(applyMobileView, "push");
  }
}

function renderTabs(): void {
  for (const [tab, button] of tabButtons) {
    button.classList.toggle("active", tab === state.tab);
    button.setAttribute("aria-selected", String(tab === state.tab));
  }
  for (const [id, button] of mobileButtons) button.classList.toggle("active", id === state.tab);
  const active = tabButtons.get(state.tab);
  if (active) {
    tabIndicator.style.width = `${active.offsetWidth}px`;
    tabIndicator.style.transform = `translateX(${active.offsetLeft}px)`;
  }
  composer.classList.toggle("compact", state.tab === "diff");
  renderFooter();
}

let activityTimer: ReturnType<typeof setTimeout> | undefined;
let outputTimer: ReturnType<typeof setTimeout> | undefined;
let diffTimer: ReturnType<typeof setTimeout> | undefined;

let activityRead = 0;
let outputRead = 0;
let diffRead = 0;
let branchRead = 0;

function refreshTabData(): void {
  activityRead++;
  outputRead++;
  diffRead++;
  branchRead++;
  clearTimeout(activityTimer);
  clearTimeout(outputTimer);
  clearTimeout(diffTimer);
  if (!selectedSession()) return;
  if (state.tab === "activity") void refreshActivity();
  if (state.tab === "output") void refreshOutput();
  if (state.tab === "diff") void refreshDiff(false);
  void refreshBranch();
}

async function refreshBranch(): Promise<void> {
  // The diff endpoint reports the branch; fetch it once per selection.
  const read = ++branchRead;
  const session = selectedSession();
  if (!session?.cwd) {
    branchLabel = "";
    return;
  }
  if (state.tab === "diff") return;
  try {
    const data = await get<WorkspaceDiffData>(`/api/run/diff?dir=${encodeURIComponent(session.stateDir)}`);
    if (read !== branchRead || session.stateDir !== state.selected) return;
    branchLabel = data.branch ?? "";
    // Prefetch: the Diff tab renders in the background, so opening it is instant.
    diffView.update(data);
    updateDiffBadge(data);
    renderHeader();
  } catch {
    // The branch label is decoration.
  }
}

function updateDiffBadge(data: WorkspaceDiffData): void {
  const count = (data.content.match(/^diff --git /gm) ?? []).length + data.untracked.length;
  const badge = tabButtons.get("diff")?.querySelector(".tab-badge");
  if (badge) badge.textContent = count ? String(count) : "";
}

async function refreshActivity(): Promise<void> {
  const read = ++activityRead;
  const session = selectedSession();
  clearTimeout(activityTimer);
  if (!session) return;
  try {
    const data = await get<{ activities: Activity[] }>(`/api/run/activity?dir=${encodeURIComponent(session.stateDir)}`);
    if (read === activityRead && session.stateDir === state.selected) renderActivity(data.activities);
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) return showLogin();
  }
  if (read === activityRead && state.tab === "activity" && selectedSession()?.stateDir === session.stateDir)
    activityTimer = setTimeout(() => void refreshActivity(), isLive(session) ? 1200 : 6000);
}

const openActivities = new Set<string>();
let activitySignature = "";

function renderActivity(activities: Activity[]): void {
  const signature = JSON.stringify(activities.map((activity) => [activity.timestamp, activity.kind, activity.text, activity.toolStatus, activity.durationMs]));
  if (signature === activitySignature && activityList.childElementCount) return;
  activitySignature = signature;
  const atBottom = activityPane.scrollHeight - activityPane.scrollTop - activityPane.clientHeight < 48;
  if (!activities.length) {
    activityList.replaceChildren(h("div", { class: "empty-state" }, "No activity yet."));
    return;
  }
  activityList.replaceChildren(
    ...activities.map((activity, index) => {
      const key = `${activity.timestamp}:${index}`;
      const time = new Date(activity.timestamp);
      const clock = Number.isNaN(time.getTime()) ? "" : time.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
      const glyph =
        activity.kind === "tool"
          ? activity.toolStatus === "running"
            ? "◌"
            : activity.toolStatus === "failed"
              ? "✗"
              : "✓"
          : activity.kind === "thought"
            ? "∴"
            : activity.kind === "message"
              ? "›"
              : activity.kind === "warning"
                ? "!"
                : activity.kind === "error"
                  ? "×"
                  : "•";
      const row = h(
        "div",
        { class: `activity ${activity.kind} ${activity.toolStatus ?? ""}${openActivities.has(key) ? " open" : ""}`, "data-search": `${activity.label ?? ""} ${activity.text}` },
        h(
          "button",
          {
            class: "activity-head",
            type: "button",
            onclick: () => {
              if (activity.kind === "message") {
                row.classList.toggle("open");
                return;
              }
              if (activity.kind !== "tool") return;
              if (openActivities.has(key)) openActivities.delete(key);
              else openActivities.add(key);
              row.classList.toggle("open");
              if (!row.querySelector(".activity-detail")) row.append(activityDetail(activity));
            },
          },
          h("span", { class: "a-time" }, clock),
          h("span", { class: "a-glyph" }, glyph),
          activity.label ? h("span", { class: "a-label" }, activity.label) : null,
          h("span", { class: "a-text" }, activity.text),
          activity.durationMs !== undefined ? h("span", { class: "row-meta" }, formatDuration(activity.durationMs)) : null,
        ),
      );
      if (openActivities.has(key)) row.append(activityDetail(activity));
      return row;
    }),
  );
  if (atBottom) activityPane.scrollTop = activityPane.scrollHeight;
}

function activityDetail(activity: Activity): HTMLElement {
  const detail = activity.detail;
  if (!detail) return h("div", { class: "activity-detail dim small" }, "No additional tool detail was captured.");
  const field = (label: string, value: string | undefined) => (value ? h("div", { class: "kv" }, h("span", null, label), h("code", null, value)) : null);
  return h(
    "div",
    { class: "activity-detail" },
    field("command", detail.command ?? activity.text),
    field("status", `${detail.status}${detail.exitCode === undefined ? "" : ` · exit ${detail.exitCode}`}${detail.durationMs === undefined ? "" : ` · ${formatDuration(detail.durationMs)}`}`),
    field("cwd", detail.cwd),
    field("thread", detail.agentThreadId),
    detail.input && Object.keys(detail.input).length ? h("pre", { class: "json" }, JSON.stringify(detail.input, null, 2)) : null,
    detail.output ? h("pre", { class: "term" }, ...ansiNodes(detail.output)) : null,
  );
}

async function refreshOutput(): Promise<void> {
  const read = ++outputRead;
  const session = selectedSession();
  clearTimeout(outputTimer);
  if (!session) return;
  try {
    const data = await get<{ text: string }>(`/api/run/output?dir=${encodeURIComponent(session.stateDir)}`);
    if (read === outputRead && session.stateDir === state.selected && data.text !== outputText) {
      outputText = data.text;
      renderOutput(false);
    }
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) return showLogin();
  }
  if (read === outputRead && state.tab === "output" && selectedSession()?.stateDir === session.stateDir)
    outputTimer = setTimeout(() => void refreshOutput(), isLive(session) ? 2000 : 8000);
}

function renderOutput(force: boolean): void {
  if (!force && !outputText && outputBody.childElementCount) return;
  const atBottom = outputPane.scrollHeight - outputPane.scrollTop - outputPane.clientHeight < 48;
  if (!outputText.trim()) outputBody.replaceChildren(h("div", { class: "empty-state" }, "No completed agent messages yet."));
  else if (outputRaw) outputBody.replaceChildren(h("pre", { class: "raw" }, outputText));
  else outputBody.innerHTML = markdownHTML(outputText);
  if (atBottom) outputPane.scrollTop = outputPane.scrollHeight;
}

async function refreshDiff(force: boolean): Promise<void> {
  const read = ++diffRead;
  const session = selectedSession();
  if (!session) return;
  clearTimeout(diffTimer);
  try {
    const data = await get<WorkspaceDiffData>(`/api/run/diff?dir=${encodeURIComponent(session.stateDir)}${force ? "&force=1" : ""}`);
    if (read !== diffRead || session.stateDir !== state.selected) return;
    branchLabel = data.branch ?? "";
    diffView.update(data);
    updateDiffBadge(data);
    renderHeader();
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) return showLogin();
  }
  if (read === diffRead && state.tab === "diff" && selectedSession()?.stateDir === session.stateDir)
    diffTimer = setTimeout(() => void refreshDiff(false), isLive(session) ? 1500 : 5000);
}

// ---------------------------------------------------------------------------
// Composer

const drafts = storage.get<Record<string, string>>("drafts", {});
const draftTargets = new Map<string, DraftTarget>();

function composerTarget(session = selectedSession()): DraftTarget | undefined {
  const target = draftTarget(session, session ? draftTargets.get(session.stateDir) : undefined, Boolean(composerInput.value));
  if (target) draftTargets.set(target.stateDir, target);
  else if (session) draftTargets.delete(session.stateDir);
  return target;
}

function loadDraft(): void {
  composerInput.value = (state.selected && drafts[state.selected]) || "";
  composerTarget();
  autosize();
}

function saveDraft(): void {
  if (!state.selected) return;
  if (composerInput.value) drafts[state.selected] = composerInput.value;
  else delete drafts[state.selected];
  storage.set("drafts", drafts);
}

function autosize(): void {
  composerInput.style.height = "auto";
  const wanted = composerInput.scrollHeight;
  // A hidden composer measures 0; leave it at its natural height.
  composerInput.style.height = wanted ? `${Math.min(wanted, Math.round(window.innerHeight * 0.4))}px` : "";
}

function renderComposer(): void {
  const session = selectedSession();
  const target = composerTarget(session);
  const route = target?.route;
  composer.dataset.route = route ?? "none";
  composerInput.disabled = !route;
  sendButton.disabled = !route || state.busy;
  const labels = {
    steer: ["Steer", "Steer the running turn…"],
    prompt: ["Prompt", "Send the next turn to this idle session…"],
    continue: ["Continue", "Ask for changes or a follow-up in a new run…"],
  } as const;
  if (route) {
    composerRoute.textContent = labels[route][0];
    composerInput.placeholder = labels[route][1];
  } else {
    composerRoute.textContent = session ? session.status : "";
    composerInput.placeholder = !session
      ? "Select a session"
      : session.status === "starting"
        ? "Session is starting…"
        : session.status === "stale"
          ? "This controller is gone; its state is stale"
          : "This session cannot take a prompt";
  }
  const showModel = route === "continue";
  composerModel.element.classList.toggle("hidden", !showModel);
  if (showModel && session) {
    const provider = session.provider ?? "codex";
    const options = state.models.filter((model) => model.provider === provider && model.id && model.available !== false);
    const signature = `${provider}:${options.map((model) => model.id).join(",")}:${session.model}`;
    if (composerModel.element.dataset.signature !== signature) {
      composerModel.element.dataset.signature = signature;
      composerModel.setOptions([
        { value: "", label: `same model · ${session.model || "default"}` },
        ...options.filter((model) => model.id !== session.model).map((model) => ({ value: model.id!, label: model.label ?? model.id! })),
      ]);
    }
    composerModel.value = state.continueModel ?? "";
  }
}

composerModel.onChange((value) => {
  state.continueModel = value || undefined;
  composerInput.focus();
});

composerInput.addEventListener("input", () => {
  composerTarget();
  autosize();
  saveDraft();
  renderComposer();
});
composerInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey && !event.isComposing && !(isMobile() && !event.metaKey && !event.ctrlKey)) {
    event.preventDefault();
    void submitPrompt();
  } else if (event.key === "Escape") composerInput.blur();
});

async function submitPrompt(): Promise<void> {
  const session = selectedSession();
  const target = composerTarget(session);
  const route = target?.route;
  const message = composerInput.value.trim();
  if (!session || !route || !message || state.busy) return;
  state.busy = true;
  composer.classList.add("sending");
  renderComposer();
  const sent = composerInput.value;
  const selection = selectionVersion;
  try {
    const result = await post<{ status: string; stateDir?: string }>("/api/prompt", {
      stateDir: session.stateDir,
      route,
      turnId: target?.turnId,
      message,
      model: route === "continue" ? state.continueModel : undefined,
    });
    // A late success must not wipe text typed while the request was in flight.
    if (selection === selectionVersion && state.selected === session.stateDir && composerInput.value === sent) {
      composerInput.value = "";
      draftTargets.delete(session.stateDir);
      saveDraft();
      autosize();
    }
    toast(result.status, "success");
    if (selection === selectionVersion) chat.follow();
    if (result.stateDir && selection === selectionVersion) {
      await refreshSessions();
      if (selection === selectionVersion) selectSession(result.stateDir, true);
    }
  } catch (error) {
    toast(error instanceof Error ? error.message : String(error), "error");
  } finally {
    state.busy = false;
    composer.classList.remove("sending");
    renderComposer();
  }
}

// ---------------------------------------------------------------------------
// Control actions

async function requestStop(): Promise<void> {
  const session = selectedSession();
  if (!session || (session.status !== "active" && session.status !== "idle")) {
    toast("Only an active or idle session can be stopped", "warning");
    return;
  }
  const now = Date.now();
  if (state.interruptArmedUntil < now) {
    state.interruptArmedUntil = now + 2000;
    renderHeader();
    stopButton.classList.add("armed");
    setTimeout(() => {
      stopButton.classList.remove("armed");
      renderHeader();
    }, 2050);
    return;
  }
  state.interruptArmedUntil = 0;
  renderHeader();
  try {
    const result = await post<{ status: string }>("/api/stop", { stateDir: session.stateDir });
    toast(result.status, "success");
  } catch (error) {
    toast(error instanceof Error ? error.message : String(error), "error");
  }
}

async function deleteSession(): Promise<void> {
  const session = selectedSession();
  if (!session) return;
  if (!confirm(`Delete the run files for ${projectName(session)}?\n${session.stateDir}\n\nThis cannot be undone.`)) return;
  try {
    const result = await post<{ status: string }>("/api/delete", { stateDir: session.stateDir });
    toast(result.status, "success");
    const next = state.sessions.find((candidate) => candidate.stateDir !== session.stateDir);
    selectSession(next?.stateDir);
  } catch (error) {
    toast(error instanceof Error ? error.message : String(error), "error");
  }
}

async function runUpdate(): Promise<void> {
  const target = state.meta?.updateAvailable;
  if (!target) return;
  toast(`Updating Ruddr to ${target}…`);
  try {
    const result = await post<{ status: string }>("/api/update", {});
    toast(result.status, "success");
    if (state.meta) state.meta.updateAvailable = undefined;
    updateBadge.classList.add("hidden");
  } catch (error) {
    toast(`Update failed: ${error instanceof Error ? error.message : String(error)}`, "error");
  }
}

// ---------------------------------------------------------------------------
// Dialogs

let activeDialog: { element: HTMLElement; close: () => void } | undefined;

function openDialog(content: HTMLElement, options: { onClose?: () => void; className?: string } = {}): () => void {
  activeDialog?.close();
  const backdrop = h("div", { class: `dialog-backdrop ${options.className ?? ""}` });
  const panel = h("div", { class: "dialog", role: "dialog", "aria-modal": "true" }, content);
  backdrop.append(panel);
  const close = () => {
    if (activeDialog?.element !== backdrop) return;
    activeDialog = undefined;
    backdrop.classList.add("closing");
    setTimeout(() => backdrop.remove(), 180);
    options.onClose?.();
  };
  backdrop.addEventListener("mousedown", (event) => {
    if (event.target === backdrop) close();
  });
  document.body.append(backdrop);
  activeDialog = { element: backdrop, close };
  return close;
}

interface PaletteItem {
  id: string;
  label: string;
  key?: string;
  hint?: string;
  disabled?: string;
  group?: string;
  run: () => void;
}

function paletteItems(): PaletteItem[] {
  const session = selectedSession();
  const route = promptRoute(session);
  const stoppable = session?.status === "active" || session?.status === "idle";
  const items: PaletteItem[] = [
    { id: "prompt", label: "Send a prompt", key: "s", hint: "steer, prompt, or continue the selected session", disabled: route ? undefined : "no promptable session selected", run: () => focusComposer() },
    { id: "new", label: "New session", key: "n", hint: "pick a provider and model, then type the first prompt", run: () => openNewSession() },
    { id: "continue", label: "Continue thread in a new run", key: "R", hint: "finished sessions only", disabled: route === "continue" ? undefined : "select a finished session with a thread", run: () => focusComposer() },
    { id: "model", label: "Choose model", key: "m", hint: "continuation model, or a new session's model", run: () => chooseModel() },
    { id: "find", label: "Find a past session", key: "f", hint: "deja search", disabled: state.meta?.dejaAvailable ? undefined : "deja is not on PATH", run: () => openDeja() },
    { id: "stop", label: session?.status === "idle" ? "End idle session" : "Interrupt turn", key: "x x", disabled: stoppable ? undefined : "no active or idle session", run: () => void requestStop() },
    { id: "tab-chat", label: "Show chat", key: "1", run: () => setTab("chat") },
    { id: "tab-activity", label: "Show activity", key: "2", run: () => setTab("activity") },
    { id: "tab-output", label: "Show output", key: "3", run: () => setTab("output") },
    { id: "tab-diff", label: "Show diff", key: "4", hint: "tracked changes against HEAD", run: () => setTab("diff") },
    { id: "fold", label: "Fold or unfold every diff file", key: "Z", disabled: state.tab === "diff" ? undefined : "diff tab only", run: () => diffView.toggleAll() },
    { id: "split", label: diffPreferences.style === "split" ? "Use unified diffs" : "Use split diffs", run: () => diffView.setPreferences({ style: diffPreferences.style === "split" ? "unified" : "split" }) },
    { id: "search", label: "Search this pane", key: "/", run: () => openSearch() },
    { id: "filter", label: "Filter sessions", key: "F", hint: "project, thread, status, or model", run: () => focusFilter() },
    { id: "follow", label: "Resume live follow", key: "End", disabled: chat.isFollowing() ? "already following" : undefined, run: () => chat.follow() },
    { id: "details", label: "Toggle session details", key: "i", run: () => toggleDetails() },
    { id: "copy", label: "Copy the last agent message", key: "c", run: () => copyLastMessage() },
    { id: "copy-thread", label: "Copy thread ID", disabled: session?.threadId ? undefined : "no thread", run: () => void copyText(session!.threadId!).then(() => toast("Thread ID copied", "success")) },
    { id: "delete", label: "Delete session files", disabled: session && (isTerminal(session.status) || session.status === "stale") ? undefined : "finished or stale sessions only", run: () => void deleteSession() },
    {
      id: "notify",
      label: "Notify me when turns finish",
      hint: "browser notifications while this tab is hidden",
      disabled: typeof Notification !== "undefined" && Notification.permission === "granted" ? "already on" : undefined,
      run: () => void enableNotifications(),
    },
    { id: "theme", label: "Change theme", key: "t", hint: "live preview", run: () => openThemePicker() },
    { id: "refresh", label: "Refresh sessions", key: "r", run: () => void refreshSessions().then(() => toast("Sessions refreshed")) },
    {
      id: "update",
      label: state.meta?.updateAvailable ? `Update Ruddr to ${state.meta.updateAvailable}` : "Update Ruddr",
      hint: "runs ruddr update; restart ruddr web afterwards",
      disabled: state.meta?.updateAvailable ? undefined : "no newer release found on the last daily check",
      run: () => void runUpdate(),
    },
    { id: "help", label: "Keyboard shortcuts", key: "?", run: () => openHelp() },
  ];
  for (const candidate of state.sessions.slice(0, 40))
    items.push({
      id: `go:${candidate.stateDir}`,
      group: "Sessions",
      label: `${statusGlyph(candidate.status)} ${projectName(candidate)}`,
      hint: `${candidate.provider ?? "codex"} · ${candidate.model || "default"} · ${formatAge(candidate.updatedAt)}`,
      run: () => selectSession(candidate.stateDir, true),
    });
  return items;
}

function scoreItem(item: PaletteItem, terms: string[]): number {
  const label = item.label.toLowerCase();
  const haystack = `${label} ${item.key ?? ""} ${item.hint ?? ""}`.toLowerCase();
  let total = 0;
  for (const term of terms) {
    if (label.startsWith(term)) total += 3;
    else if (label.includes(term)) total += 2;
    else if (haystack.includes(term)) total += 1;
    else return 0;
  }
  return total;
}

function openPalette(): void {
  const input = h("input", { class: "dialog-input", placeholder: "Type a command or a session", autocomplete: "off", spellcheck: "false" });
  const list = h("div", { class: "palette-list", role: "listbox" });
  let index = 0;
  let items: PaletteItem[] = [];
  const all = paletteItems();
  const render = () => {
    const terms = input.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
    items = terms.length
      ? all
          .map((item, order) => ({ item, order, score: scoreItem(item, terms) }))
          .filter((entry) => entry.score > 0)
          .sort((a, b) => b.score - a.score || a.order - b.order)
          .map((entry) => entry.item)
      : all;
    index = Math.min(index, Math.max(0, items.length - 1));
    let group: string | undefined;
    list.replaceChildren(
      ...items.flatMap((item, position) => {
        const nodes: HTMLElement[] = [];
        if (item.group !== group) {
          group = item.group;
          if (group) nodes.push(h("div", { class: "palette-group" }, group));
        }
        nodes.push(
          h(
            "button",
            {
              class: `palette-item${position === index ? " active" : ""}${item.disabled ? " disabled" : ""}`,
              type: "button",
              onmousemove: () => {
                if (index !== position) {
                  index = position;
                  render();
                }
              },
              onclick: () => run(item),
            },
            h("span", { class: "p-label" }, item.label),
            h("span", { class: "p-hint" }, item.disabled ?? item.hint ?? ""),
            item.key ? h("kbd", null, item.key) : null,
          ),
        );
        return nodes;
      }),
    );
    list.querySelector(".active")?.scrollIntoView({ block: "nearest" });
  };
  const run = (item: PaletteItem | undefined) => {
    if (!item) return;
    if (item.disabled) {
      toast(item.disabled, "warning");
      return;
    }
    close();
    item.run();
  };
  input.addEventListener("input", () => {
    index = 0;
    render();
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown" || (event.ctrlKey && event.key === "n")) {
      event.preventDefault();
      index = Math.min(items.length - 1, index + 1);
      render();
    } else if (event.key === "ArrowUp" || (event.ctrlKey && event.key === "p")) {
      event.preventDefault();
      index = Math.max(0, index - 1);
      render();
    } else if (event.key === "Enter") {
      event.preventDefault();
      run(items[index]);
    } else if (event.key === "Escape") close();
  });
  const close = openDialog(h("div", { class: "palette" }, input, list), { className: "top" });
  render();
  input.focus();
}

async function loadModels(): Promise<void> {
  try {
    state.models = await get<ModelInfo[]>("/api/models");
  } catch {
    state.models = [];
  }
  renderComposer();
}

function openNewSession(prefill: { provider?: string; resumeThreadId?: string; resumeLabel?: string; cwd?: string } = {}): void {
  const providers = state.meta?.providers ?? ["codex"];
  const remembered = storage.get<{ provider?: string; model?: string; effort?: string; cwd?: string }>("newSession", {});
  let provider = prefill.provider ?? remembered.provider ?? providers[0];
  const providerRow = h("div", { class: "segmented" });
  const modelSelect = new ThemedSelect("field");
  const effortSelect = new ThemedSelect("field");
  const cwdInput = h("input", { class: "field", spellcheck: "false", autocomplete: "off" });
  const promptInput = h("textarea", { class: "field prompt", rows: 6, placeholder: "What should the agent do?" });
  const submit = h("button", { class: "btn primary", type: "submit" }, prefill.resumeThreadId ? "Resume session" : "Start session");
  const recentCwds = [...new Set(state.sessions.map((session) => session.cwd).filter((cwd): cwd is string => Boolean(cwd) && !cwd!.includes("/.scratch/")))];
  cwdInput.value = prefill.cwd ?? selectedSession()?.cwd ?? remembered.cwd ?? state.meta?.cwd ?? "";

  const renderProviders = () => {
    providerRow.replaceChildren(
      ...providers.map((name) =>
        h(
          "button",
          {
            type: "button",
            class: `seg${name === provider ? " on" : ""}`,
            disabled: Boolean(prefill.resumeThreadId) && name !== provider,
            onclick: () => {
              provider = name;
              renderProviders();
              renderModels();
            },
          },
          name,
        ),
      ),
    );
  };
  const renderModels = () => {
    const models = state.models.filter((model) => model.provider === provider && model.available !== false);
    const preferred = remembered.provider === provider ? remembered.model : undefined;
    modelSelect.setOptions([
      { value: "", label: "provider default" },
      ...models.filter((model) => model.id).map((model) => ({ value: model.id!, label: model.label ?? model.id!, hint: model.default ? "default" : undefined })),
    ]);
    modelSelect.value = models.some((model) => model.id === preferred) ? preferred! : models.find((model) => model.default)?.id ?? "";
    renderEfforts();
  };
  const renderEfforts = () => {
    const model = state.models.find((candidate) => candidate.provider === provider && candidate.id === modelSelect.value);
    const efforts = model?.efforts ?? [];
    effortSelect.setOptions([{ value: "", label: "default effort" }, ...efforts.map((effort) => ({ value: effort, label: effort }))]);
    effortSelect.value = remembered.effort && efforts.includes(remembered.effort) ? remembered.effort : "";
    effortSelect.disabled = efforts.length === 0;
  };
  modelSelect.onChange(renderEfforts);
  const home = state.meta?.home ?? "";
  const suggestions = new Suggestions(cwdInput, async (value) => {
    const typed = value.trim();
    const data = await get<{ entries: string[] }>(`/api/dirs?path=${encodeURIComponent(typed)}`);
    const recent = recentCwds.map((path) => shortPath(path, home)).filter((path) => !typed || path.startsWith(typed));
    return [...new Set([...recent, ...data.entries.map((path) => shortPath(path, home))])].filter((path) => !path.includes("/.scratch/"));
  });

  const form = h(
    "form",
    { class: "new-session" },
    h("div", { class: "dialog-title" }, prefill.resumeThreadId ? "Resume a past session" : "New session"),
    prefill.resumeLabel ? h("div", { class: "resume-note" }, prefill.resumeLabel) : null,
    h("label", { class: "label" }, "Provider"),
    providerRow,
    h("div", { class: "field-row" }, h("div", null, h("label", { class: "label" }, "Model"), modelSelect.element), h("div", null, h("label", { class: "label" }, "Effort"), effortSelect.element)),
    h("label", { class: "label" }, "Working directory"),
    cwdInput,
    h("label", { class: "label" }, prefill.resumeThreadId ? "Next prompt" : "First prompt"),
    promptInput,
    h("div", { class: "dialog-actions" }, h("span", { class: "hint" }, "⌘/Ctrl+Enter to start"), h("span", { class: "spacer" }), h("button", { class: "btn ghost", type: "button", onclick: () => close() }, "Cancel"), submit),
  );
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (submit.disabled) return;
    const message = promptInput.value.trim();
    if (!message) {
      promptInput.focus();
      toast("Type the first prompt", "warning");
      return;
    }
    submit.disabled = true;
    submit.textContent = "Starting…";
    const choice = { provider, model: modelSelect.value || undefined, effort: effortSelect.value || undefined, cwd: cwdInput.value.trim() || undefined };
    storage.set("newSession", choice);
    try {
      const result = await post<{ status: string; stateDir: string }>("/api/new", { ...choice, message, resumeThreadId: prefill.resumeThreadId });
      close();
      toast(result.status, "success");
      await refreshSessions();
      selectSession(result.stateDir, true);
      setTab("chat");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
      submit.disabled = false;
      submit.textContent = prefill.resumeThreadId ? "Resume session" : "Start session";
    }
  });
  form.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      form.requestSubmit();
    } else if (event.key === "Escape") close();
  });
  const close = openDialog(form, { onClose: () => suggestions.close() });
  renderProviders();
  renderModels();
  if (!state.models.length) void loadModels().then(renderModels);
  promptInput.focus();
}

function openDeja(): void {
  if (!state.meta?.dejaAvailable) {
    toast("deja is not on PATH; install it to resume past sessions", "warning");
    return;
  }
  const input = h("input", { class: "dialog-input", placeholder: "Search past Claude and Codex sessions…", autocomplete: "off" });
  const list = h("div", { class: "palette-list" });
  let hits: DejaHit[] = [];
  let index = 0;
  const render = () => {
    list.replaceChildren(
      ...hits.map((hit, position) =>
        h(
          "button",
          { class: `palette-item deja${position === index ? " active" : ""}`, type: "button", onclick: () => pick(hit) },
          h("span", { class: "p-label" }, `${hit.provider} · ${hit.project}`),
          h("span", { class: "p-hint" }, `${hit.date} · ${hit.openingPrompt.slice(0, 120)}`),
        ),
      ),
    );
  };
  const pick = (hit: DejaHit | undefined) => {
    if (!hit) return;
    close();
    openNewSession({ provider: hit.provider, resumeThreadId: hit.sessionId, resumeLabel: `${hit.provider} ${hit.sessionId.slice(0, 12)} · ${hit.project} · ${hit.openingPrompt.slice(0, 80)}` });
  };
  input.addEventListener("keydown", async (event) => {
    if (event.key === "Escape") return close();
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      index = Math.max(0, Math.min(hits.length - 1, index + (event.key === "ArrowDown" ? 1 : -1)));
      render();
      return;
    }
    if (event.key !== "Enter") return;
    event.preventDefault();
    if (hits.length && input.dataset.searched === input.value) return pick(hits[index]);
    list.replaceChildren(h("div", { class: "empty-state small" }, "Searching…"));
    try {
      hits = await get<DejaHit[]>(`/api/deja?q=${encodeURIComponent(input.value)}`);
      input.dataset.searched = input.value;
      index = 0;
      if (!hits.length) list.replaceChildren(h("div", { class: "empty-state small" }, `No resumable sessions matched “${input.value}”.`));
      else render();
    } catch (error) {
      list.replaceChildren(h("div", { class: "empty-state small" }, error instanceof Error ? error.message : String(error)));
    }
  });
  const close = openDialog(h("div", { class: "palette" }, h("div", { class: "dialog-title" }, "deja find"), input, list), { className: "top" });
  input.focus();
}

function openThemePicker(): void {
  const themes = state.meta?.themes ?? [];
  const original = state.meta?.theme ?? "ruddr";
  let index = Math.max(0, themes.findIndex((theme) => theme.name === original));
  let saved = false;
  const input = h("input", { class: "dialog-input", placeholder: "Filter themes", autocomplete: "off" });
  const list = h("div", { class: "palette-list themes" });
  let shown = themes;
  let rows: HTMLButtonElement[] = [];
  // Hover and arrow keys only move the highlight. Rebuilding the list under the
  // pointer would fire mouseenter again and flicker.
  const highlight = (scroll: boolean) => {
    rows.forEach((row, position) => row.classList.toggle("active", position === index));
    if (scroll) rows[index]?.scrollIntoView({ block: "nearest" });
  };
  const render = () => {
    const needle = input.value.trim().toLowerCase();
    shown = themes.filter((theme) => !needle || theme.label.toLowerCase().includes(needle) || theme.name.includes(needle));
    index = Math.min(index, Math.max(0, shown.length - 1));
    rows = shown.map((theme, position) =>
      h(
        "button",
        {
          class: "palette-item",
          type: "button",
          onmouseenter: () => {
            if (index === position) return;
            index = position;
            highlight(false);
            preview();
          },
          onclick: () => void save(theme.name),
        },
        h(
          "span",
          { class: "swatches" },
          ...(["background", "panel", "accent", "success", "warning", "danger"] as const).map((key) => h("span", { class: "swatch", style: `background:${theme.palette[key]}` })),
        ),
        h("span", { class: "p-label" }, theme.label),
        h("span", { class: "p-hint" }, `${theme.source}${theme.name === original ? " · current" : ""}`),
      ),
    );
    list.replaceChildren(...rows);
    highlight(true);
  };
  const preview = () => {
    const theme = shown[index];
    if (theme) applyTheme(theme.name);
  };
  const save = async (name: string) => {
    saved = true;
    close();
    applyTheme(name);
    if (state.meta) state.meta.theme = name;
    try {
      const result = await post<{ status: string }>("/api/theme", { name });
      toast(result.status, "success");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };
  input.addEventListener("input", () => {
    index = 0;
    render();
    preview();
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      index = Math.max(0, Math.min(shown.length - 1, index + (event.key === "ArrowDown" ? 1 : -1)));
      highlight(true);
      preview();
    } else if (event.key === "Enter") {
      event.preventDefault();
      const theme = shown[index];
      if (theme) void save(theme.name);
    } else if (event.key === "Escape") close();
  });
  const close = openDialog(h("div", { class: "palette" }, h("div", { class: "dialog-title" }, "Theme · shared with ruddr tui"), input, list), {
    className: "top",
    onClose: () => {
      if (!saved) applyTheme(original);
    },
  });
  render();
  input.focus();
}

function openHelp(): void {
  const rows: Array<[string, string]> = [
    ["⌘K  :", "Command palette"],
    ["?", "This list"],
    ["j / k  ↑ / ↓", "Move between sessions"],
    ["1 2 3 4  o", "Chat, Activity, Output, Diff; o cycles"],
    ["s  Enter", "Focus the prompt box"],
    ["n", "New session"],
    ["m", "Choose the model"],
    ["R", "Continue a finished thread"],
    ["x x", "Interrupt the turn, or end an idle session"],
    ["i", "Session details"],
    ["/", "Search this pane, then Enter and Shift+Enter"],
    ["F", "Filter sessions"],
    ["f", "Find a past session with deja"],
    ["t", "Theme"],
    ["r", "Refresh sessions"],
    ["c", "Copy the last agent message"],
    ["Z  [  ]", "Diff: fold all, previous and next file"],
    ["End", "Follow the live chat"],
    ["Esc", "Close, or go back on a phone"],
  ];
  const close = openDialog(
    h(
      "div",
      { class: "help" },
      h("div", { class: "dialog-title" }, "Keyboard shortcuts"),
      h("div", { class: "help-grid" }, ...rows.flatMap(([keys, action]) => [h("kbd", null, keys), h("span", null, action)])),
      h("div", { class: "dialog-actions" }, h("span", { class: "spacer" }), h("button", { class: "btn ghost", onclick: () => close() }, "Close")),
    ),
  );
}

// ---------------------------------------------------------------------------
// Pane search

let searchMatches: HTMLElement[] = [];
let searchIndex = 0;

function openSearch(): void {
  searchBar.classList.remove("hidden");
  searchInput.focus();
  searchInput.select();
}

function closeSearch(): void {
  searchBar.classList.add("hidden");
  for (const element of searchMatches) element.classList.remove("search-hit", "search-current");
  searchMatches = [];
  searchCount.textContent = "";
}

function runSearch(): void {
  for (const element of searchMatches) element.classList.remove("search-hit", "search-current");
  const needle = searchInput.value.trim().toLowerCase();
  if (!needle) {
    searchMatches = [];
    searchCount.textContent = "";
    return;
  }
  const scope = panes[state.tab];
  searchMatches = [...scope.querySelectorAll<HTMLElement>("[data-search], .diff-file, .md > *")].filter((element) =>
    (element.dataset.search ?? element.dataset.path ?? element.textContent ?? "").toLowerCase().includes(needle),
  );
  for (const element of searchMatches) element.classList.add("search-hit");
  searchIndex = searchMatches.length - 1;
  moveSearch(0);
}

function moveSearch(delta: number): void {
  if (!searchMatches.length) {
    searchCount.textContent = searchInput.value ? "0" : "";
    return;
  }
  searchMatches[searchIndex]?.classList.remove("search-current");
  searchIndex = (searchIndex + delta + searchMatches.length) % searchMatches.length;
  const current = searchMatches[searchIndex];
  current.classList.add("search-current");
  current.scrollIntoView({ block: "center", behavior: "smooth" });
  searchCount.textContent = `${searchIndex + 1}/${searchMatches.length}`;
}

let searchTimer: ReturnType<typeof setTimeout> | undefined;
searchInput.addEventListener("input", () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(runSearch, 120);
});
searchInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    moveSearch(event.shiftKey ? -1 : 1);
  } else if (event.key === "Escape") closeSearch();
});

// ---------------------------------------------------------------------------
// Mobile

const mobileQuery = matchMedia("(max-width: 760px)");

function isMobile(): boolean {
  return mobileQuery.matches;
}

function applyMobileView(): void {
  document.body.dataset.view = isMobile() ? (selectedSession() ? state.mobileView : "list") : "split";
  autosize();
  renderTabs();
}

function showMobileList(): void {
  state.mobileView = "list";
  transition(applyMobileView, "pop");
}

mobileQuery.addEventListener("change", () => {
  applyMobileView();
  renderTabs();
});

// ---------------------------------------------------------------------------
// Keyboard

function focusComposer(): void {
  if (isMobile() && state.mobileView !== "session") {
    state.mobileView = "session";
    applyMobileView();
  }
  if (!composerInput.disabled) composerInput.focus();
  else toast("This session cannot take a prompt", "warning");
}

/** `m`: the continuation model for a finished session, otherwise a new session's model. */
function chooseModel(): void {
  if (promptRoute(selectedSession()) === "continue") {
    composerModel.open();
  } else openNewSession();
}

function focusFilter(): void {
  if (isMobile()) showMobileList();
  filterInput.focus();
}

function copyLastMessage(): void {
  const selection = getSelection()?.toString();
  const text = selection || chat.lastAgentText();
  if (!text) return toast("Nothing to copy yet", "warning");
  void copyText(text).then((ok) => toast(ok ? (selection ? "Selection copied" : "Last agent message copied") : "Copy failed", ok ? "success" : "error"));
}

function moveSelection(delta: number): void {
  const sessions = visibleSessions();
  const ordered = [...sessions.filter(isLive), ...sessions.filter((session) => !isLive(session))];
  if (!ordered.length) return;
  const current = ordered.findIndex((session) => session.stateDir === state.selected);
  const next = ordered[Math.max(0, Math.min(ordered.length - 1, current + delta))];
  selectSession(next.stateDir, false);
  sessionRows.get(next.stateDir)?.scrollIntoView({ block: "nearest" });
}

function typingTarget(target: EventTarget | null): boolean {
  const element = target as HTMLElement | null;
  if (!element) return false;
  return element.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(element.tagName);
}

document.addEventListener("keydown", (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    if (activeDialog) activeDialog.close();
    else openPalette();
    return;
  }
  if (event.key === "Escape" && activeDialog) {
    activeDialog.close();
    return;
  }
  if (activeDialog || typingTarget(event.target) || event.metaKey || event.ctrlKey || event.altKey) return;
  const key = event.key;
  const handled = (() => {
    switch (key) {
      case ":":
        openPalette();
        return true;
      case "?":
        openHelp();
        return true;
      case "j":
      case "ArrowDown":
        moveSelection(1);
        return true;
      case "k":
      case "ArrowUp":
        moveSelection(-1);
        return true;
      case "1":
      case "2":
      case "3":
      case "4":
        setTab(TABS[Number(key) - 1]);
        return true;
      case "o":
        setTab(TABS[(TABS.indexOf(state.tab) + 1) % TABS.length]);
        return true;
      case "s":
      case "Enter":
      case "R":
        focusComposer();
        return true;
      case "n":
        openNewSession();
        return true;
      case "m":
        chooseModel();
        return true;
      case "f":
        openDeja();
        return true;
      case "F":
        focusFilter();
        return true;
      case "x":
        void requestStop();
        return true;
      case "i":
        toggleDetails();
        return true;
      case "/":
        openSearch();
        return true;
      case "r":
        void refreshSessions().then(() => toast("Sessions refreshed"));
        return true;
      case "t":
        openThemePicker();
        return true;
      case "c":
        copyLastMessage();
        return true;
      case "Z":
        diffView.toggleAll();
        return true;
      case "]":
        if (state.tab === "diff") diffView.jumpFile(1);
        return true;
      case "[":
        if (state.tab === "diff") diffView.jumpFile(-1);
        return true;
      case "End":
        chat.follow();
        return true;
      case "Escape":
        if (!searchBar.classList.contains("hidden")) closeSearch();
        else if (isMobile() && state.mobileView === "session") showMobileList();
        return true;
      default:
        return false;
    }
  })();
  if (handled) event.preventDefault();
});

filterInput.addEventListener("input", () => {
  state.filter = filterInput.value;
  renderSessions();
});
filterInput.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    filterInput.value = "";
    state.filter = "";
    renderSessions();
    filterInput.blur();
  } else if (event.key === "Enter") {
    const first = visibleSessions()[0];
    if (first) selectSession(first.stateDir, true);
    filterInput.blur();
  }
});

// ---------------------------------------------------------------------------
// Connection and boot

let sessionStream: EventSource | undefined;

async function refreshSessions(): Promise<void> {
  try {
    applySessions(await get<Session[]>("/api/sessions"));
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) showLogin();
  }
}

/** Turns that end while the page is in the background raise a notification. */
function notifyFinishedTurns(before: Session[], after: Session[]): void {
  if (!document.hidden || typeof Notification === "undefined" || Notification.permission !== "granted") return;
  const was = new Map(before.map((session) => [session.stateDir, session.status]));
  for (const session of after) {
    if (was.get(session.stateDir) !== "active" || session.status === "active") continue;
    const verb = session.status === "idle" ? "finished its turn" : session.status;
    const note = new Notification(`${projectName(session)} ${verb}`, {
      body: `${session.provider ?? "codex"} · ${session.model || "default"}`,
      tag: session.stateDir,
    });
    note.onclick = () => {
      window.focus();
      selectSession(session.stateDir, true);
      note.close();
    };
  }
}

async function enableNotifications(): Promise<void> {
  if (typeof Notification === "undefined") return toast("This browser has no notifications", "warning");
  const permission = await Notification.requestPermission();
  toast(permission === "granted" ? "Notifications on: finished turns notify you while this tab is hidden" : "Notifications are blocked for this page", permission === "granted" ? "success" : "warning");
}

/** The tab title carries the live count and the selected session's state. */
function renderTitle(): void {
  const session = selectedSession();
  const live = state.sessions.filter((candidate) => candidate.status === "active").length;
  const prefix = live ? `● ${live} · ` : "";
  document.title = session ? `${prefix}${projectName(session)} ${statusGlyph(session.status)} · ruddr` : `${prefix}ruddr · ${state.meta?.hostname ?? ""}`;
}

function applySessions(sessions: Session[]): void {
  const previous = selectedSession();
  notifyFinishedTurns(state.sessions, sessions);
  state.sessions = sessions;
  if (!selectedSession()) {
    const fallback = sessions.find(isLive) ?? sessions[0];
    selectSession(fallback?.stateDir);
  } else {
    const current = selectedSession()!;
    // A finished turn changes the prompt route and the Stop button.
    if (previous?.status !== current.status && previous?.status === "active" && isTerminal(current.status))
      toast(`${projectName(current)} ${current.status}`, current.status === "completed" ? "success" : "warning");
    renderSessions();
    renderHeader();
    if (previous?.threadId !== current.threadId) connectRun();
  }
  renderTitle();
}

function connectSessions(): void {
  sessionStream?.close();
  const stream = new EventSource("/api/sessions/stream");
  sessionStream = stream;
  stream.addEventListener("open", () => {
    if (stream !== sessionStream) return;
    state.connected = true;
    connection.classList.add("on");
    connection.title = "Live";
  });
  stream.addEventListener("error", () => {
    if (stream !== sessionStream) return;
    state.connected = false;
    connection.classList.remove("on");
    connection.title = "Reconnecting…";
    // EventSource retries on its own; a 401 means the cookie is gone.
    void get("/api/meta").catch((error) => {
      if (error instanceof ApiError && error.status === 401) {
        stream.close();
        showLogin();
      }
    });
  });
  stream.addEventListener("sessions", (event) => {
    if (stream === sessionStream) applySessions(JSON.parse((event as MessageEvent).data));
  });
}

function showLogin(): void {
  sessionStream?.close();
  runStream?.close();
  sessionStream = undefined;
  runStream = undefined;
  activityRead++; outputRead++; diffRead++; branchRead++;
  clearTimeout(activityTimer); clearTimeout(outputTimer); clearTimeout(diffTimer);
  const input = h("input", { class: "field", type: "password", placeholder: "Paste the token from `ruddr web`", autocomplete: "current-password" });
  const form = h(
    "form",
    { class: "login" },
    h("div", { class: "empty-logo" }, "⎈"),
    h("h2", null, "Connect to Ruddr"),
    h("p", { class: "dim" }, "Open the link that `ruddr web` printed, or paste its token. The token lives in ~/.config/ruddr/web-token on the host."),
    input,
    h("button", { class: "btn primary", type: "submit" }, "Connect"),
  );
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    try {
      await post("/api/login", { token: input.value });
      location.reload();
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  });
  root.replaceChildren(h("div", { class: "login-wrap" }, form), toasts);
  input.focus();
}

let tickCount = 0;
setInterval(() => {
  tickCount++;
  if (!state.meta) return;
  const session = selectedSession();
  if (session && isLive(session)) renderHeader();
  if (tickCount % 10 === 0) renderSessions();
}, 1000);

window.addEventListener("resize", () => {
  renderTabs();
  placeGlide();
});

// Braille spinners and the follow indicator. Text-only updates, no layout.
let spinFrame = 0;
let followShown: boolean | undefined;
setInterval(() => {
  spinFrame = (spinFrame + 1) % SPINNER.length;
  for (const element of document.querySelectorAll<HTMLElement>(".spin")) element.textContent = SPINNER[spinFrame];
  const following = chat.isFollowing();
  if (following !== followShown) {
    followShown = following;
    followHint.textContent = following ? "● live" : "‖ paused · End";
    followHint.classList.toggle("paused", !following);
  }
  followHint.classList.toggle("hidden", state.tab !== "chat");
}, 80);

async function boot(): Promise<void> {
  try {
    state.meta = await get<Meta>("/api/meta");
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) return showLogin();
    toast(error instanceof Error ? error.message : String(error), "error");
    return;
  }
  connection.title = `Live · ${state.meta.hostname}`;
  renderTitle();
  if (state.meta.updateAvailable) {
    updateBadge.textContent = `Update ${state.meta.updateAvailable}`;
    updateBadge.classList.remove("hidden");
  }
  applyTheme(state.meta.theme);
  for (const tab of TABS) panes[tab].classList.toggle("active", tab === state.tab);
  renderTabs();
  applyMobileView();
  if (isMobile()) state.mobileView = "list";
  applyMobileView();
  await refreshSessions();
  connectSessions();
  void loadModels();
  requestAnimationFrame(renderTabs);
}

void boot();
