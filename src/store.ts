// Global UI state. Server data (feeds, articles…) lives in React Query;
// this store holds view selection plus the appearance preferences the
// design's settings / tweaks controls drive.

import { create } from "zustand";
import i18n from "./i18n";
import * as api from "./api";
import type { AiBriefing, AiContextRef, ArticleQuery } from "./types";

/** Appearance is two independent axes: a colour `Palette` (the family — warm
 *  Paper, cool Frost, high-contrast) and a light/dark `Mode`. Their product is
 *  the 6 themes; the CSS keys off `data-palette` + `data-mode` on the root.
 *
 *  `Mode` also carries `"system"`, which isn't a theme of its own — it follows
 *  the OS light/dark preference and resolves to one of the two concrete
 *  `ResolvedMode`s (see `resolveMode`) that the CSS + native backing key off. */
export type Palette = "paper" | "frost" | "contrast";
export type Mode = "light" | "dark" | "system";
export type ResolvedMode = "light" | "dark";
export const PALETTES: Palette[] = ["paper", "frost", "contrast"];
export const MODES: Mode[] = ["light", "dark", "system"];

/** Whether the OS currently prefers a dark colour scheme. Drives `"system"`
 *  mode. `matchMedia` always exists in the Tauri webview; the guard is just
 *  defensive so a non-DOM import site can't throw. */
export function systemPrefersDark(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: dark)").matches
  );
}

/** Resolve the stored appearance preference to the concrete light/dark that the
 *  CSS `data-mode` and native window backing use. `"system"` follows the OS. */
export function resolveMode(mode: Mode): ResolvedMode {
  if (mode === "system") return systemPrefersDark() ? "dark" : "light";
  return mode;
}
export type Density = "compact" | "cozy" | "spacious";
export type ViewMode = "list" | "card";
export type StartupView = "all" | "unread" | "starred" | "last";
export type ReaderFont = "serif" | "sans" | "hyperlegible";

/** Reader title/body typeface options. `stack` feeds the `--reader-font` CSS
 *  variable; `adjust` nudges the body font-size — sans and Hyperlegible read
 *  visually larger than the serif at the same pixel size, so they shrink a
 *  touch to keep the optical size even across choices. */
export const READER_FONTS: Record<ReaderFont, { stack: string; adjust: string }> = {
  serif: { stack: "var(--serif)", adjust: "0px" },
  sans: { stack: "var(--ui)", adjust: "-1.5px" },
  hyperlegible: { stack: "'Atkinson Hyperlegible', var(--ui)", adjust: "-1.5px" },
};

/** Valid ranges for the reader appearance sliders — the single source of
 *  truth shared by the Settings sliders, persistence validation, and the
 *  `setReader` write guard, so all three stay in lockstep. */
export const READER_BOUNDS = {
  size: { min: 14, max: 22 },
  leading: { min: 130, max: 200 },
  width: { min: 520, max: 840 },
} as const;

/** Valid ranges for the draggable pane widths — shared by the resize handles,
 *  persistence validation, and the `setPanel` write guard so all stay in
 *  lockstep (mirrors `READER_BOUNDS`). The article-list column can grow wide,
 *  but never so far it crowds out the reader; the sidebar stays a navigation
 *  rail. */
export const PANEL_BOUNDS = {
  sidebar: { min: 200, max: 420 },
  list: { min: 300, max: 560 },
  /** The reader's AI-chat sidebar — a conversation pane, not a reading
   *  surface: it may go narrow, but never so narrow the bubbles wrap badly. */
  aiChat: { min: 280, max: 560 },
} as const;

/** Clamp `n` into `[min, max]`. */
function clamp(n: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, n));
}

/** How an article opens by default: reader view, auto-extracted full text,
 *  or the embedded web view. Individual feeds can override it (`Feed.openMode`). */
export type OpenMode = "reader" | "extracted" | "web";
export const OPEN_MODES: readonly OpenMode[] = ["reader", "extracted", "web"];

/** Behavioural preferences driven by the Settings panel. */
export interface Prefs {
  showSidebarCounts: boolean;
  showCardThumbs: boolean;
  reduceMotion: boolean;
  showReadingTime: boolean;
  markReadOnOpen: boolean;
  markReadOnScroll: boolean;
  defaultOpenMode: OpenMode;
  startupView: StartupView;
  hideReadOnStartup: boolean;
  /** Sidebar "unread only" mode — hide feeds with no unread articles. */
  sidebarUnreadOnly: boolean;
  /** Load the AI summary automatically when an article opens — a cached one
   *  shows instantly, a missing one generates in the background. */
  autoSummarize: boolean;
}

