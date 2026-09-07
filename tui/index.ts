import {
  BoxRenderable,
  bold,
  createCliRenderer,
  fg,
  InputRenderable,
  InputRenderableEvents,
  italic,
  parseColor,
  ScrollBoxRenderable,
  SelectRenderable,
  SelectRenderableEvents,
  StyledText,
  t,
  TextareaRenderable,
  TextRenderable,
  underline,
} from "@opentui/core";
import {
  chmod,
  mkdir,
  mkdtemp,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  artifactAllowsTextSelection,
  AsyncTaskGate,
  LatestRead,
  attachToolDetails,
  contextUsageFromEvents,
  deleteSessionArtifacts,
  filterPaletteCommands,
  formatElapsed,
  typewriterReveal,
  helpSegments,
  spinnerFrame,
  statusGlyphForKind,
  statusTimeoutMs,
  compactSessionDetails,
  continuationRunArguments,
  contextualHelp,
  dashboardNavigation,
  DEFAULT_MOBILE_WIDTH_THRESHOLD,
  layoutForWidth,
  discoverSessions,
  emptyPromptHint,
  filterSessions,
  initialViewState,
  idlePromptControlArguments,
  latestAgentUpdate,
  modelPickerOptions,
  newSessionRunArguments,
  nextArtifact,
  parseArguments,
  parseChatTranscript,
  parseDejaHits,
  parseModelCatalog,
  parseToolEventDetails,
  parseTraceActivities,
  promptModeForSession,
  promptTargetForSession,
  readTail,
  reduceView,
  resolvePromptTarget,
  sessionDescription,
  sessionDetails,
  sessionIsDeletable,
  sessionLabel,
  sessionsPanelTitle,
  statusGlyph,
  steerControlArguments,
  visibleArtifactTail,
  visibleSessions,
  FALLBACK_MODELS,
  type Artifact,
  type DejaHit,
  type ModelInfo,
  type PromptTarget,
  type Session,
  type PaletteCommand,
  type StatusKind,
  type TUILayout,
  type ViewState,
} from "./core";
import {
  defaultThemeName,
  findTheme,
  persistTheme,
  persistTUIConfig,
  readTUIConfig,
  resolveThemeName,
  themes,
} from "./themes";
import { readWorkspaceDiff, touchedSince, diffCache } from "./git";
import { diffTints, palette, setPalette } from "./palette";
import { errorMessage, runControl } from "./process";
import {
  activityCopyText,
  activitySearchText,
  codeChunks,
  formatDuration,
  isLive,
  listScrollOffset,
  renderChatEntry,
  renderSessionDetails,
  renderUsage,
  renderToolDetail,
  renderTraceActivity,
  scrollListBy,
  spanChunks,
  TOOL_OUTPUT_LINES,
} from "./render";
import { LIVE_ROW_ID, type ActivityRow } from "./rows";
import { createDiffView } from "./diff-view";
import { createContextMenu, type MenuItem } from "./menu";


const ACTIVITY_HISTORY_LIMIT = 200;
const OUTPUT_HISTORY_LINES = 1_000;
const ARTIFACT_TAIL_BYTES = 1024 * 1024;


const ANIMATION_INTERVAL_MS = 100;


// The TUI resolves "auto" once when the input opens. A status change can then
// reject the captured command, but it can never convert prompt into steer.
type PromptMode = "steer" | "prompt" | "continue" | "new";
type SearchMode = "sessions" | "artifact" | "deja";