const ls = {
  get: (k: string, fallback: string) => localStorage.getItem(k) ?? fallback,
  /** A persisted enum value, validated against `allowed` — a corrupt or
   *  stale value falls back instead of flowing through an unchecked cast. */
  oneOf: <T extends string>(k: string, allowed: readonly T[], fallback: T): T => {
    const v = localStorage.getItem(k);
    return v != null && (allowed as readonly string[]).includes(v)
      ? (v as T)
      : fallback;
  },
  /** A persisted number, clamped to `[min, max]`. localStorage is
   *  webview-writable and may hold a corrupt non-numeric value (NaN would
   *  reach a CSS variable like `--reader-size: NaNpx` and break the layout)
   *  or a stale out-of-range value from an older build with different
   *  slider limits — both would distort the reader. NaN falls back; an
   *  in-band-but-out-of-range value is clamped into range. */
  num: (k: string, fallback: number, min: number, max: number) => {
    const v = localStorage.getItem(k);
    if (v == null) return fallback;
    const n = Number(v);
    if (!Number.isFinite(n)) return fallback;
    return clamp(n, min, max);
  },
  bool: (k: string, fallback: boolean) => {
    const v = localStorage.getItem(k);
    return v == null ? fallback : v === "1";
  },
  set: (k: string, v: string | number | boolean) =>
    localStorage.setItem(k, typeof v === "boolean" ? (v ? "1" : "0") : String(v)),
};

/** A prompt queued by another surface (the reader's empty-state "summarize
 *  this timeline" action) for the AI composer to send once it is open and
 *  idle. Carries the AI contexts captured when it was queued: the queue holds
 *  while a reply is still streaming, and the user may switch feed / folder /
 *  tag in the meantime — the prompt must describe the timeline it was written
 *  for, not whichever one is active when the send finally fires. */
export interface AiPendingPrompt {
  text: string;
  contexts: AiContextRef[];
}

/** The AI auto context for the active timeline (no article open) — the same
 *  mapping the composer derives for its own auto-context, kept in one place
 *  so a queued prompt's snapshot and a live send label a timeline identically. */
export function timelineContextRef(
  query: ArticleQuery,
  label: string,
): AiContextRef {
  if (query.kind === "feed") {
    return { kind: "feed", feedId: query.value, title: label };
  }
  if (query.kind === "folder") {
    return { kind: "folder", folderId: query.value, name: label };
  }
  if (query.kind === "tag") {
    return { kind: "tag", tagId: query.value, name: label };
  }
  return { kind: "view", query, label };
}

interface UiState {
  /** The active sidebar selection driving the article list. */
  query: ArticleQuery;
  /** Human-readable label of the current selection (list header). */
  queryLabel: string;
  /** Currently open article, or null. */
  selectedArticleId: number | null;
  /** Hide already-read articles in the list. */
  unreadOnly: boolean;
  /** Sort the list oldest-first instead of newest-first. */
  sortOldest: boolean;
  /** Offset of the first page the middle-pane list loads — its paging anchor.
   *  Normally 0 (newest first). Opening an article from search that lives far
   *  down the list jumps the anchor to that article's page so the list loads
   *  *only* that page and pages outward from there, instead of every page above
   *  it. Part of the list's React Query key, so `currentList` reads the same
   *  window the user sees. Reset to 0 on any list-context change (feed / filter
   *  / sort). */
  listAnchor: number;

  // appearance preferences
  palette: Palette;
  mode: Mode;
  density: Density;
  viewMode: ViewMode;
  readerFont: ReaderFont;
  readerSize: number;
  readerLeading: number;
  readerWidth: number;

  // draggable pane widths (px)
  sidebarWidth: number;
  listWidth: number;
  /** The reader's AI-chat sidebar width. */
  aiChatWidth: number;

  // behavioural preferences
  prefs: Prefs;

  // transient view modes
  focusMode: boolean;
  /** The inline AI summary card above the article is visible. Reset per
   *  article — the reader closes it on article switch (the auto-summarize
   *  preference re-opens it for the new article when on). */
  aiOpen: boolean;
  /** The global AI-chat sidebar (docked on the right) is open. */
  aiChatOpen: boolean;
  /** The AI Briefing view is active in the main area instead of list+reader. */
  briefingOpen: boolean;
  /** Active persisted AI chat session ID (null = fresh unsaved session). */
  aiActiveSessionId: string | null;
  /** Manually pinned / attached context chips in the global AI composer. */
  aiPinnedContexts: AiContextRef[];
  /** Optional draft prompt handed to the AI composer (e.g. from the reader's
   *  empty-state quick actions), with the AI contexts captured at queue time. */
  aiPendingPrompt: AiPendingPrompt | null;
  /** Client-side importance ordering for the article list — the reader's
   *  empty-state "sort by importance" quick action. `key` is the
   *  currentListKey() signature of the list the scores were computed for, so
   *  the ordering applies only while that exact view/filter combination is
   *  shown; switching feed or filters drops back to the default order. The
   *  scored ids drive `applyIdOrder` in lib/currentList.ts; scores are
   *  session-only and never persisted. */
  importanceOrder: { key: string; ids: number[] } | null;
  /** Whether the reader shows the reading view or the article's original
   *  page (native child webview). Lives here, not in the reader component,
   *  so the keyboard / command-palette AI-summary toggle can return to the
   *  reading view the same way the reader toolbar button does — the summary
   *  card only renders in the reading view. */
  readerView: "reader" | "web";
  /** A covering modal (subscribe / settings / explore …) is open. The reader's
   *  original-page view is a native child webview that floats above the whole
   *  DOM — including modals — so it must be torn down while one is up, or it
   *  occludes the dialog (issue #54). The reader effect watches this flag. */
  modalOpen: boolean;
  /** A floating context menu is open. Same hazard as `modalOpen`: a context
   *  menu raised over the reading area would be occluded by the native
   *  original-page webview that floats above the DOM (issue #74), so the
   *  reader suspends that view while a menu is up. */
  menuOpen: boolean;
  /** A background refresh (Rust scheduler / mobile sync) landed while the user
   *  was parked on the current article list. The visible list is deliberately
   *  NOT refetched then — a mid-read refetch would drop rows out from under
   *  them (a just-opened article vanishing from an "unread only" view) — so
   *  App.tsx queues the event here instead. ArticleList drains the backlog the
   *  moment the list context actually changes (feed / filter / sort / anchor),
   *  i.e. once the user has left the list that was being protected. A plain
   *  boolean: repeat events collapse into one backlog, nothing accumulates. */
  pendingListRefresh: boolean;

  select: (query: ArticleQuery, label: string) => void;
  openArticle: (id: number | null) => void;
  toggleUnreadOnly: () => void;
  toggleSort: () => void;
  setListAnchor: (offset: number) => void;

  setPalette: (p: Palette) => void;
  setMode: (m: Mode) => void;
  setDensity: (d: Density) => void;
  setViewMode: (v: ViewMode) => void;
  setReaderFont: (v: ReaderFont) => void;
  setReader: (p: Partial<Pick<UiState, "readerSize" | "readerLeading" | "readerWidth">>) => void;
  setPanel: (p: Partial<Pick<UiState, "sidebarWidth" | "listWidth" | "aiChatWidth">>) => void;

  setPref: (patch: Partial<Prefs>) => void;

  setFocusMode: (v: boolean) => void;
  setAiOpen: (v: boolean) => void;
  setAiChatOpen: (v: boolean) => void;
  setBriefingOpen: (v: boolean) => void;
  setAiActiveSessionId: (id: string | null) => void;
  addAiContext: (ctx: AiContextRef) => void;
  removeAiContext: (index: number) => void;
  clearAiContexts: () => void;
  consumeAiPendingPrompt: () => AiPendingPrompt | null;
  askAiAboutSelection: (articleId: number, title: string, quote: string) => void;
  askAiAboutBriefing: (briefing: AiBriefing) => void;
  /** Open the AI assistant sidebar and auto-send a "summarize this timeline"
   *  prompt (the reader's empty-state quick action). The prompt rides the
   *  one-shot `aiPendingPrompt` queue with the timeline context snapshotted
   *  at click time; the composer sends it once it is open and idle, using
   *  that snapshot — even if the user has switched feeds while a previous
   *  reply was still streaming. */
  askAiSummarizeTimeline: () => void;
  setImportanceOrder: (order: { key: string; ids: number[] } | null) => void;
  setReaderView: (v: "reader" | "web") => void;
  setModalOpen: (v: boolean) => void;
  setMenuOpen: (v: boolean) => void;
  markListRefreshPending: () => void;
  consumePendingListRefresh: () => boolean;
}