async function main(): Promise<void> {
  const args = parseArguments(Bun.argv.slice(2));
  const launchLayout: TUILayout = args.beta ? "beta" : "classic";
  let layout: TUILayout = launchLayout;
  let mobile = false;
  const persistedConfig = await readTUIConfig();
  let activeThemeName = resolveThemeName(
    args.theme || process.env.RUDDR_TUI_THEME || process.env.RUDDER_TUI_THEME,
    persistedConfig.theme,
  );
  setPalette(findTheme(activeThemeName)!.palette);
  const renderer = await createCliRenderer({
    screenMode: "alternate-screen",
    consoleMode: "disabled",
    exitOnCtrlC: false,
    exitSignals: [],
    targetFps: 15,
    backgroundColor: palette.background,
    useMouse: true,
    enableMouseMovement: true,
  });
  let destroyed = false;

  try {
    let discoveredSessions: Session[] = [];
    let listedSessions: Session[] = [];
    let sessions: Session[] = [];
    let view: ViewState = initialViewState;
    const refreshGate = new AsyncTaskGate();
    const artifactReads = new LatestRead();
    let actionRunning = false;
    let detailsExpanded = false;
    let sessionQuery = "";
    const artifactQueries: Record<Artifact, string> = {
      chat: "",
      trace: "",
      output: "",
      diff: "",
    };
    let searchMode: SearchMode | undefined;
    let promptMode: PromptMode | undefined;
    let promptTarget: PromptTarget | undefined;
    let sessionsVisible = false;
    let modelPickerOpen = false;
    let dejaPickerOpen = false;
    let models: ModelInfo[] = FALLBACK_MODELS;
    let pendingModel: ModelInfo | undefined;
    let pendingResume: DejaHit | undefined;
    let dejaHits: DejaHit[] = [];
    const dejaAvailable = Boolean(Bun.which("deja"));
    let artifactRows: ActivityRow[] = [];
    let rowRenderables: TextRenderable[] = [];
    let selectedRow = -1;
    let expandedToolIDs = new Set<string>();
    let artifactSignature = "";
    let artifactKey = "";
    let artifactFollowing = true;
    let unseenRows = 0;
    let previousRowCount = 0;
    let themePickerOpen = false;
    let themeBeforePicker = activeThemeName;
    // Typewriter reveal for the newest agent message while a session works.
    let stream: { id: string; revealed: number; target: number } | undefined;
    const seenAgentLength = new Map<string, number>();
    let animationTick = 0;
    let statusState: { message: string; kind: StatusKind; idle: boolean } = {
      message: "",
      kind: "info",
      idle: true,
    };
    let statusTimer: ReturnType<typeof setTimeout> | undefined;
    let lastRefreshAt = 0;

    const sessionList = new SelectRenderable(renderer, {
      id: "sessions",
      flexGrow: 1,
      options: [],
      showDescription: true,
      wrapSelection: true,
      backgroundColor: palette.panel,
      focusedBackgroundColor: palette.panel,
      textColor: palette.text,
      focusedTextColor: palette.text,
      descriptionColor: palette.dim,
      selectedDescriptionColor: palette.text,
      selectedBackgroundColor: palette.selected,
      selectedTextColor: palette.accent,
      showScrollIndicator: true,
      renderAfter(buffer) {
        const linesPerItem = 2;
        const visibleItems = Math.max(
          1,
          Math.floor(this.height / linesPerItem),
        );
        const scrollOffset = listScrollOffset(this);
        const accent = parseColor(palette.accent);
        const success = parseColor(palette.success);
        const warning = parseColor(palette.warning);
        const danger = parseColor(palette.danger);
        const labelX = 3;

        const dimColor = parseColor(palette.dim);
        for (const [visibleIndex, session] of sessions
          .slice(scrollOffset, scrollOffset + visibleItems)
          .entries()) {
          const index = scrollOffset + visibleIndex;
          const live = isLive(session);
          const previous = sessions[index - 1];
          const firstOfGroup = !previous || isLive(previous) !== live;
          const statusColor =
            session.status === "active" || session.status === "completed"
              ? success
              : session.status === "starting" || session.status === "idle"
                ? warning
                : danger;
          const rowY = visibleIndex * linesPerItem;
          const provider = session.provider ?? "codex";

          buffer.drawText(
            session.status === "active" || session.status === "starting"
              ? spinnerFrame(animationTick)
              : statusGlyph(session.status),
            labelX,
            rowY,
            statusColor,
          );
          buffer.drawText(provider, labelX, rowY + 1, accent);
          buffer.drawText(
            session.status,
            labelX + provider.length + 3,
            rowY + 1,
            statusColor,
          );
          if (firstOfGroup) {
            const tag = live ? "LIVE" : "RECENT";
            const age = sessionLabel(session).split("  ").pop() ?? "";
            const tagX =
              labelX + sessionCardInnerWidth() - [...age].length - 1 - tag.length - 1;
            buffer.drawText(tag, Math.max(labelX, tagX), rowY, live ? success : dimColor);
          }
        }
      },
    });
    const sessionsPanel = new BoxRenderable(renderer, {
      position: "relative",
      width: "34%",
      minWidth: 36,
      maxWidth: 44,
      height: "100%",
      flexShrink: 0,
      zIndex: 0,
      border: true,
      borderStyle: "rounded",
      borderColor: palette.border,
      focusedBorderColor: palette.accent,
      title: sessionsPanelTitle({
        layout,
        liveCount: 0,
        recentCount: 0,
      }),
      padding: 1,
      backgroundColor: palette.background,
      visible: true,
    });
    sessionsPanel.add(sessionList);

    const details = new TextRenderable(renderer, {
      content: "No session selected",
      fg: palette.text,
      width: "100%",
    });
    const detailsPanel = new BoxRenderable(renderer, {
      width: "100%",
      height: 8,
      border: true,
      borderStyle: "rounded",
      borderColor: palette.border,
      title: " Session · i hide ",
      padding: 1,
      backgroundColor: palette.background,
      visible: true,
    });
    detailsPanel.add(details);

    const chatTab = new TextRenderable(renderer, {
      content: "chat",
      fg: palette.accent,
    });
    const activityTab = new TextRenderable(renderer, {
      content: "activity",
      fg: palette.dim,
    });
    const outputTab = new TextRenderable(renderer, {
      content: "output",
      fg: palette.dim,
    });
    const diffTab = new TextRenderable(renderer, {
      content: "diff",
      fg: palette.dim,
    });
    const tabsLeft = new BoxRenderable(renderer, {
      height: 1,
      flexDirection: "row",
      gap: 2,
    });
    tabsLeft.add(chatTab);
    tabsLeft.add(activityTab);
    tabsLeft.add(outputTab);
    tabsLeft.add(diffTab);
    // Only speaks up when live-follow is paused; silence is the default.
    const followIndicator = new TextRenderable(renderer, {
      content: "",
      fg: palette.warning,
    });
    // Live pulse for the selected session: spinner + elapsed while it works.
    const workingIndicator = new TextRenderable(renderer, {
      content: "",
      fg: palette.success,
    });
    const tabsRight = new BoxRenderable(renderer, {
      height: 1,
      flexDirection: "row",
      gap: 2,
    });
    tabsRight.add(workingIndicator);
    tabsRight.add(followIndicator);
    const tabsBar = new BoxRenderable(renderer, {
      width: "100%",
      height: 1,
      flexDirection: "row",
      justifyContent: "space-between",
      paddingLeft: 1,
      paddingRight: 1,
    });
    tabsBar.add(tabsLeft);
    tabsBar.add(tabsRight);

    const artifactScroll = new ScrollBoxRenderable(renderer, {
      id: "artifact",
      width: "100%",
      flexGrow: 1,
      scrollY: true,
      scrollX: true,
      stickyScroll: true,
      stickyStart: "bottom",
      viewportCulling: false,
      verticalScrollbarOptions: {
        trackOptions: {
          foregroundColor: palette.accent,
          backgroundColor: palette.border,
        },
      },
    });
    const artifactBody = new BoxRenderable(renderer, {
      width: "100%",
      flexGrow: 1,
      flexDirection: "row",
      gap: 0,
    });
    const diffView = createDiffView({
      renderer,
      artifactScroll,
      artifactBody,
      isActive: () => view.artifact === "diff",
      rows: () => artifactRows,
      selectedRow: () => selectedRow,
      setSelectedRow: (index) => {
        selectedRow = index;
      },
      setRows: (rows) => setArtifactRows(rows),
      renderRows: (preserveScroll) => renderArtifactRows(preserveScroll),
      refreshRow: (index) => refreshArtifactRow(index),
      stopFollowing: () => {
        artifactFollowing = false;
        artifactScroll.stickyScroll = false;
      },
      invalidateRows: () => {
        artifactSignature = "";
      },
      reloadSelected: () => void updateSelectedSession(),
      activateSelectedRow: () => activateSelectedRow(),
      selectedSession: () => selectedSession(),
      setStatus: (message, kind) => setStatus(message, kind),
      updateChrome: () => updateChrome(),
      copyText: (value) => copyText(value),
      persistSidebar: (width, ratio) => {
        persistedConfig.diffTreeRatio = ratio;
        persistedConfig.diffTreeWidth = width;
        void persistTUIConfig({
          ...persistedConfig,
          theme: activeThemeName,
          diffTreeWidth: width,
          ...(ratio !== undefined ? { diffTreeRatio: ratio } : {}),
        }).catch((error) =>
          setStatus(`Sidebar resized, but could not save: ${errorMessage(error)}`, true),
        );
      },
      initialTreeWidth: Math.max(20, Math.min(60, persistedConfig.diffTreeWidth ?? 30)),
      initialTreeRatio: persistedConfig.diffTreeRatio,
    });
    artifactBody.add(artifactScroll);
    // Borderless main surface: the conversation floats on the background the
    // way opencode's does; the prompt input is the only framed element.
    const artifactPanel = new BoxRenderable(renderer, {
      width: "100%",
      flexGrow: 1,
      flexDirection: "column",
      gap: 1,
      paddingLeft: 1,
      paddingRight: 1,
      paddingTop: 0,
    });
    artifactPanel.add(tabsBar);
    artifactPanel.add(artifactBody);

    const rightColumn = new BoxRenderable(renderer, {
      minWidth: 50,
      height: "100%",
      flexGrow: 1,
      flexDirection: "column",
      gap: 1,
      backgroundColor: palette.background,
    });
    rightColumn.add(detailsPanel);
    rightColumn.add(artifactPanel);

    const body = new BoxRenderable(renderer, {
      width: "100%",
      flexGrow: 1,
      flexDirection: "row",
      gap: 1,
    });
    body.add(sessionsPanel);
    body.add(rightColumn);
    let sessionsParent: BoxRenderable = body;

    // Meta line under the input: mode + model on the left, tokens/cost right.
    const promptMetaLeft = new TextRenderable(renderer, {
      content: "",
      fg: palette.dim,
      height: 1,
      flexGrow: 1,
      flexShrink: 1,
      wrapMode: "none",
      truncate: true,
    });
    const promptMetaRight = new TextRenderable(renderer, {
      content: "",
      fg: palette.dim,
      height: 1,
      flexShrink: 0,
      wrapMode: "none",
      truncate: true,
    });
    const promptMeta = new BoxRenderable(renderer, {
      width: "100%",
      height: 1,
      flexDirection: "row",
      justifyContent: "space-between",
      paddingLeft: 1,
      paddingRight: 1,
    });
    promptMeta.add(promptMetaLeft);
    promptMeta.add(promptMetaRight);

    const statusLine = new TextRenderable(renderer, {
      content: "",
      fg: palette.dim,
      height: 1,
      paddingLeft: 1,
    });
    const footerLeft = new TextRenderable(renderer, {
      content: "",
      fg: palette.dim,
      height: 1,
      width: "34%",
      flexShrink: 1,
      wrapMode: "none",
      truncate: true,
    });
    const footerRight = new TextRenderable(renderer, {
      content: "",
      fg: palette.dim,
      height: 1,
      width: "64%",
      flexGrow: 1,
      flexShrink: 1,
      paddingLeft: 1,
      wrapMode: "none",
      truncate: true,
    });
    const footerBar = new BoxRenderable(renderer, {
      width: "100%",
      height: 1,
      flexDirection: "row",
      justifyContent: "flex-start",
      gap: 1,
      paddingLeft: 1,
      paddingRight: 1,
    });
    footerBar.add(footerLeft);
    footerBar.add(footerRight);

    // Mobile action bar: the keys a phone cannot press, as tappable buttons.
    const actionButtons: Array<[TextRenderable, () => void]> = [];
    const actionBar = new BoxRenderable(renderer, {
      width: "100%",
      height: 1,
      flexDirection: "row",
      justifyContent: "space-around",
      gap: 1,
      visible: false,
    });
    const makeAction = (label: string, run: () => void) => {
      const button = new TextRenderable(renderer, {
        content: label,
        fg: palette.accent,
        wrapMode: "none",
      });
      button.onMouseDown = (event) => {
        event.preventDefault();
        run();
      };
      actionButtons.push([button, run]);
      actionBar.add(button);
      return button;
    };
    const actionSessions = makeAction(" ≡ sessions ", () => showSessions());
    const actionPrompt = makeAction(" ✎ prompt ", () => {
      if (promptTargetForSession(selectedSession())) openPrompt("auto");
      else startNewSessionFlow();
    });
    const actionStop = makeAction(" ■ stop ", () => void requestInterrupt());
    const actionMore = makeAction(" ⋯ more ", () => openPalette());

    const searchInput = new InputRenderable(renderer, {
      id: "search-input",
      width: "100%",
      placeholder: "Filter project, thread, status, or model…",
      backgroundColor: palette.panel,
      focusedBackgroundColor: palette.selected,
      textColor: palette.text,
      focusedTextColor: palette.text,
      placeholderColor: palette.dim,
    });
    const searchPanel = new BoxRenderable(renderer, {
      position: "absolute",
      left: "18%",
      top: "35%",
      width: "64%",
      height: 5,
      zIndex: 10,
      border: true,
      borderStyle: "double",
      borderColor: palette.accent,
      title: " Search · Enter keep · Esc clear ",
      padding: 1,
      backgroundColor: palette.background,
      visible: false,
    });
    searchPanel.add(searchInput);

    const PROMPT_MIN_ROWS = 1;
    const PROMPT_MAX_ROWS = 6;
    const promptInput = new TextareaRenderable(renderer, {
      id: "prompt-input",
      width: "100%",
      height: PROMPT_MIN_ROWS,
      placeholder: "Message for the selected provider…",
      backgroundColor: palette.panel,
      focusedBackgroundColor: palette.selected,
      textColor: palette.text,
      focusedTextColor: palette.text,
      placeholderColor: palette.dim,
      cursorColor: palette.accent,
      wrapMode: "word",
      keyBindings: [
        { name: "return", action: "submit" },
        { name: "return", shift: true, action: "newline" },
        { name: "return", meta: true, action: "newline" },
        { name: "j", ctrl: true, action: "newline" },
      ],
      onSubmit: () => {
        if (promptMode) void submitPrompt();
      },
      onContentChange: () => fitPromptHeight(),
    });
    const promptPanel = new BoxRenderable(renderer, {
      width: "100%",
      height: PROMPT_MIN_ROWS + 2,
      border: true,
      borderStyle: "rounded",
      borderColor: palette.border,
      focusedBorderColor: palette.accent,
      paddingLeft: 1,
      paddingRight: 1,
      backgroundColor: palette.background,
    });
    promptPanel.add(promptInput);
    function fitPromptHeight(): void {
      const rows = Math.max(
        PROMPT_MIN_ROWS,
        Math.min(PROMPT_MAX_ROWS, promptInput.lineCount || 1),
      );
      if (promptInput.height !== rows) {
        promptInput.height = rows;
        promptPanel.height = rows + 2;
      }
    }

    // Command palette: every action, searchable, with its key beside it.
    const paletteInput = new InputRenderable(renderer, {
      id: "palette-input",
      width: "100%",
      placeholder: "Type a command…",
      backgroundColor: palette.panel,
      focusedBackgroundColor: palette.selected,
      textColor: palette.text,
      focusedTextColor: palette.text,
      placeholderColor: palette.dim,
    });
    const paletteList = new SelectRenderable(renderer, {
      id: "palette-list",
      width: "100%",
      flexGrow: 1,
      options: [],
      showDescription: true,
      wrapSelection: true,
      backgroundColor: palette.background,
      focusedBackgroundColor: palette.background,
      textColor: palette.text,
      focusedTextColor: palette.text,
      descriptionColor: palette.dim,
      selectedDescriptionColor: palette.text,
      selectedBackgroundColor: palette.selected,
      selectedTextColor: palette.accent,
      showScrollIndicator: true,
    });
    const palettePanel = new BoxRenderable(renderer, {
      position: "absolute",
      left: "22%",
      top: "12%",
      width: "56%",
      height: "70%",
      zIndex: 25,
      border: true,
      borderStyle: "rounded",
      borderColor: palette.accent,
      title: " Commands · Enter run · Esc close ",
      padding: 1,
      gap: 1,
      flexDirection: "column",
      backgroundColor: palette.background,
      visible: false,
    });
    palettePanel.add(paletteInput);
    palettePanel.add(paletteList);
    let paletteOpen = false;
    let paletteCommands: PaletteCommand[] = [];

    const contextMenu = createContextMenu(renderer, () => focusCurrentPanel());

    const modelPicker = new SelectRenderable(renderer, {
      id: "model-picker",
      width: "100%",
      flexGrow: 1,
      options: [],
      showDescription: true,
      wrapSelection: true,
      backgroundColor: palette.panel,
      focusedBackgroundColor: palette.panel,
      textColor: palette.text,
      focusedTextColor: palette.text,
      descriptionColor: palette.dim,
      selectedDescriptionColor: palette.text,
      selectedBackgroundColor: palette.selected,
      selectedTextColor: palette.accent,
      showScrollIndicator: true,
    });
    const modelPanel = new BoxRenderable(renderer, {
      position: "absolute",
      left: "27%",
      top: "18%",
      width: "46%",
      height: "64%",
      zIndex: 20,
      border: true,
      borderStyle: "double",
      borderColor: palette.accent,
      title: " Model · Enter choose · Esc cancel ",
      padding: 1,
      backgroundColor: palette.background,
      visible: false,
    });
    modelPanel.add(modelPicker);

    const dejaPicker = new SelectRenderable(renderer, {
      id: "deja-picker",
      width: "100%",
      flexGrow: 1,
      options: [],
      showDescription: true,
      wrapSelection: true,
      backgroundColor: palette.panel,
      focusedBackgroundColor: palette.panel,
      textColor: palette.text,
      focusedTextColor: palette.text,
      descriptionColor: palette.dim,
      selectedDescriptionColor: palette.text,
      selectedBackgroundColor: palette.selected,
      selectedTextColor: palette.accent,
      showScrollIndicator: true,
    });
    const dejaPanel = new BoxRenderable(renderer, {
      position: "absolute",
      left: "12%",
      top: "14%",
      width: "76%",
      height: "72%",
      zIndex: 20,
      border: true,
      borderStyle: "double",
      borderColor: palette.accent,
      title: " Resume past session · Enter pick · Esc cancel ",
      padding: 1,
      backgroundColor: palette.background,
      visible: false,
    });
    dejaPanel.add(dejaPicker);

    const themePicker = new SelectRenderable(renderer, {
      id: "theme-picker",
      width: "100%",
      flexGrow: 1,
      options: themes.map((theme) => ({
        name: theme.label,
        description:
          theme.source === "OpenCode" ? "OpenCode · dark" : "Ruddr default",
        value: theme.name,
      })),
      showDescription: true,
      wrapSelection: true,
      backgroundColor: palette.panel,
      focusedBackgroundColor: palette.panel,
      textColor: palette.text,
      focusedTextColor: palette.text,
      descriptionColor: palette.dim,
      selectedDescriptionColor: palette.text,
      selectedBackgroundColor: palette.selected,
      selectedTextColor: palette.accent,
      showScrollIndicator: true,
    });
    const themePanel = new BoxRenderable(renderer, {
      position: "absolute",
      left: "27%",
      top: "12%",
      width: "46%",
      height: "76%",
      zIndex: 20,
      border: true,
      borderStyle: "double",
      borderColor: palette.accent,
      title: " Theme · live preview · Enter save · Esc cancel ",
      padding: 1,
      backgroundColor: palette.background,
      visible: false,
    });
    themePanel.add(themePicker);

    const root = new BoxRenderable(renderer, {
      width: "100%",
      height: "100%",
      flexDirection: "column",
      padding: 1,
      backgroundColor: palette.background,
    });
    root.add(body);
    root.add(promptPanel);
    root.add(promptMeta);
    root.add(statusLine);
    root.add(footerBar);
    root.add(actionBar);
    root.add(searchPanel);
    root.add(palettePanel);
    root.add(contextMenu.panel);
    root.add(themePanel);
    root.add(modelPanel);
    root.add(dejaPanel);
    renderer.root.add(root);
    // Mouse events bubble here last; a click anywhere outside the open menu
    // dismisses it. The opening click itself bubbles too, hence the delay.
    root.onMouseDown = (event) => {
      if (!contextMenu.open || contextMenu.ageMs() < 150) return;
      if (!contextMenu.contains(event.x, event.y)) contextMenu.close();
    };
    artifactScroll.focus();
    view = { ...view, focus: "artifact" };

    chatTab.onMouseDown = () => setArtifact("chat");
    activityTab.onMouseDown = () => setArtifact("trace");
    outputTab.onMouseDown = () => setArtifact("output");
    diffTab.onMouseDown = () => setArtifact("diff");
    followIndicator.onMouseDown = () => resumeFollowing();
    promptPanel.onMouseDown = () => openPrompt("auto");
    sessionsPanel.onMouseDown = () => focusSessions();
    modelPanel.onMouseDown = () => modelPicker.focus();
    palettePanel.onMouseDown = () => paletteInput.focus();
    paletteInput.on(InputRenderableEvents.INPUT, (value: string) => {
      if (paletteOpen) fillPalette(value);
    });
    paletteList.on(SelectRenderableEvents.ITEM_SELECTED, () => {
      if (paletteOpen) runPaletteSelection();
    });
    dejaPanel.onMouseDown = () => dejaPicker.focus();
    let sessionScrollOffset: number | undefined;
    sessionList.onMouseScroll = (event) => {
      const steps = Math.max(1, Math.round(event.scroll?.delta ?? 1));
      const direction =
        event.scroll?.direction === "up" ? -1 : event.scroll?.direction === "down" ? 1 : 0;
      if (direction === 0) return;
      sessionScrollOffset = scrollListBy(sessionList, direction * steps, 2);
    };
    sessionList.on(SelectRenderableEvents.SELECTION_CHANGED, () => {
      sessionScrollOffset = undefined;
    });
    sessionList.onMouseDown = (event) => {
      focusSessions();
      const linesPerItem = 2;
      const scrollOffset = listScrollOffset(sessionList);
      const clickedIndex =
        scrollOffset + Math.floor((event.y - sessionList.y) / linesPerItem);
      if (clickedIndex >= 0 && clickedIndex < sessions.length)
        sessionList.setSelectedIndex(clickedIndex);
      if (event.button === 2) {
        event.preventDefault();
        const session = sessions[clickedIndex];
        if (session) contextMenu.show(sessionMenu(session), event.x, event.y, sessionMenuTitle(session));
      }
    };

    function sessionMenuTitle(session: Session): string {
      return sessionLabel(session).split("  ")[0]?.trim() ?? "session";
    }

    function sessionMenu(session: Session): MenuItem[] {
      const items: MenuItem[] = [];
      const target = promptTargetForSession(session);
      if (target)
        items.push({
          label:
            target.route === "steer"
              ? "Steer running turn"
              : target.route === "prompt"
                ? "Send a prompt"
                : "Continue thread in a new run",
          run: () => openPrompt(target.route === "continue" ? "continue" : "auto"),
        });
      if (session.status === "active" || session.status === "idle")
        items.push({
          label: session.status === "idle" ? "End idle session" : "Interrupt turn",
          hint: "asks once more before acting",
          run: () => requestInterrupt(),
        });
      items.push(
        { label: "Open chat", run: () => setArtifact("chat") },
        { label: "Open diff", run: () => setArtifact("diff") },
      );
      if (session.threadId)
        items.push({
          label: "Copy thread ID",
          hint: session.threadId,
          run: () => copyText(session.threadId!),
        });
      items.push({
        label: "Copy state directory",
        hint: session.stateDir,
        run: () => copyText(session.stateDir),
      });
      if (sessionIsDeletable(session))
        items.push({
          label: "Delete session…",
          hint: "removes its state directory and registry entry",
          danger: true,
          run: () => confirmDelete([session], `Delete ${sessionMenuTitle(session)}?`),
        });
      const filtered = sessions.filter(sessionIsDeletable);
      if (sessionQuery.trim() && filtered.length > 1)
        items.push({
          label: `Delete ${filtered.length} finished sessions matching "${sessionQuery.trim()}"…`,
          hint: "live sessions in the filter are kept",
          danger: true,
          run: () => confirmDelete(filtered, `Delete ${filtered.length} sessions matching "${sessionQuery.trim()}"?`),
        });
      const broken = listedSessions.filter(
        (candidate) => candidate.status === "failed" || candidate.status === "stale",
      );
      if (broken.length > 0)
        items.push({
          label: `Delete all ${broken.length} failed or stale sessions…`,
          danger: true,
          run: () => confirmDelete(broken, `Delete ${broken.length} failed or stale sessions?`),
        });
      return items;
    }

    function confirmDelete(targets: Session[], question: string): void {
      const x = contextMenu.left;
      const y = contextMenu.top;
      contextMenu.show(
        [
          {
            label: `Yes, delete ${targets.length === 1 ? "it" : `${targets.length} sessions`}`,
            hint: "cannot be undone",
            danger: true,
            run: () => deleteSessions(targets),
          },
          { label: "Cancel", run: () => undefined },
        ],
        x,
        y,
        question,
      );
    }

    async function deleteSessions(targets: Session[]): Promise<void> {
      if (actionRunning) return;
      actionRunning = true;
      setStatus(`Deleting ${targets.length} ${targets.length === 1 ? "session" : "sessions"}…`);
      let removed = 0;
      const failures: string[] = [];
      for (const target of targets) {
        try {
          const result = await deleteSessionArtifacts(target);
          if (result.removedStateDir || result.removedRegistryEntries > 0) removed++;
          else failures.push(`${sessionMenuTitle(target)}: state file did not match`);
          const explicit = args.stateDirs.indexOf(target.stateDir);
          if (explicit >= 0) args.stateDirs.splice(explicit, 1);
        } catch (error) {
          failures.push(`${sessionMenuTitle(target)}: ${errorMessage(error)}`);
        }
      }
      actionRunning = false;
      if (targets.some((target) => target.stateDir === view.selectedStateDir))
        view = reduceView(view, { type: "select", stateDir: undefined });
      await refresh();
      if (failures.length > 0)
        setStatus(`Deleted ${removed}; ${failures[0]}${failures.length > 1 ? ` (+${failures.length - 1} more)` : ""}`, "warning");
      else setStatus(`Deleted ${removed} ${removed === 1 ? "session" : "sessions"}`, "success");
    }

    function copyText(value: string): void {
      const copied = renderer.copyToClipboardOSC52(value);
      setStatus(
        copied ? "Copied to clipboard" : "Terminal clipboard copy is unavailable",
        copied ? "success" : "error",
      );
    }
    artifactPanel.onMouseDown = () => focusArtifact();
    artifactScroll.onMouseScroll = () => {
      focusArtifact();
      setTimeout(updateFollowFromPosition, 0);
    };
    renderer.on("resize", () => {
      syncLayoutToWidth();
      updateChrome();
      if (view.artifact !== "diff") applySessionFilter();
      renderArtifactRows(true);
    });
    themePanel.onMouseDown = () => themePicker.focus();

    themePicker.on(
      SelectRenderableEvents.SELECTION_CHANGED,
      (_index, option) => {
        if (themePickerOpen && option?.value) applyTheme(option.value);
      },
    );
    // TODO(review): Confirm OpenTUI's keyboard ordering does not deliver Enter to both this ITEM_SELECTED handler and the global keypress handler.
    themePicker.on(SelectRenderableEvents.ITEM_SELECTED, () => {
      if (themePickerOpen) void commitThemePicker();
    });
    modelPicker.on(SelectRenderableEvents.ITEM_SELECTED, () => {
      if (modelPickerOpen) commitModelPicker();
    });
    dejaPicker.on(SelectRenderableEvents.ITEM_SELECTED, () => {
      if (dejaPickerOpen) commitDejaPicker();
    });

    searchInput.on(InputRenderableEvents.INPUT, (value: string) => {
      if (searchMode === "sessions") {
        sessionQuery = value;
        applySessionFilter();
      } else if (searchMode === "artifact") {
        artifactQueries[view.artifact] = value;
        renderArtifactRows(true);
      }
      updateChrome();
    });

    sessionList.on(
      SelectRenderableEvents.SELECTION_CHANGED,
      (_index, option) => {
        const changed = option?.value !== view.selectedStateDir;
        view = reduceView(view, { type: "select", stateDir: option?.value });
        if (changed) {
          if (!promptMode) {
            pendingModel = undefined;
            pendingResume = undefined;
          }
          resetArtifactPosition();
        }
        void updateSelectedSession();
        updateChrome();
      },
    );

    renderer.keyInput.on("keypress", (key) => {
      if (key.ctrl && key.name === "c") {
        key.preventDefault();
        key.stopPropagation();
        void shutdown();
        return;
      }
      if (contextMenu.open) {
        key.preventDefault();
        key.stopPropagation();
        if (key.name === "escape" || key.name === "q") contextMenu.close();
        else if (key.name === "return" || key.name === "enter") contextMenu.run();
        else if (key.name === "up" || key.name === "k") contextMenu.moveUp();
        else if (key.name === "down" || key.name === "j") contextMenu.moveDown();
        return;
      }
      if (paletteOpen) {
        if (key.name === "escape") {
          key.preventDefault();
          key.stopPropagation();
          closePalette();
        } else if (key.name === "return" || key.name === "enter") {
          key.preventDefault();
          key.stopPropagation();
          runPaletteSelection();
        } else if (key.name === "up" || (key.ctrl && key.name === "p")) {
          key.preventDefault();
          key.stopPropagation();
          paletteList.moveUp(1);
        } else if (key.name === "down" || (key.ctrl && key.name === "n")) {
          key.preventDefault();
          key.stopPropagation();
          paletteList.moveDown(1);
        }
        return;
      }
      if (key.ctrl && key.name === "k") {
        key.preventDefault();
        key.stopPropagation();
        if (!promptMode && !searchMode) openPalette();
        return;
      }
      if (themePickerOpen) {
        if (key.name === "escape" || key.name === "t") {
          key.preventDefault();
          key.stopPropagation();
          cancelThemePicker();
        } else if (key.name === "return" || key.name === "enter") {
          key.preventDefault();
          key.stopPropagation();
          void commitThemePicker();
        }
        return;
      }
      if (modelPickerOpen) {
        if (key.name === "escape" || key.name === "m") {
          key.preventDefault();
          key.stopPropagation();
          closeModelPicker();
        } else if (key.name === "return" || key.name === "enter") {
          key.preventDefault();
          key.stopPropagation();
          commitModelPicker();
        }
        return;
      }
      if (dejaPickerOpen) {
        if (key.name === "escape") {
          key.preventDefault();
          key.stopPropagation();
          closeDejaPicker();
        } else if (key.name === "return" || key.name === "enter") {
          key.preventDefault();
          key.stopPropagation();
          commitDejaPicker();
        }
        return;
      }
      if (searchMode) {
        if (key.name === "escape") {
          key.preventDefault();
          key.stopPropagation();
          clearAndCloseSearch();
        } else if (key.name === "return" || key.name === "enter") {
          key.preventDefault();
          key.stopPropagation();
          if (searchMode === "deja") void submitDejaSearch();
          else closeSearch();
        }
        return;
      }
      if (sessionsVisible) {
        if (
          key.name === "escape" ||
          key.name === "tab" ||
          key.name === "return" ||
          key.name === "enter"
        ) {
          key.preventDefault();
          key.stopPropagation();
          hideSessions();
        } else if (key.name === "/") {
          key.preventDefault();
          key.stopPropagation();
          openSearch();
        } else if (key.name === "q") {
          key.preventDefault();
          key.stopPropagation();
          void shutdown();
        }
        return;
      }
      if (promptMode) {
        // Enter submits through the textarea's own binding; Shift/Alt+Enter
        // and Ctrl+J insert newlines there, so only Escape is handled here.
        if (key.name === "escape") {
          key.preventDefault();
          key.stopPropagation();
          closePrompt();
        }
        return;
      }

      if (
        view.focus === "artifact" &&
        view.artifact === "diff" &&
        diffView.handleKey(key)
      ) {
        key.preventDefault();
        key.stopPropagation();
        return;
      }

      const navigation = dashboardNavigation(layout, view.focus, key.name);
      if (navigation) {
        key.preventDefault();
        key.stopPropagation();
        if (navigation === "show-sessions") showSessions();
        else if (navigation === "focus-sessions") focusSessions();
        else focusArtifact();
        return;
      }

      const handled =
        [
          "q",
          "r",
          "tab",
          "o",
          "s",
          "x",
          "i",
          "/",
          "n",
          "m",
          "f",
          "c",
          "return",
          "enter",
          "end",
          "t",
          ":",
          "?",
        ].includes(key.name) ||
        (view.focus === "artifact" && (key.name === "j" || key.name === "k"));
      if (handled) {
        key.preventDefault();
        key.stopPropagation();
      }
      if (key.name === "q") {
        void shutdown();
        return;
      }
      if (key.name === ":" || key.name === "?") openPalette();
      else if (key.name === "t") openThemePicker();
      else if (key.name === "r" && key.shift) openPrompt("continue");
      else if (key.name === "r") void refresh("Refreshing sessions…");
      else if (key.name === "o") setArtifact(nextArtifactView());
      else if (key.name === "s") openPrompt("auto");
      else if (key.name === "n" && !key.shift) {
        if (view.focus === "artifact" && artifactQueries[view.artifact].trim())
          moveToSearchMatch(1);
        else startNewSessionFlow();
      }
      else if (key.name === "m") openModelPicker();
      else if (key.name === "f") openDejaSearch();
      else if (key.name === "x") void requestInterrupt();
      else if (key.name === "i") {
        cycleDetails();
      } else if (key.name === "/") openSearch();
      else if (key.name === "n" && key.shift) moveToSearchMatch(-1);
      else if (key.name === "c") copyCurrentSelection();
      else if (
        (key.name === "return" || key.name === "enter") &&
        view.focus === "artifact"
      )
        activateSelectedRow();
      else if (key.name === "end" && view.focus === "artifact")
        resumeFollowing();
      else if (view.focus === "artifact" && key.name === "j")
        moveSelectedRow(1);
      else if (view.focus === "artifact" && key.name === "k")
        moveSelectedRow(-1);
      else if (
        view.focus === "artifact" &&
        (key.name === "up" || key.name === "down")
      )
        setTimeout(updateFollowFromPosition, 0);
    });

    function selectedSession(): Session | undefined {
      return sessions.find(
        (session) => session.stateDir === view.selectedStateDir,
      );
    }

    function openThemePicker(): void {
      themeBeforePicker = activeThemeName;
      themePickerOpen = true;
      themePanel.visible = true;
      const selectedIndex = themes.findIndex(
        (theme) => theme.name === activeThemeName,
      );
      themePicker.setSelectedIndex(Math.max(0, selectedIndex));
      themePicker.focus();
    }

    function closeThemePicker(): void {
      themePickerOpen = false;
      themePanel.visible = false;
      focusCurrentPanel();
    }

    function cancelThemePicker(): void {
      applyTheme(themeBeforePicker);
      closeThemePicker();
      setStatus(`Kept ${findTheme(activeThemeName)!.label} theme`);
    }

    async function commitThemePicker(): Promise<void> {
      const selected = themePicker.getSelectedOption()?.value as
        | string
        | undefined;
      if (selected) applyTheme(selected);
      closeThemePicker();
      try {
        await persistTheme(activeThemeName);
        setStatus(`Theme saved: ${findTheme(activeThemeName)!.label}`, "success");
      } catch (error) {
        setStatus(`Theme changed, but could not save: ${errorMessage(error)}`, true);
      }
    }

    function applyTheme(name: string): void {
      const theme = findTheme(name);
      if (!theme) return;
      activeThemeName = theme.name;
      setPalette(theme.palette);
      renderer.setBackgroundColor(palette.background);
      root.backgroundColor = palette.background;
      rightColumn.backgroundColor = palette.background;
      artifactPanel.backgroundColor = palette.background;
      sessionList.backgroundColor = palette.panel;
      sessionList.focusedBackgroundColor = palette.panel;
      sessionList.textColor = palette.text;
      sessionList.focusedTextColor = palette.text;
      sessionList.descriptionColor = palette.dim;
      sessionList.selectedDescriptionColor = palette.text;
      sessionList.selectedBackgroundColor = palette.selected;
      sessionList.selectedTextColor = palette.accent;
      diffView.applyTheme();
      workingIndicator.fg = palette.success;
      sessionsPanel.borderColor = palette.border;
      sessionsPanel.focusedBorderColor = palette.accent;
      sessionsPanel.titleColor = palette.accent;
      details.fg = palette.text;
      detailsPanel.backgroundColor = palette.background;
      detailsPanel.borderColor = palette.border;
      detailsPanel.titleColor = palette.accent;
      artifactScroll.verticalScrollbarOptions = {
        trackOptions: {
          foregroundColor: palette.accent,
          backgroundColor: palette.border,
        },
      };
      statusLine.fg = palette.dim;
      footerLeft.fg = palette.dim;
      footerRight.fg = palette.dim;
      promptMetaLeft.fg = palette.dim;
      promptMetaRight.fg = palette.dim;
      followIndicator.fg = palette.warning;
      chatTab.fg = palette.accent;
      for (const [button] of actionButtons) {
        button.fg = palette.accent;
        button.bg = palette.panel;
      }
      sessionsPanel.backgroundColor = palette.background;
      promptPanel.backgroundColor = palette.background;
      promptPanel.focusedBorderColor = palette.accent;
      for (const picker of [modelPicker, dejaPicker]) {
        picker.backgroundColor = palette.panel;
        picker.focusedBackgroundColor = palette.panel;
        picker.textColor = palette.text;
        picker.focusedTextColor = palette.text;
        picker.descriptionColor = palette.dim;
        picker.selectedDescriptionColor = palette.text;
        picker.selectedBackgroundColor = palette.selected;
        picker.selectedTextColor = palette.accent;
      }
      for (const panel of [modelPanel, dejaPanel]) {
        panel.backgroundColor = palette.background;
        panel.borderColor = palette.accent;
        panel.titleColor = palette.accent;
      }
      for (const input of [searchInput, paletteInput]) {
        input.backgroundColor = palette.panel;
        input.focusedBackgroundColor = palette.selected;
        input.textColor = palette.text;
        input.focusedTextColor = palette.text;
        input.placeholderColor = palette.dim;
      }
      promptInput.backgroundColor = palette.panel;
      promptInput.focusedBackgroundColor = palette.selected;
      promptInput.textColor = palette.text;
      promptInput.focusedTextColor = palette.text;
      promptInput.placeholderColor = palette.dim;
      contextMenu.applyTheme();
      palettePanel.backgroundColor = palette.background;
      palettePanel.borderColor = palette.accent;
      palettePanel.titleColor = palette.accent;
      paletteList.backgroundColor = palette.background;
      paletteList.focusedBackgroundColor = palette.background;
      paletteList.textColor = palette.text;
      paletteList.focusedTextColor = palette.text;
      paletteList.descriptionColor = palette.dim;
      paletteList.selectedDescriptionColor = palette.text;
      paletteList.selectedBackgroundColor = palette.selected;
      paletteList.selectedTextColor = palette.accent;
      searchPanel.backgroundColor = palette.background;
      searchPanel.borderColor = palette.accent;
      searchPanel.titleColor = palette.accent;
      promptPanel.borderColor = palette.border;
      promptPanel.titleColor = palette.accent;
      themePanel.backgroundColor = palette.background;
      themePanel.borderColor = palette.accent;
      themePanel.titleColor = palette.accent;
      themePicker.backgroundColor = palette.panel;
      themePicker.focusedBackgroundColor = palette.panel;
      themePicker.textColor = palette.text;
      themePicker.focusedTextColor = palette.text;
      themePicker.descriptionColor = palette.dim;
      themePicker.selectedDescriptionColor = palette.text;
      themePicker.selectedBackgroundColor = palette.selected;
      themePicker.selectedTextColor = palette.accent;
      updateDetails();
      renderArtifactRows(true);
      updateChrome();
    }

    function buildPaletteCommands(): PaletteCommand[] {
      const session = selectedSession();
      const stoppable =
        session?.status === "active" || session?.status === "idle";
      const target = promptTargetForSession(session);
      return [
        { id: "prompt", label: "Send a prompt", key: "s", hint: "steer, prompt, or continue the selected session", disabled: target ? undefined : "no promptable session selected" },
        { id: "new", label: "New session", key: "n", hint: "pick a provider and model, then type the first prompt" },
        { id: "continue", label: "Continue thread in a new run", key: "R", hint: "finished sessions only", disabled: target?.route === "continue" ? undefined : "select a finished session with a thread" },
        { id: "model", label: "Choose model", key: "m" },
        { id: "find", label: "Find a past session", key: "f", hint: "deja search", disabled: dejaAvailable ? undefined : "deja is not on PATH" },
        { id: "stop", label: session?.status === "idle" ? "End idle session" : "Interrupt turn", key: "x x", disabled: stoppable ? undefined : "no active or idle session" },
        { id: "tab-chat", label: "Show chat", key: "o" },
        { id: "tab-trace", label: "Show activity", key: "o" },
        { id: "tab-output", label: "Show output", key: "o" },
        { id: "tab-diff", label: "Show diff", key: "o", hint: "tracked changes against HEAD" },
        { id: "fold", label: "Fold or unfold every diff file", key: "Z", disabled: view.artifact === "diff" ? undefined : "diff tab only" },
        { id: "search", label: "Search this pane", key: "/" },
        { id: "filter", label: "Filter sessions", key: "/", hint: "project, thread, status, or model" },
        { id: "follow", label: "Resume live follow", key: "End", disabled: artifactFollowing ? "already following" : undefined },
        { id: "details", label: "Cycle session details", key: "i" },
        { id: "sessions", label: layout === "beta" ? "Open sessions" : "Focus sessions", key: "Tab" },
        { id: "theme", label: "Change theme", key: "t", hint: "live preview" },
        { id: "refresh", label: "Refresh sessions", key: "r" },
        { id: "copy", label: "Copy selected row", key: "c" },
        { id: "update", label: updateAvailable ? `Update Ruddr to ${updateAvailable}` : "Update Ruddr", hint: "runs ruddr update; restart the TUI afterwards", disabled: updateAvailable ? undefined : "no newer release found on the last daily check" },
        { id: "quit", label: "Quit", key: "q" },
      ];
    }

    function openPalette(): void {
      paletteCommands = buildPaletteCommands();
      paletteOpen = true;
      palettePanel.visible = true;
      paletteInput.value = "";
      fillPalette("");
      paletteInput.focus();
    }

    function fillPalette(query: string): void {
      const matches = filterPaletteCommands(paletteCommands, query);
      paletteList.options = matches.map((command) => ({
        name: `${command.label}${command.key ? `   ${command.key}` : ""}`,
        description: command.disabled
          ? `unavailable · ${command.disabled}`
          : (command.hint ?? ""),
        value: command.id,
      }));
      paletteList.setSelectedIndex(0);
    }

    function closePalette(): void {
      paletteOpen = false;
      palettePanel.visible = false;
      focusCurrentPanel();
    }

    function runPaletteSelection(): void {
      const id = paletteList.getSelectedOption()?.value as string | undefined;
      const command = paletteCommands.find((candidate) => candidate.id === id);
      closePalette();
      if (!command) return;
      if (command.disabled) {
        setStatus(`${command.label}: ${command.disabled}`, "warning");
        return;
      }
      switch (command.id) {
        case "prompt": openPrompt("auto"); break;
        case "new": startNewSessionFlow(); break;
        case "continue": openPrompt("continue"); break;
        case "model": openModelPicker(); break;
        case "find": openDejaSearch(); break;
        case "stop": void requestInterrupt(); break;
        case "tab-chat": setArtifact("chat"); break;
        case "tab-trace": setArtifact("trace"); break;
        case "tab-output": setArtifact("output"); break;
        case "tab-diff": setArtifact("diff"); break;
        case "fold": diffView.toggleAll(); break;
        case "search": focusArtifact(); openSearch(); break;
        case "filter": focusSessions(); openSearch(); break;
        case "follow": resumeFollowing(); break;
        case "details": cycleDetails(); break;
        case "sessions": layout === "beta" ? showSessions() : focusSessions(); break;
        case "theme": openThemePicker(); break;
        case "refresh": void refresh("Refreshing sessions…"); break;
        case "copy": copyCurrentSelection(); break;
        case "update": void runUpdate(); break;
        case "quit": void shutdown(); break;
      }
    }

    function mobileThreshold(): number {
      return persistedConfig.mobileWidthThreshold ?? DEFAULT_MOBILE_WIDTH_THRESHOLD;
    }

    /** Re-evaluates the layout for the current width; no-op when unchanged. */
    function syncLayoutToWidth(): boolean {
      const next = layoutForWidth(renderer.width, mobileThreshold(), launchLayout, args.mobile);
      if (next.layout === layout && next.mobile === mobile) return false;
      applyLayout(next.layout, next.mobile);
      return true;
    }

    // Switches between the docked dashboard and the chat-first overlay layout
    // in place: the sessions pane changes parent and positioning, and the
    // mobile flag swaps the key-hint footer for the tappable action bar.
    function applyLayout(nextLayout: TUILayout, nextMobile: boolean): void {
      layout = nextLayout;
      mobile = nextMobile;
      const overlay = layout === "beta";
      const wantedParent = overlay ? root : body;
      if (sessionsParent !== wantedParent) {
        sessionsParent.remove(sessionsPanel);
        if (overlay) root.add(sessionsPanel);
        else {
          // Docked pane belongs before the right column.
          body.remove(rightColumn);
          body.add(sessionsPanel);
          body.add(rightColumn);
        }
        sessionsParent = wantedParent;
      }
      sessionsPanel.position = overlay ? "absolute" : "relative";
      sessionsPanel.left = overlay ? (mobile ? "2%" : "8%") : "auto";
      sessionsPanel.top = overlay ? (mobile ? "4%" : "8%") : "auto";
      sessionsPanel.width = overlay ? (mobile ? "96%" : "84%") : "34%";
      sessionsPanel.minWidth = overlay ? undefined : 36;
      sessionsPanel.maxWidth = overlay ? undefined : 44;
      sessionsPanel.height = overlay ? (mobile ? "92%" : "84%") : "100%";
      sessionsPanel.zIndex = overlay ? 15 : 0;
      sessionsVisible = false;
      sessionsPanel.visible = !overlay;
      detailsPanel.visible = !overlay;
      detailsExpanded = false;
      rightColumn.width = overlay ? "100%" : "auto";
      rightColumn.minWidth = overlay ? undefined : 50;
      body.flexDirection = overlay ? "column" : "row";
      root.padding = mobile ? 0 : 1;
      artifactPanel.paddingLeft = mobile ? 0 : 1;
      artifactPanel.paddingRight = mobile ? 0 : 1;
      footerBar.visible = !mobile;
      actionBar.visible = mobile;
      // Narrow screens keep only the meter and status under the prompt.
      promptMetaLeft.visible = !mobile;
      if (view.focus === "sessions") {
        view = { ...view, focus: "artifact" };
        artifactScroll.focus();
      }
      updateDetails();
      renderArtifactRows(true);
      updateChrome();
    }

    function focusCurrentPanel(): void {
      if (
        view.focus === "sessions" &&
        (layout === "classic" || sessionsVisible)
      )
        sessionList.focus();
      else {
        view = { ...view, focus: "artifact" };
        artifactScroll.focus();
      }
      updateChrome();
    }

    function focusSessions(): void {
      view = { ...view, focus: "sessions" };
      sessionList.focus();
      updateChrome();
    }

    function focusArtifact(): void {
      view = { ...view, focus: "artifact" };
      artifactScroll.focus();
      updateChrome();
    }

    function setArtifact(artifact: Artifact): void {
      if (view.artifact !== artifact) {
        view = { ...view, artifact };
        resetArtifactPosition();
      }
      focusArtifact();
      updateChrome();
      void updateSelectedSession();
    }

    function resetArtifactPosition(): void {
      artifactKey = "";
      artifactSignature = "";
      selectedRow = -1;
      artifactFollowing = view.artifact !== "diff";
      artifactScroll.stickyScroll = view.artifact !== "diff";
      artifactScroll.stickyStart = view.artifact === "diff" ? "top" : "bottom";
      artifactScroll.scrollTo({ x: 0, y: 0 });
      unseenRows = 0;
      previousRowCount = 0;
      expandedToolIDs = new Set();
    }

    function openSearch(): void {
      searchMode = view.focus === "artifact" ? "artifact" : "sessions";
      searchInput.value =
        searchMode === "sessions"
          ? sessionQuery
          : artifactQueries[view.artifact];
      searchInput.placeholder =
        searchMode === "sessions"
          ? "Filter project, thread, status, or model…"
          : `Search ${
              view.artifact === "trace" ? "activity" : view.artifact
            }…`;
      searchPanel.title =
        searchMode === "sessions"
          ? " Filter sessions · Enter keep · Esc clear "
          : " Search pane · Enter keep · n/N matches · Esc clear ";
      searchPanel.visible = true;
      searchInput.focus();
    }

    function closeSearch(): void {
      searchPanel.visible = false;
      searchMode = undefined;
      focusCurrentPanel();
    }

    function clearAndCloseSearch(): void {
      if (searchMode === "sessions") {
        sessionQuery = "";
        applySessionFilter();
      } else if (searchMode === "artifact") {
        artifactQueries[view.artifact] = "";
        renderArtifactRows(true);
      }
      closeSearch();
    }

    function showSessions(): void {
      sessionsVisible = true;
      sessionsPanel.visible = true;
      view = { ...view, focus: "sessions" };
      sessionList.focus();
      updateChrome();
    }

    function hideSessions(): void {
      sessionsVisible = false;
      sessionsPanel.visible = false;
      view = { ...view, focus: "artifact" };
      artifactScroll.focus();
      updateChrome();
    }

    function nextArtifactView(): Artifact {
      return nextArtifact(view.artifact);
    }

    function cycleDetails(): void {
      if (!detailsPanel.visible) {
        detailsPanel.visible = true;
        detailsExpanded = false;
      } else if (!detailsExpanded) {
        detailsExpanded = true;
      } else {
        detailsPanel.visible = false;
        detailsExpanded = false;
      }
      updateDetails();
    }

    function availableModels(): ModelInfo[] {
      if (promptMode === "new") return models;
      const provider = selectedSession()?.provider;
      return provider
        ? models.filter((model) => model.provider === provider)
        : models;
    }

    function openModelPicker(): void {
      modelPickerOpen = true;
      modelPanel.visible = true;
      const options = modelPickerOptions(availableModels());
      modelPicker.options = options.map((option) => ({
        name: option.name,
        description: option.description,
        value: option.value,
      }));
      const current = pendingModel
        ? options.findIndex((option) => option.model === pendingModel)
        : options.findIndex((option) => option.model.default);
      modelPicker.setSelectedIndex(Math.max(0, current));
      modelPicker.focus();
    }

    function closeModelPicker(): void {
      modelPickerOpen = false;
      modelPanel.visible = false;
      if (promptMode) promptInput.focus();
      else focusCurrentPanel();
      updateChrome();
    }

    function commitModelPicker(): void {
      const selected = modelPicker.getSelectedOption()?.value as
        | string
        | undefined;
      const option = modelPickerOptions(availableModels()).find(
        (candidate) => candidate.value === selected,
      );
      if (!option) return;
      if (option.disabled) {
        setStatus(
          `${option.model.provider} is ${option.model.note || "unavailable"}`,
          true,
        );
        return;
      }
      pendingModel = option.model;
      closeModelPicker();
      setStatus(`Model: ${option.model.label || option.model.id}`);
      updatePromptChrome();
    }

    function startNewSessionFlow(): void {
      pendingResume = undefined;
      pendingModel = undefined;
      promptMode = "new";
      promptTarget = undefined;
      openPromptInput();
      openModelPicker();
    }

    function openDejaSearch(): void {
      if (!dejaAvailable) {
        setStatus("deja is not on PATH; install it to resume past sessions", true);
        return;
      }
      searchMode = "deja";
      searchInput.value = "";
      searchInput.placeholder = "Search past Claude/Codex sessions…";
      searchPanel.title = " deja find · Enter search · Esc cancel ";
      searchPanel.visible = true;
      searchInput.focus();
    }

    async function submitDejaSearch(): Promise<void> {
      const terms = searchInput.value.trim();
      searchPanel.visible = false;
      searchMode = undefined;
      if (!terms) {
        focusCurrentPanel();
        return;
      }
      setStatus(`Searching past sessions for “${terms}”…`);
      try {
        const child = Bun.spawn(
          ["deja", "find", ...terms.split(/\s+/), "--json", "--quiet"],
          { stdout: "pipe", stderr: "pipe" },
        );
        const [stdout, exitCode] = await Promise.all([
          new Response(child.stdout).text(),
          child.exited,
        ]);
        if (exitCode !== 0) throw new Error(`deja find exited ${exitCode}`);
        dejaHits = parseDejaHits(stdout);
        if (dejaHits.length === 0) {
          setStatus(`No resumable sessions matched “${terms}”`, true);
          focusCurrentPanel();
          return;
        }
        openDejaPicker();
      } catch (error) {
        setStatus(errorMessage(error), true);
        focusCurrentPanel();
      }
    }

    function openDejaPicker(): void {
      dejaPickerOpen = true;
      dejaPanel.visible = true;
      dejaPicker.options = dejaHits.map((hit, index) => ({
        name: `${hit.provider}  ${hit.project || "unknown project"}  ${hit.date}`,
        description: hit.openingPrompt.slice(0, 120) || hit.sessionId,
        value: String(index),
      }));
      dejaPicker.setSelectedIndex(0);
      dejaPicker.focus();
      setStatus(`${dejaHits.length} resumable sessions found`, "success");
    }

    function closeDejaPicker(): void {
      dejaPickerOpen = false;
      dejaPanel.visible = false;
      focusCurrentPanel();
    }

    function commitDejaPicker(): void {
      const index = Number(dejaPicker.getSelectedOption()?.value ?? -1);
      const hit = dejaHits[index];
      closeDejaPicker();
      if (!hit) return;
      pendingResume = hit;
      pendingModel = undefined;
      promptMode = "new";
      promptTarget = undefined;
      openPromptInput();
      setStatus(
        `Resuming ${hit.provider} session ${hit.sessionId.slice(0, 12)}… — type the next prompt`,
      );
    }

    function openPrompt(mode: "auto" | "continue"): void {
      const session = selectedSession();
      const target = promptTargetForSession(session);
      if (!target) {
        setStatus(
          mode === "continue"
            ? "Select a finished session with a thread and working directory to continue"
            : session
              ? `Session is ${session.status}; press n for a new session`
              : "No session selected; press n for a new session",
          true,
        );
        return;
      }
      if (
        mode === "continue" &&
        target.route !== "continue"
      ) {
        setStatus(
          "Select a finished session with a thread and working directory to continue",
          true,
        );
        return;
      }
      promptMode = target.route;
      promptTarget = target;
      if (promptMode === "steer" || promptMode === "prompt")
        pendingModel = undefined;
      openPromptInput();
    }

    function openPromptInput(): void {
      view = reduceView(view, { type: "open-steer" });
      promptInput.setText("");
      fitPromptHeight();
      updatePromptChrome();
      promptInput.focus();
      updateChrome();
    }

    function updatePromptChrome(): void {
      const modeTitle =
        promptMode === "steer"
          ? " steer "
          : promptMode === "prompt"
            ? " prompt "
            : promptMode === "continue"
              ? " continue thread "
              : promptMode === "new"
                ? pendingResume
                  ? " resume session "
                  : " new session "
                : "";
      promptPanel.title = modeTitle;
      promptPanel.borderColor = promptMode ? palette.accent : palette.border;
      const promptStateDir = promptTarget?.stateDir;
      const session =
        promptMode && promptMode !== "new" && promptStateDir
          ? discoveredSessions.find(
              (candidate) => candidate.stateDir === promptStateDir,
            )
          : selectedSession();
      const modelLabel =
        pendingModel && (promptMode === "new" || promptMode === "continue")
          ? (pendingModel.label ?? pendingModel.id)
          : (session?.model ?? "");
      const effort = session?.effort ? ` · ${session.effort}` : "";
      promptMetaRight.content = renderUsage(session);
      if (promptMode === "new") {
        const target = pendingResume
          ? `resume ${pendingResume.provider} ${pendingResume.sessionId.slice(0, 12)}…`
          : `new session`;
        promptMetaLeft.content = `${modelLabel || (pendingModel?.provider ?? "codex")} · ${target}`;
        promptInput.placeholder = "First prompt for the session… (Esc cancels)";
        return;
      }
      const route =
        promptMode === "steer" ||
        promptMode === "prompt" ||
        promptMode === "continue"
          ? promptMode
          : promptModeForSession(session);
      if (!session || !route) {
        promptMetaLeft.content = emptyPromptHint(layout);
        promptInput.placeholder = "Ask anything — n starts a new session";
        return;
      }
      const routeLabel =
        route === "steer"
          ? "steering active turn"
          : route === "prompt"
            ? "ready"
            : "continues thread";
      promptMetaLeft.content = [modelLabel, effort ? session.effort : "", routeLabel]
        .filter(Boolean)
        .join(" · ");
      promptInput.placeholder =
        route === "steer"
          ? "Send a new direction to the running turn…"
          : route === "prompt"
            ? "Ask for changes or a follow-up…"
            : "Continue this thread in a new run…";
    }

    function closePrompt(): void {
      pendingModel = undefined;
      pendingResume = undefined;
      promptMode = undefined;
      promptTarget = undefined;
      view = reduceView(view, { type: "close-steer" });
      view = { ...view, focus: "artifact" };
      artifactScroll.focus();
      updatePromptChrome();
      updateChrome();
    }

    async function submitPrompt(): Promise<void> {
      if (actionRunning || !promptMode) return;
      const mode = promptMode;
      const message = promptInput.plainText.trim();
      if (!message) return;
      if (mode === "new") {
        await startNewSession(message);
        return;
      }
      const target = promptTarget;
      if (!target || target.route !== mode) {
        setStatus(
          "The prompt target is no longer available; the prompt was not sent",
          true,
        );
        return;
      }
      const session = resolvePromptTarget(discoveredSessions, target);
      if (!session) {
        const current = discoveredSessions.find(
          (candidate) => candidate.stateDir === target.stateDir,
        );
        setStatus(
          current?.status === "active" && target.route === "steer"
            ? "The active turn changed; the prompt was not sent"
            : current
              ? `Session is now ${current.status}; the prompt was not sent`
            : "The prompt session is no longer available; the prompt was not sent",
          true,
        );
        return;
      }
      if (mode === "steer") await submitSteer(message, session);
      else if (mode === "prompt") await submitIdlePrompt(message, session);
      else await continueThread(message, session);
    }

    async function submitIdlePrompt(
      message: string,
      session: Session,
    ): Promise<void> {
      if (session.status !== "idle") {
        setStatus("Session is no longer idle; the prompt was not sent", true);
        return;
      }
      actionRunning = true;
      closePrompt();
      setStatus("Sending prompt…");
      let scratchDirectory = "";
      try {
        scratchDirectory = await mkdtemp(join(tmpdir(), "ruddr-tui-prompt-"));
        await chmod(scratchDirectory, 0o700);
        const messageFile = join(scratchDirectory, "message.md");
        await writeFile(messageFile, `${message}\n`, { mode: 0o600 });
        const result = await runControl(
          args.ruddr,
          idlePromptControlArguments(session.stateDir, messageFile),
        );
        setStatus(result || "Prompt accepted", "success");
        await refresh();
        setTimeout(() => void refresh(), 300);
      } catch (error) {
        setStatus(errorMessage(error), true);
      } finally {
        actionRunning = false;
        if (scratchDirectory)
          await rm(scratchDirectory, { recursive: true, force: true });
      }
    }

    async function startNewSession(message: string): Promise<void> {
      actionRunning = true;
      const resume = pendingResume;
      const model = pendingModel;
      closePrompt();
      try {
        const provider = resume?.provider ?? model?.provider ?? "codex";
        // TODO(review): Deja hits do not expose their original cwd, so resumes currently use the TUI launch directory.
        const cwd = process.cwd();
        const baseDirectory = join(cwd, ".scratch", "ruddr-tui");
        await mkdir(baseDirectory, { recursive: true, mode: 0o700 });
        await chmod(baseDirectory, 0o700);
        const stamp = new Date().toISOString().replace(/[:.]/g, "-");
        const stateDirectory = join(baseDirectory, `${stamp}.run`);
        await mkdir(stateDirectory, { mode: 0o700 });
        const promptFile = join(stateDirectory, "prompt.md");
        await writeFile(promptFile, `${message}\n`, { mode: 0o600 });
        const runArgs = newSessionRunArguments({
          provider,
          model: model?.id,
          cwd,
          promptFile,
          stateDirectory,
          ...(resume ? { resumeThreadId: resume.sessionId } : {}),
        });
        const child = Bun.spawn([args.ruddr, ...runArgs], {
          cwd,
          stdout: "ignore",
          stderr: "ignore",
          stdin: "ignore",
        });
        child.unref();
        args.stateDirs.push(stateDirectory);
        view = reduceView(view, { type: "select", stateDir: stateDirectory });
        setStatus(
          resume
            ? `Resuming ${resume.provider} session…`
            : `Starting ${provider} session…`,
        );
        setTimeout(() => void refresh(), 250);
        setTimeout(() => void refresh(), 1_000);
      } catch (error) {
        setStatus(errorMessage(error), true);
      } finally {
        actionRunning = false;
      }
    }

    async function submitSteer(
      message: string,
      session: Session,
    ): Promise<void> {
      if (session.status !== "active") {
        setStatus("Turn is no longer active; the steer was not sent", true);
        return;
      }
      actionRunning = true;
      closePrompt();
      setStatus(`Steering ${sessionLabel(session)}…`);
      let scratchDirectory = "";
      try {
        scratchDirectory = await mkdtemp(join(tmpdir(), "ruddr-tui-steer-"));
        await chmod(scratchDirectory, 0o700);
        const messageFile = join(scratchDirectory, "message.md");
        await writeFile(messageFile, `${message}\n`, { mode: 0o600 });
        const result = await runControl(
          args.ruddr,
          steerControlArguments(session.stateDir, session.turnId!, messageFile),
        );
        setStatus(result || "Steer accepted", "success");
        await refresh();
      } catch (error) {
        setStatus(errorMessage(error), true);
      } finally {
        actionRunning = false;
        if (scratchDirectory)
          await rm(scratchDirectory, { recursive: true, force: true });
      }
    }

    async function continueThread(
      message: string,
      session: Session,
    ): Promise<void> {
      if (!session?.threadId || !session.cwd || isLive(session)) return;
      const modelOverride = pendingModel;
      actionRunning = true;
      closePrompt();
      try {
        const baseDirectory = join(session.cwd, ".scratch", "ruddr-tui");
        await mkdir(baseDirectory, { recursive: true, mode: 0o700 });
        await chmod(baseDirectory, 0o700);
        const stamp = new Date().toISOString().replace(/[:.]/g, "-");
        const stateDirectory = join(baseDirectory, `${stamp}.run`);
        await mkdir(stateDirectory, { mode: 0o700 });
        const promptFile = join(stateDirectory, "prompt.md");
        await writeFile(promptFile, `${message}\n`, { mode: 0o600 });
        const overrides =
          modelOverride &&
          modelOverride.provider === (session.provider ?? "codex")
            ? { model: modelOverride.id }
            : {};
        const runArgs = continuationRunArguments(
          session,
          promptFile,
          stateDirectory,
          overrides,
        );
        const child = Bun.spawn([args.ruddr, ...runArgs], {
          cwd: session.cwd,
          stdout: "ignore",
          stderr: "ignore",
          stdin: "ignore",
        });
        child.unref();
        args.stateDirs.push(stateDirectory);
        setStatus(
          `Started a new run for thread ${session.threadId.slice(0, 12)}`,
          "success",
        );
        setTimeout(() => void refresh(), 250);
      } catch (error) {
        setStatus(errorMessage(error), true);
      } finally {
        actionRunning = false;
      }
    }

    async function requestInterrupt(): Promise<void> {
      if (actionRunning) return;
      const session = selectedSession();
      if (
        !session ||
        (session.status !== "active" && session.status !== "idle")
      ) {
        setStatus("Only an active or idle session can be stopped", true);
        return;
      }
      const idle = session.status === "idle";
      const now = Date.now();
      if (view.interruptArmedUntil < now) {
        view = reduceView(view, { type: "arm-interrupt", now });
        renderStatus();
        return;
      }
      view = reduceView(view, { type: "clear-interrupt" });
      actionRunning = true;
      setStatus(idle ? "Ending idle session…" : "Interrupting selected turn…", "warning");
      try {
        const result = await runControl(args.ruddr, [
          idle ? "stop" : "interrupt",
          "--state-dir",
          session.stateDir,
        ]);
        setStatus(result || (idle ? "Shutdown requested" : "Interrupt requested"), "success");
        await refresh();
      } catch (error) {
        setStatus(errorMessage(error), true);
      } finally {
        actionRunning = false;
      }
    }

    async function refresh(reason?: string): Promise<void> {
      return refreshGate.run(async () => {
        if (reason) setStatus(reason);
        try {
          discoveredSessions = await discoverSessions({
            roots: args.roots,
            stateDirs: args.stateDirs,
          });
          listedSessions = visibleSessions(
            discoveredSessions,
            args.includeAll,
            args.stateDirs,
          );
          applySessionFilter();
          await updateSelectedSession();
          lastRefreshAt = Date.now();
          if (reason) showIdleStatus();
        } catch (error) {
          setStatus(errorMessage(error), true);
        }
      });
    }

    function applySessionFilter(): void {
      const selectedStateDir = view.selectedStateDir;
      sessions = filterSessions(listedSessions, sessionQuery);
      view = reduceView(view, { type: "sessions", sessions });
      sessionList.options = sessions.map((session) => ({
        name: sessionCardTitle(session),
        description: sessionDescription(session),
        value: session.stateDir,
      }));
      const selectedIndex = sessions.findIndex(
        (session) => session.stateDir === selectedStateDir,
      );
      if (selectedIndex >= 0) {
        const keep = sessionScrollOffset;
        sessionList.setSelectedIndex(selectedIndex);
        view = reduceView(view, { type: "select", stateDir: selectedStateDir });
        sessionScrollOffset = keep;
      }
      if (sessionScrollOffset !== undefined)
        sessionScrollOffset = scrollListBy(
          sessionList,
          sessionScrollOffset - listScrollOffset(sessionList),
          2,
        );
      const liveCount = sessions.filter(isLive).length;
      const historyCount = sessions.length - liveCount;
      sessionsPanel.title = sessionsPanelTitle({
        layout,
        liveCount,
        recentCount: historyCount,
        query: sessionQuery,
      });
      updateDetails();
      updateChrome();
    }

    // "● project            2m ago": the age hugs the right edge of the card.
    const SESSION_TAG_CELLS = 8;
    function sessionCardInnerWidth(): number {
      return Math.max(24, (sessionList.width || 36) - 4);
    }
    function sessionCardTitle(session: Session): string {
      const label = sessionLabel(session);
      const separator = label.lastIndexOf("  ");
      const head = separator > 0 ? label.slice(0, separator) : label;
      const age = separator > 0 ? label.slice(separator + 2) : "";
      const room = sessionCardInnerWidth() - [...age].length - 1 - SESSION_TAG_CELLS;
      const shownHead =
        [...head].length > room ? `${[...head].slice(0, Math.max(1, room - 1)).join("")}…` : head;
      return `${shownHead.padEnd(room)}${" ".repeat(SESSION_TAG_CELLS)} ${age}`;
    }

    async function updateSelectedSession(): Promise<void> {
      if (destroyed || shutdownPromise) return;
      const isLatestRead = artifactReads.begin();
      const artifact = view.artifact;
      const session = selectedSession();
      const readIsCurrent = () =>
        isLatestRead() && artifact === view.artifact && session?.stateDir === view.selectedStateDir;
      updateDetails();
      if (!session) {
        setArtifactRows([
          {
            id: "empty-action",
            text: "No sessions yet. Press n or click here to start one.",
            copyText: "",
            action: () => startNewSessionFlow(),
          },
        ]);
        return;
      }
      const currentKey = `${session.stateDir}:${artifact}`;
      const forceDiff = artifactKey !== currentKey;
      if (artifactKey !== currentKey) {
        artifactKey = currentKey;
        artifactSignature = "";
        // The diff reads top-down; every other artifact tails its newest rows.
        artifactFollowing = artifact !== "diff";
        artifactScroll.stickyScroll = artifactFollowing;
        if (!artifactFollowing) artifactScroll.scrollTo({ x: 0, y: 0 });
        unseenRows = 0;
        previousRowCount = 0;
        selectedRow = -1;
      }
      const artifactPath =
        artifact === "trace" ? session.tracePath : session.outputPath;
      const diffResult =
        artifact === "diff" && session.cwd
          ? await readWorkspaceDiff(session.cwd, forceDiff)
          : undefined;
      const [content, eventContent] = await Promise.all([
        artifact === "chat" || artifact === "diff"
          ? Promise.resolve("")
          : readTail(artifactPath, ARTIFACT_TAIL_BYTES),
        (artifact === "output" || artifact === "diff") && session.tokenUsage?.contextTokens !== undefined
          ? Promise.resolve("")
          : readTail(session.eventsPath, ARTIFACT_TAIL_BYTES),
      ]);
      if (!readIsCurrent()) return;
      if (session.tokenUsage?.contextTokens === undefined) {
        const context = contextUsageFromEvents(eventContent, session.threadId);
        if (context) {
          session.tokenUsage = { ...session.tokenUsage, ...context };
          updateChrome();
        }
      }
      if (artifact === "chat") {
        const entries = parseChatTranscript(eventContent, session.threadId);
        const rows: ActivityRow[] = [];
        const initialLoad = artifactSignature === "";
        for (const [index, entry] of entries.entries()) {
          const previous = entries[index - 1];
          // Breathing room: a blank line before each speaker change keeps the
          // transcript readable the way opencode's conversation flow is.
          if (previous && entry.kind !== previous.kind)
            rows.push({ id: `chat-gap:${index}`, text: "", copyText: "" });
          rows.push({
            id: `chat:${entry.itemId ?? index}`,
            chat: entry,
            text: entry.text,
            copyText: entry.text,
          });
        }
        // Stream only text that arrived while we were watching; a transcript
        // opened mid-session appears fully written.
        const lastAgent = [...rows].reverse().find((row) => row.chat?.kind === "agent");
        if (lastAgent && sessionIsWorking(session)) {
          const previousLength = seenAgentLength.get(lastAgent.id);
          if (!initialLoad && previousLength !== lastAgent.text.length) {
            const from =
              stream?.id === lastAgent.id ? stream.revealed : (previousLength ?? 0);
            stream = { id: lastAgent.id, revealed: from, target: lastAgent.text.length };
          }
        } else if (stream && lastAgent?.id !== stream.id) stream = undefined;
        for (const row of rows)
          if (row.chat?.kind === "agent") seenAgentLength.set(row.id, row.text.length);
        setArtifactRows(
          rows.length > 0
            ? [...rows, ...liveRow(session)]
            : [
                session.status === "starting"
                  ? { id: "empty", text: "Session is starting…", copyText: "" }
                  : {
                      id: "empty-action",
                      text: "No conversation yet. Press s or click here to send a prompt.",
                      copyText: "",
                      action: () => openPrompt("auto"),
                    },
                ...liveRow(session),
              ],
        );
      } else if (artifact === "trace") {
        let activities = parseTraceActivities(content);
        const fullUpdate = latestAgentUpdate(eventContent);
        if (fullUpdate) {
          activities = activities.filter(
            (activity) => activity.kind !== "message",
          );
          activities.push({ timestamp: "", kind: "message", text: fullUpdate });
        }
        activities = activities.slice(-ACTIVITY_HISTORY_LIMIT);
        const detailsByActivity = attachToolDetails(
          activities,
          parseToolEventDetails(eventContent),
        );
        const rows = activities.map((activity, index) => {
          const detail = detailsByActivity[index];
          const id =
            detail?.id || `${activity.kind}:${activity.timestamp}:${index}`;
          return {
            id,
            activity,
            detail,
            text: activitySearchText(activity, detail),
            copyText: activityCopyText(activity, detail),
          };
        });
        setArtifactRows(
          rows.length > 0
            ? [...rows, ...liveRow(session)]
            : [
                {
                  id: "empty",
                  text: "No activity has been recorded yet.",
                  copyText: "",
                },
                ...liveRow(session),
              ],
        );
      } else if (artifact === "output") {
        const output = visibleArtifactTail(content, OUTPUT_HISTORY_LINES);
        const rows = output
          ? output.split("\n").map((line, index) => ({
              id: `output:${index}`,
              text: line,
              copyText: line,
            }))
          : [
              {
                id: "empty",
                text: "No output has been written yet.",
                copyText: "",
              },
            ];
        setArtifactRows(rows);
      } else {
        const current = await diffView.load(
          session,
          diffResult,
          readIsCurrent,
        );
        if (!current || !readIsCurrent()) return;
        setArtifactRows(diffView.buildRows());
      }
    }


    function liveRow(session: Session | undefined): ActivityRow[] {
      return session?.status === "active" || session?.status === "starting"
        ? [{ id: LIVE_ROW_ID, live: true, text: "", copyText: "" }]
        : [];
    }

    function artifactRowMenu(row: ActivityRow): MenuItem[] {
      const items: MenuItem[] = [];
      if (row.copyText)
        items.push({ label: "Copy row", run: () => copyText(row.copyText) });
      items.push(...diffView.menuItems(row));
      if (row.activity?.kind === "tool")
        items.push({
          label: expandedToolIDs.has(row.id) ? "Collapse tool" : "Expand tool",
          run: () => toggleTool(row.id),
        });
      if (row.action) items.push({ label: "Run", run: row.action });
      if (!artifactFollowing && view.artifact !== "diff")
        items.push({ label: "Resume live follow", run: () => resumeFollowing() });
      return items;
    }

    function activateSelectedRow(): void {
      const row = artifactRows[selectedRow];
      if (!row) return;
      if (row.action) row.action();
      else if (row.diff?.path) diffView.toggleFile(row.diff.path);
      else toggleSelectedTool();
    }

    function setArtifactRows(rows: ActivityRow[]): void {
      const nextSignature = rows
        .map(
          (row) =>
            `${row.id}:${row.text}:${row.detail?.status}:${row.detail?.output?.length ?? 0}`,
        )
        .join("\u0000");
      if (nextSignature === artifactSignature) return;
      if (!artifactFollowing && artifactSignature)
        unseenRows += Math.max(0, rows.length - previousRowCount);
      artifactRows = rows;
      if (view.artifact === "diff") diffView.refreshTree();
      previousRowCount = rows.length;
      artifactSignature = nextSignature;
      if (selectedRow >= rows.length) selectedRow = rows.length - 1;
      renderArtifactRows(true);
      updateChrome();
    }

    function renderArtifactRows(preserveScroll: boolean): void {
      const oldScrollTop = artifactScroll.scrollTop;
      for (const row of rowRenderables) {
        artifactScroll.remove(row);
        row.destroy();
      }
      rowRenderables = artifactRows.map((row, index) => {
        const match = rowMatchesQuery(row);
        // scrollX leaves the content box unbounded, so "100%" would never
        // wrap; pin prose rows to the viewport width instead.
        const renderable = new TextRenderable(renderer, {
          id: `artifact-row-${index}`,
          width: view.artifact === "diff" ? diffView.rowWidth(row) : proseRowWidth(),
          wrapMode: view.artifact === "diff" ? "none" : "word",
          content: renderArtifactRow(row, match, index === selectedRow),
          fg: palette.text,
          // Activity is a structured list: mouse gestures select/expand rows.
          // Letting OpenTUI begin a text selection first also activates its
          // drag auto-scroll, which makes an ordinary click feel erratic.
          selectable: artifactAllowsTextSelection(view.artifact),
        });
        renderable.onMouseDown = (event) => {
          focusArtifact();
          const previousRow = selectedRow;
          selectedRow = index;
          refreshArtifactRow(previousRow);
          if (event.button === 2) {
            refreshArtifactRow(index);
            event.preventDefault();
            contextMenu.show(artifactRowMenu(row), event.x, event.y);
            return;
          }
          if (row.action) row.action();
          else if (row.activity?.kind === "tool") toggleTool(row.id);
          else if (row.diff?.kind === "file" && row.diff.path)
            diffView.toggleFile(row.diff.path);
          else {
            refreshArtifactRow(index);
            if (row.diff) diffView.syncTreeToRow(index);
          }
        };
        artifactScroll.add(renderable);
        return renderable;
      });
      if (artifactFollowing) resumeFollowing(false);
      else if (view.artifact === "diff" && !preserveScroll)
        artifactScroll.scrollTo({ x: 0, y: 0 });
      else if (preserveScroll) artifactScroll.scrollTop = oldScrollTop;
    }

    function renderArtifactRow(
      row: ActivityRow,
      match: boolean,
      selected: boolean,
    ): string | StyledText {
      const selection = selected ? t`${bold(fg(palette.accent)("▎"))}` : t` `;
      const marker = match ? t`${bold(fg(palette.warning)("◆ "))}` : t``;
      if (row.chat)
        return new StyledText([
          ...selection.chunks,
          ...marker.chunks,
          ...renderChatEntry(
            row.chat,
            stream?.id === row.id ? stream.revealed : undefined,
          ).chunks,
        ]);
      if (row.action)
        return new StyledText([
          ...selection.chunks,
          ...marker.chunks,
          ...t`${underline(fg(palette.accent)(row.text.trimStart()))}`.chunks,
        ]);
      if (row.live)
        return t`${fg(palette.success)(spinnerFrame(animationTick))} ${italic(fg(palette.dim)(liveRowText()))}`;
      if (row.diff) return diffView.renderRow(row, match, selected);
      if (view.artifact === "diff" && row.id.startsWith("diff-gap:"))
        return diffView.renderSpacerRow(row, selected);
      if (view.artifact === "output" || !row.activity) {
        // Empty-state rows lead with a status glyph; color it like a toast.
        const glyphColor = row.text.startsWith("✓ ")
          ? palette.success
          : row.text.startsWith("× ")
            ? palette.danger
            : undefined;
        const body =
          glyphColor && row.id.startsWith("empty")
            ? t`${bold(fg(glyphColor)(row.text.slice(0, 1)))}${fg(palette.text)(row.text.slice(1))}`
            : t`${fg(row.id.startsWith("empty") ? palette.dim : palette.text)(row.text)}`;
        return new StyledText([
          ...selection.chunks,
          ...marker.chunks,
          ...body.chunks,
        ]);
      }
      const expanded =
        row.activity.kind === "tool" && expandedToolIDs.has(row.id);
      const base = renderTraceActivity(row.activity, row.detail, expanded);
      const chunks = [...selection.chunks, ...marker.chunks, ...base.chunks];
      if (expanded)
        chunks.push(...renderToolDetail(row.detail, row.activity).chunks);
      return new StyledText(chunks);
    }

    function liveRowText(): string {
      const session = selectedSession();
      if (!session) return "";
      const elapsed = formatElapsed(session.startedAt, undefined);
      const verb = session.status === "starting" ? "starting" : "working";
      return `${verb} · ${elapsed}`;
    }

    function refreshArtifactRow(index: number): void {
      const row = artifactRows[index];
      const renderable = rowRenderables[index];
      if (!row || !renderable) return;
      if (view.artifact === "diff") renderable.width = diffView.rowWidth(row);
      renderable.content = renderArtifactRow(
        row,
        rowMatchesQuery(row),
        index === selectedRow,
      );
    }

    function proseRowWidth(): number {
      return Math.max(20, artifactScroll.width - 1);
    }

    function toggleSelectedTool(): void {
      const row = artifactRows[selectedRow];
      if (row?.activity?.kind === "tool") toggleTool(row.id);
    }

    function moveSelectedRow(direction: number): void {
      if (artifactRows.length === 0) return;
      const previousRow = selectedRow;
      if (selectedRow < 0)
        selectedRow = direction > 0 ? 0 : artifactRows.length - 1;
      else
        selectedRow = Math.max(
          0,
          Math.min(artifactRows.length - 1, selectedRow + direction),
        );
      artifactFollowing = false;
      refreshArtifactRow(previousRow);
      refreshArtifactRow(selectedRow);
      artifactScroll.scrollChildIntoView(`artifact-row-${selectedRow}`);
      updateChrome();
    }

    function toggleTool(id: string): void {
      if (expandedToolIDs.has(id)) expandedToolIDs.delete(id);
      else expandedToolIDs.add(id);
      refreshArtifactRow(selectedRow);
      updateChrome();
    }

    function rowMatchesQuery(row: ActivityRow): boolean {
      const query = artifactQueries[view.artifact].trim().toLocaleLowerCase();
      return Boolean(query && row.text.toLocaleLowerCase().includes(query));
    }

    function moveToSearchMatch(direction: number): void {
      if (view.focus !== "artifact") return;
      const query = artifactQueries[view.artifact].trim();
      if (!query) {
        openSearch();
        return;
      }
      const matches = artifactRows.flatMap((row, index) =>
        rowMatchesQuery(row) ? [index] : [],
      );
      if (matches.length === 0) {
        setStatus(`No matches for “${query}”`, true);
        return;
      }
      const currentMatch = matches.findIndex((index) => index === selectedRow);
      const next =
        currentMatch < 0
          ? direction > 0
            ? 0
            : matches.length - 1
          : (currentMatch + direction + matches.length) % matches.length;
      selectedRow = matches[next];
      artifactFollowing = false;
      renderArtifactRows(true);
      artifactScroll.scrollChildIntoView(`artifact-row-${selectedRow}`);
      updateChrome();
      setStatus(`Match ${next + 1} of ${matches.length}`);
    }

    function copyCurrentSelection(): void {
      let value = "";
      if (view.focus === "artifact")
        value = artifactRows[selectedRow]?.copyText || "";
      else {
        const session = selectedSession();
        value = session?.threadId || session?.stateDir || "";
      }
      if (!value) {
        setStatus("Select a row to copy", true);
        return;
      }
      const copied = renderer.copyToClipboardOSC52(value);
      setStatus(
        copied ? "Copied selection" : "Terminal clipboard copy is unavailable",
        copied ? "success" : "error",
      );
    }

    function updateFollowFromPosition(): void {
      const maxScroll = Math.max(
        0,
        artifactScroll.scrollHeight - artifactScroll.height,
      );
      const atBottom = artifactScroll.scrollTop >= maxScroll - 1;
      artifactFollowing = atBottom;
      if (atBottom) unseenRows = 0;
      updateChrome();
    }

    function resumeFollowing(render = true): void {
      artifactFollowing = true;
      unseenRows = 0;
      artifactScroll.stickyScroll = true;
      artifactScroll.scrollTo({
        x: 0,
        y: Math.max(0, artifactScroll.scrollHeight),
      });
      if (render) updateChrome();
    }

    function updateDetails(): void {
      const session = selectedSession();
      detailsPanel.height = detailsExpanded ? 12 : 8;
      detailsPanel.title = detailsExpanded
        ? " Session · i hide "
        : " Session · i expand ";
      details.content = renderSessionDetails(
        session,
        detailsExpanded
          ? sessionDetails(session)
          : compactSessionDetails(session),
      );
    }

    function updateChrome(): void {
      const tabs: Array<[TextRenderable, Artifact]> = [
        [chatTab, "chat"],
        [activityTab, "trace"],
        [outputTab, "output"],
        [diffTab, "diff"],
      ];
      for (const [tab, artifact] of tabs) {
        const selected = view.artifact === artifact;
        tab.content = renderTabLabel(artifact, selected);
      }
      diffView.updateChrome();
      followIndicator.content = view.artifact === "diff" || artifactFollowing
        ? ""
        : `paused${unseenRows > 0 ? ` · ${unseenRows} new` : ""} · End resumes`;
      const session = selectedSession();
      const query = artifactQueries[view.artifact];
      sessionsPanel.borderColor =
        view.focus === "sessions" ? palette.accent : palette.border;
      footerLeft.content = renderLocation(sessionLocationSummary(session));
      footerRight.content = renderHelp(
        contextualHelp({
          layout,
          focus: view.focus,
          session,
          hasQuery: Boolean(query),
          dejaAvailable,
          compact: renderer.width < 120,
          artifact: view.focus === "artifact" ? view.artifact : undefined,
        }),
      );
      updateWorkingIndicator();
      renderStatus();
      updatePromptChrome();
      updateActionBar();
    }

    function updateActionBar(): void {
      if (!mobile) return;
      const session = selectedSession();
      const stoppable = session?.status === "active" || session?.status === "idle";
      const promptable = Boolean(promptTargetForSession(session));
      actionSessions.content = ` ≡ ${sessions.length} `;
      actionPrompt.content = promptable ? " ✎ prompt " : " ✎ new ";
      actionStop.content = " ■ stop ";
      actionStop.fg = stoppable ? palette.danger : palette.dim;
      actionMore.content = " ⋯ more ";
      for (const [button] of actionButtons) button.bg = palette.panel;
    }

    function renderTabLabel(artifact: Artifact, selected: boolean): StyledText {
      const label =
        artifact === "trace" ? "activity" : artifact;
      const color = selected ? palette.accent : palette.dim;
      const name = selected
        ? underline(bold(fg(color)(label)))
        : fg(color)(label);
      const diffSummary = diffView.summary;
      if (artifact === "diff" && diffSummary && diffSummary.files > 0) {
        const additions = selected ? palette.success : palette.dim;
        const deletions = selected ? palette.danger : palette.dim;
        return t`${name} ${fg(additions)(`+${diffSummary.additions}`)} ${fg(deletions)(`−${diffSummary.deletions}`)}`;
      }
      return t`${name}`;
    }

    function renderHelp(help: string): StyledText {
      const chunks: StyledText["chunks"] = [...t` `.chunks];
      for (const [index, segment] of helpSegments(help).entries()) {
        if (index > 0) chunks.push(...t`${fg(palette.border)("  ")}`.chunks);
        chunks.push(
          ...t`${bold(fg(palette.accent)(segment.key))} ${fg(palette.dim)(segment.label)}`
            .chunks,
        );
      }
      return new StyledText(chunks);
    }

    function renderLocation(location: string): StyledText {
      const separator = location.lastIndexOf(":");
      if (separator <= 0) return t`${fg(palette.dim)(location)}`;
      return t`${fg(palette.dim)(location.slice(0, separator))}${fg(palette.border)(":")}${fg(palette.accent)(location.slice(separator + 1))}`;
    }

    // "~/dev/ruddr:main" for the footer's left corner, opencode style.
    const branchByCwd = new Map<string, string>();
    function sessionLocationSummary(session: Session | undefined): string {
      const cwd = session?.cwd ?? process.cwd();
      const home = process.env.HOME ?? "";
      const shortCwd =
        home && cwd.startsWith(home) ? `~${cwd.slice(home.length)}` : cwd;
      const branch = branchByCwd.get(cwd);
      if (branch === undefined) {
        branchByCwd.set(cwd, "");
        void (async () => {
          try {
            const child = Bun.spawn(
              ["git", "-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"],
              { stdout: "pipe", stderr: "ignore" },
            );
            const [name, exitCode] = await Promise.all([
              new Response(child.stdout).text(),
              child.exited,
            ]);
            if (exitCode === 0 && name.trim())
              branchByCwd.set(cwd, name.trim());
            updateChrome();
          } catch {
            // Not a git checkout; the bare path is enough.
          }
        })();
      }
      return branch ? `${shortCwd}:${branch}` : shortCwd;
    }

    let updateAvailable = args.updateAvailable;
    let updating = false;
    async function runUpdate(): Promise<void> {
      if (updating || !updateAvailable) return;
      updating = true;
      const target = updateAvailable;
      setStatus(`Updating Ruddr to ${target}…`, "info");
      try {
        await runControl(args.ruddr, ["update"]);
        updateAvailable = undefined;
        setStatus(`Ruddr ${target} installed · quit and relaunch ruddr tui`, "success");
      } catch (error) {
        setStatus(`Update failed: ${errorMessage(error)}`, "error");
      } finally {
        updating = false;
      }
    }

    function setStatus(
      message: string,
      danger: boolean | StatusKind = false,
      kind?: StatusKind,
    ): void {
      const resolved: StatusKind =
        typeof danger === "string" ? danger : danger ? "error" : (kind ?? "info");
      statusState = { message, kind: resolved, idle: false };
      if (statusTimer) clearTimeout(statusTimer);
      statusTimer = setTimeout(showIdleStatus, statusTimeoutMs(resolved));
      renderStatus();
    }

    function showIdleStatus(): void {
      if (statusTimer) clearTimeout(statusTimer);
      statusTimer = undefined;
      statusState = { message: "", kind: "info", idle: true };
      renderStatus();
    }

    function renderStatus(): void {
      if (destroyed) return;
      const now = Date.now();
      if (view.interruptArmedUntil > now) {
        const remaining = Math.max(1, Math.ceil((view.interruptArmedUntil - now) / 1000));
        const idle = selectedSession()?.status === "idle";
        statusLine.content = t`${bold(fg(palette.warning)("!"))} ${fg(palette.warning)(`Press x again to ${idle ? "end the idle session" : "interrupt the turn"}`)} ${fg(palette.dim)(`· ${remaining}s`)}`;
        return;
      }
      if (statusState.idle) {
        const liveCount = sessions.filter(isLive).length;
        const summary =
          liveCount > 0
            ? `${liveCount} live ${liveCount === 1 ? "session" : "sessions"}`
            : "watching for sessions";
        const glyph = liveCount > 0 ? spinnerFrame(animationTick) : "·";
        const refreshed =
          lastRefreshAt > 0 ? ` · refreshed ${formatSecondsAgo(lastRefreshAt, now)}` : "";
        const update = updateAvailable
          ? t` ${fg(palette.dim)("·")} ${fg(palette.accent)(`v${updateAvailable} available`)}`.chunks
          : [];
        statusLine.content = new StyledText([
          ...t`${fg(liveCount > 0 ? palette.success : palette.dim)(glyph)} ${fg(palette.dim)(`${summary}${refreshed}`)}`.chunks,
          ...update,
        ]);
        return;
      }
      const busy = actionRunning && statusState.message.endsWith("…");
      const color =
        statusState.kind === "error"
          ? palette.danger
          : statusState.kind === "warning"
            ? palette.warning
            : statusState.kind === "success"
              ? palette.success
              : palette.accent;
      const glyph = busy
        ? spinnerFrame(animationTick)
        : statusGlyphForKind(statusState.kind);
      const textColor =
        statusState.kind === "error" || statusState.kind === "warning"
          ? color
          : palette.text;
      statusLine.content = t`${bold(fg(color)(glyph))} ${fg(textColor)(statusState.message)}`;
    }

    function formatSecondsAgo(timestamp: number, now: number): string {
      const seconds = Math.max(0, Math.floor((now - timestamp) / 1000));
      if (seconds < 1) return "just now";
      if (seconds < 60) return `${seconds}s ago`;
      return `${Math.floor(seconds / 60)}m ago`;
    }

    function sessionIsWorking(session: Session | undefined): boolean {
      return session?.status === "active" || session?.status === "starting";
    }

    function updateWorkingIndicator(): void {
      const session = selectedSession();
      if (!sessionIsWorking(session)) {
        workingIndicator.content = "";
        return;
      }
      const verb = session!.status === "starting" ? "starting" : "working";
      const elapsed = formatElapsed(session!.startedAt, undefined);
      workingIndicator.fg =
        session!.status === "starting" ? palette.warning : palette.success;
      workingIndicator.content = mobile
        ? `${spinnerFrame(animationTick)} ${elapsed}`
        : `${spinnerFrame(animationTick)} ${verb} · ${elapsed}`;
    }

    // One cheap ticker drives every animation; each pass only touches the
    // renderables that are actually animating so idle frames stay idle.
    function animate(): void {
      if (destroyed) return;
      animationTick++;
      const session = selectedSession();
      const anyLive = sessions.some(isLive);
      const working = sessionIsWorking(session);
      const armed = view.interruptArmedUntil > Date.now();
      if (anyLive) sessionList.requestRender();
      if (working) {
        updateWorkingIndicator();
        const liveIndex = artifactRows.findIndex((row) => row.live);
        if (liveIndex >= 0) refreshArtifactRow(liveIndex);
        if (stream && stream.revealed < stream.target) {
          stream.revealed = typewriterReveal(stream.revealed, stream.target);
          const streamIndex = artifactRows.findIndex((row) => row.id === stream!.id);
          if (streamIndex >= 0) refreshArtifactRow(streamIndex);
          if (artifactFollowing) resumeFollowing(false);
        }
      }
      if (
        statusState.idle ||
        armed ||
        (actionRunning && statusState.message.endsWith("…"))
      )
        renderStatus();
      if (animationTick % 10 === 0 && working) updateDetails();
    }

    let resolveDone: (() => void) | undefined;
    const done = new Promise<void>((resolve) => {
      resolveDone = resolve;
    });
    let shutdownPromise: Promise<void> | undefined;
    const refreshTimer = setInterval(() => void refresh(), args.interval);
    const animationTimer = setInterval(animate, ANIMATION_INTERVAL_MS);
    const signalNames: NodeJS.Signals[] = [
      "SIGINT",
      "SIGTERM",
      "SIGQUIT",
      "SIGHUP",
    ];
    for (const signal of signalNames) process.once(signal, shutdown);

    function shutdown(): Promise<void> {
      if (shutdownPromise) return shutdownPromise;
      shutdownPromise = (async () => {
        artifactReads.stop();
        clearInterval(refreshTimer);
        clearInterval(animationTimer);
        if (statusTimer) clearTimeout(statusTimer);
        diffView.dispose();
        for (const signal of signalNames) process.off(signal, shutdown);
        await refreshGate.stop();
        if (!destroyed) {
          renderer.destroy();
          destroyed = true;
        }
        resolveDone?.();
      })();
      return shutdownPromise;
    }

    applyTheme(activeThemeName);
    applyLayout(
      layoutForWidth(renderer.width, mobileThreshold(), launchLayout, args.mobile).layout,
      layoutForWidth(renderer.width, mobileThreshold(), launchLayout, args.mobile).mobile,
    );
    void (async () => {
      try {
        models = parseModelCatalog(
          await runControl(args.ruddr, ["models", "--json"]),
        );
      } catch {
        models = FALLBACK_MODELS;
      }
    })();
    await refresh();
    if (updateAvailable)
      setStatus(
        `Ruddr ${updateAvailable} is available · run ruddr update or pick Update Ruddr in the palette`,
        "info",
      );
    await done;
  } finally {
    if (!destroyed) renderer.destroy();
  }
}



await main().catch((error) => {
  process.stderr.write(`ruddr tui: ${errorMessage(error)}\n`);
  process.exitCode = 1;
});