const PREF_KEYS: (keyof Prefs)[] = [
  "showSidebarCounts",
  "showCardThumbs",
  "reduceMotion",
  "showReadingTime",
  "markReadOnOpen",
  "markReadOnScroll",
  "defaultOpenMode",
  "startupView",
  "hideReadOnStartup",
  "sidebarUnreadOnly",
  "autoSummarize",
];

/** Mirror the active palette + mode into the backend settings table so the Rust
 *  side can paint the native window in the matching colour *before* the webview
 *  loads on the next launch — without this, a dark user sees a brief light flash
 *  at window-create time (the `tauri.conf.json` background is a fixed light
 *  colour the backend has no other way to override). Mirrors the way `i18n.ts`
 *  persists the language for backend-localised text. */
function mirrorAppearance(palette: Palette, mode: Mode): void {
  api.setSetting("palette", palette).catch(() => {});
  // The backend paints in concrete light/dark, so mirror the *resolved* mode —
  // it has no notion of `"system"`. (The frontend re-asserts this whenever the
  // OS scheme changes while running, so the next launch's pre-paint matches.)
  api.setSetting("mode", resolveMode(mode)).catch(() => {});
}

/** Pin the backend's `dark_shade` to the single shipped shade so the native
 *  window paints the matching colour before the first webview frame (lib.rs),
 *  and any value persisted by an older build that exposed a shade picker is
 *  normalised back to "default". */
function mirrorDarkShade(): void {
  api.setSetting("dark_shade", "default").catch(() => {});
}

/** Default `mode` for an install with no persisted `mode` yet: honour an
 *  explicit legacy `theme` choice, else follow the OS (`"system"`). */
function legacyModeDefault(): Mode {
  const legacy = localStorage.getItem("theme");
  if (legacy === "dark") return "dark";
  if (legacy === "light") return "light";
  return "system";
}

/** Resolve the persisted reader font, migrating the pre-0.2 boolean
 *  `useSerif` toggle (serif on/off) to the named-typeface preference. */
function loadReaderFont(): ReaderFont {
  const v = localStorage.getItem("readerFont");
  if (v === "serif" || v === "sans" || v === "hyperlegible") return v;
  return localStorage.getItem("useSerif") === "0" ? "sans" : "serif";
}

function loadPrefs(): Prefs {
  return {
    showSidebarCounts: ls.bool("pref.showSidebarCounts", true),
    showCardThumbs: ls.bool("pref.showCardThumbs", true),
    reduceMotion: ls.bool("pref.reduceMotion", false),
    showReadingTime: ls.bool("pref.showReadingTime", true),
    markReadOnOpen: ls.bool("pref.markReadOnOpen", true),
    markReadOnScroll: ls.bool("pref.markReadOnScroll", false),
    // Migrates the pre-0.15 boolean "auto-extract full text" toggle: a user
    // who had it on keeps auto-extraction as their default open mode.
    defaultOpenMode: ls.oneOf<OpenMode>(
      "pref.defaultOpenMode",
      OPEN_MODES,
      ls.bool("pref.autoExtract", false) ? "extracted" : "reader",
    ),
    startupView: ls.oneOf<StartupView>(
      "pref.startupView",
      ["all", "unread", "starred", "last"],
      "unread",
    ),
    hideReadOnStartup: ls.bool("pref.hideReadOnStartup", false),
    sidebarUnreadOnly: ls.bool("pref.sidebarUnreadOnly", false),
    autoSummarize: ls.bool("pref.autoSummarize", false),
  };
}

export const useUi = create<UiState>((set, get) => ({
  query: { kind: "all" },
  queryLabel: i18n.t("smart.all"),
  selectedArticleId: null,
  unreadOnly: false,
  sortOldest: false,
  listAnchor: 0,

  palette: ls.oneOf<Palette>("palette", PALETTES, "paper"),
  // Migrate the pre-6-theme `theme` key: a user who had explicitly chosen
  // `theme: "light"|"dark"` keeps that exact mode. A fresh install (no legacy
  // `theme` and no `mode`) defaults to `"system"`, following the OS.
  mode: ls.oneOf<Mode>("mode", MODES, legacyModeDefault()),
  density: ls.oneOf<Density>(
    "density",
    ["compact", "cozy", "spacious"],
    "cozy",
  ),
  viewMode: ls.oneOf<ViewMode>("viewMode", ["list", "card"], "list"),
  readerFont: loadReaderFont(),
  readerSize: ls.num("readerSize", 17, READER_BOUNDS.size.min, READER_BOUNDS.size.max),
  readerLeading: ls.num(
    "readerLeading",
    165,
    READER_BOUNDS.leading.min,
    READER_BOUNDS.leading.max,
  ),
  readerWidth: ls.num("readerWidth", 680, READER_BOUNDS.width.min, READER_BOUNDS.width.max),

  sidebarWidth: ls.num("sidebarWidth", 248, PANEL_BOUNDS.sidebar.min, PANEL_BOUNDS.sidebar.max),
  listWidth: ls.num("listWidth", 388, PANEL_BOUNDS.list.min, PANEL_BOUNDS.list.max),
  aiChatWidth: ls.num("aiChatWidth", 360, PANEL_BOUNDS.aiChat.min, PANEL_BOUNDS.aiChat.max),

  prefs: loadPrefs(),

  focusMode: false,
  aiOpen: false,
  aiChatOpen: false,
  briefingOpen: false,
  aiActiveSessionId: null,
  aiPinnedContexts: [],
  aiPendingPrompt: null,
  importanceOrder: null,
  readerView: "reader",
  modalOpen: false,
  menuOpen: false,
  pendingListRefresh: false,

  select: (query, label) => {
    // Remember the selection so the "open on startup: last view" preference
    // can restore it next launch.
    ls.set("lastView", JSON.stringify({ query, label }));
    // Reset the paging anchor: a new selection always opens at the newest page.
    set({
      query,
      queryLabel: label,
      selectedArticleId: null,
      listAnchor: 0,
      briefingOpen: false,
    });
  },
  openArticle: (id) =>
    set((s) => ({
      selectedArticleId: id,
      briefingOpen: id != null ? false : s.briefingOpen,
    })),
  // Toggling a filter/sort rebuilds the list, so re-anchor to the newest page.
  toggleUnreadOnly: () => set((s) => ({ unreadOnly: !s.unreadOnly, listAnchor: 0 })),
  toggleSort: () => set((s) => ({ sortOldest: !s.sortOldest, listAnchor: 0 })),
  setListAnchor: (listAnchor) => set({ listAnchor }),

  setPalette: (palette) => { ls.set("palette", palette); mirrorAppearance(palette, get().mode); set({ palette }); },
  setMode: (mode) => { ls.set("mode", mode); mirrorAppearance(get().palette, mode); set({ mode }); },
  setDensity: (density) => { ls.set("density", density); set({ density }); },
  setViewMode: (viewMode) => { ls.set("viewMode", viewMode); set({ viewMode }); },
  setReaderFont: (readerFont) => { ls.set("readerFont", readerFont); set({ readerFont }); },
  setReader: (p) => {
    // Clamp on write too: any caller (or a stale slider range) is kept from
    // pushing an out-of-range value into the persisted store or a CSS var.
    const next: Partial<Pick<UiState, "readerSize" | "readerLeading" | "readerWidth">> = {};
    if (p.readerSize != null) {
      next.readerSize = clamp(p.readerSize, READER_BOUNDS.size.min, READER_BOUNDS.size.max);
      ls.set("readerSize", next.readerSize);
    }
    if (p.readerLeading != null) {
      next.readerLeading = clamp(
        p.readerLeading,
        READER_BOUNDS.leading.min,
        READER_BOUNDS.leading.max,
      );
      ls.set("readerLeading", next.readerLeading);
    }
    if (p.readerWidth != null) {
      next.readerWidth = clamp(p.readerWidth, READER_BOUNDS.width.min, READER_BOUNDS.width.max);
      ls.set("readerWidth", next.readerWidth);
    }
    set(next);
  },
  setPanel: (p) => {
    // Clamp on write so neither a drag past the handle's guard nor a stale
    // persisted value (from an older build with different limits) can push an
    // out-of-range width into the store or its `--col-*` CSS variable.
    const next: Partial<Pick<UiState, "sidebarWidth" | "listWidth" | "aiChatWidth">> = {};
    if (p.sidebarWidth != null) {
      next.sidebarWidth = clamp(p.sidebarWidth, PANEL_BOUNDS.sidebar.min, PANEL_BOUNDS.sidebar.max);
      ls.set("sidebarWidth", next.sidebarWidth);
    }
    if (p.listWidth != null) {
      next.listWidth = clamp(p.listWidth, PANEL_BOUNDS.list.min, PANEL_BOUNDS.list.max);
      ls.set("listWidth", next.listWidth);
    }
    if (p.aiChatWidth != null) {
      next.aiChatWidth = clamp(p.aiChatWidth, PANEL_BOUNDS.aiChat.min, PANEL_BOUNDS.aiChat.max);
      ls.set("aiChatWidth", next.aiChatWidth);
    }
    set(next);
  },

  setPref: (patch) => {
    for (const k of PREF_KEYS) {
      if (patch[k] !== undefined) ls.set(`pref.${k}`, patch[k] as string | boolean);
    }
    set((s) => ({ prefs: { ...s.prefs, ...patch } }));
  },

  setFocusMode: (focusMode) => set({ focusMode }),
  setAiOpen: (aiOpen) => set({ aiOpen }),
  setAiChatOpen: (aiChatOpen) => set({ aiChatOpen }),
  setBriefingOpen: (briefingOpen) =>
    set((s) => ({
      briefingOpen,
      focusMode: briefingOpen ? false : s.focusMode,
    })),
  setAiActiveSessionId: (aiActiveSessionId) => set({ aiActiveSessionId }),
  addAiContext: (ctx) =>
    set((s) => {
      const key = JSON.stringify(ctx);
      if (s.aiPinnedContexts.some((c) => JSON.stringify(c) === key)) return s;
      return { aiPinnedContexts: [...s.aiPinnedContexts, ctx] };
    }),
  removeAiContext: (index) =>
    set((s) => ({
      aiPinnedContexts: s.aiPinnedContexts.filter((_, i) => i !== index),
    })),
  clearAiContexts: () => set({ aiPinnedContexts: [] }),
  consumeAiPendingPrompt: () => {
    const p = get().aiPendingPrompt;
    if (p != null) set({ aiPendingPrompt: null });
    return p;
  },
  askAiAboutSelection: (articleId, title, quote) =>
    set((s) => {
      const nextCtx: AiContextRef = { kind: "selection", articleId, title, quote };
      const filtered = s.aiPinnedContexts.filter((c) => c.kind !== "selection");
      return {
        aiChatOpen: true,
        aiPinnedContexts: [...filtered, nextCtx],
      };
    }),
  askAiAboutBriefing: (briefing) =>
    set((s) => {
      const nextCtx: AiContextRef = {
        kind: "briefing",
        briefingId: briefing.id,
        title: briefing.title,
        content: briefing.content,
      };
      const filtered = s.aiPinnedContexts.filter((c) => c.kind !== "briefing");
      return {
        aiChatOpen: true,
        aiPinnedContexts: [...filtered, nextCtx],
      };
    }),
  askAiSummarizeTimeline: () =>
    set((s) => {
      // Snapshot the timeline context NOW — the click time. The prompt queues
      // while a reply may still be streaming, and the user can switch feed /
      // folder / tag before the composer goes idle; sending would otherwise
      // attach the then-current timeline and mislabel the summary. Pinned
      // chips ride along as they were at click time; a timeline that is
      // already pinned is not prepended twice.
      const timeline = timelineContextRef(s.query, s.queryLabel);
      const key = JSON.stringify(timeline);
      const contexts = s.aiPinnedContexts.some((c) => JSON.stringify(c) === key)
        ? [...s.aiPinnedContexts]
        : [timeline, ...s.aiPinnedContexts];
      return {
        aiChatOpen: true,
        aiPendingPrompt: {
          text: i18n.t("ai.summarizeTimelinePrompt"),
          contexts,
        },
      };
    }),
  setImportanceOrder: (importanceOrder) => set({ importanceOrder }),
  setReaderView: (readerView) => set({ readerView }),
  setModalOpen: (modalOpen) => set({ modalOpen }),
  setMenuOpen: (menuOpen) => set({ menuOpen }),
  markListRefreshPending: () => set({ pendingListRefresh: true }),
  consumePendingListRefresh: () => {
    if (!get().pendingListRefresh) return false;
    set({ pendingListRefresh: false });
    return true;
  },
}));

// Seed the backend's appearance copy on startup so an existing install — whose
// theme has lived only in localStorage until now — still gets the native
// launch background themed correctly from the next launch onward.
mirrorAppearance(useUi.getState().palette, useUi.getState().mode);
mirrorDarkShade();
