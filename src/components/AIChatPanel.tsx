import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import * as api from "../api";
import type { ChatTurn } from "../api";
import { PANEL_BOUNDS, useUi } from "../store";
import { renderMarkdown } from "../lib/markdown";
import { reportError, toast } from "../toast";
import type { AiContextRef, AiToolTrace } from "../types";
import Icon from "./Icon";
import ResizeHandle from "./ResizeHandle";

/** One exchange in the sidebar conversation. The assistant's `content`
 *  accumulates stream deltas live and tracks any read-only tool calls. */
interface ChatMessage {
  role: "user" | "assistant";
  content: string;
  contexts?: AiContextRef[];
  tools?: AiToolTrace[];
}

/** Click handler for links inside an assistant reply:
 *  - `papr://article/<id>` navigates directly to that article in the Reader.
 *  - External URLs open in the OS browser. */
function chatLinkClick(e: React.MouseEvent) {
  const link = (e.target as HTMLElement).closest("a");
  if (!link) return;
  const href = link.getAttribute("href");
  if (!href) return;
  e.preventDefault();
  const m = /^papr:\/\/article\/(\d+)$/.exec(href);
  if (m) {
    useUi.getState().openArticle(Number(m[1]));
    return;
  }
  if (/^(https?:|mailto:)/i.test(link.href)) {
    openUrl(link.href).catch(() => {});
  }
}

function safeParseJson<T>(raw: string | null): T | undefined {
  if (!raw) return undefined;
  try {
    return JSON.parse(raw) as T;
  } catch {
    return undefined;
  }
}

function contextLabel(ctx: AiContextRef, t: (k: string) => string): string {
  switch (ctx.kind) {
    case "article":
      return ctx.title;
    case "selection":
      return `“${ctx.quote.slice(0, 32)}${ctx.quote.length > 32 ? "…" : ""}”`;
    case "feed":
      return ctx.title;
    case "folder":
      return ctx.name;
    case "tag":
      return `#${ctx.name}`;
    case "view":
      return ctx.label || t("smart.all");
    case "briefing":
      return ctx.title;
  }
}

function toolDisplayName(name: string, t: (k: string) => string): string {
  const key = `ai.tool.${name}`;
  const label = t(key);
  return label && label !== key ? label : name;
}

function newSessionId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `sess-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`;
}

/** Global docked AI assistant sidebar (right 4th column).
 *  Supports both single-article Q&A (auto-tracking the currently open article)
 *  and Folo-style cross-feed timeline Q&A via read-only retrieval tools,
 *  explicit `@` context chips, selection quotes, and persisted chat sessions. */
export default function AIChatPanel({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const qc = useQueryClient();

  const aiChatWidth = useUi((s) => s.aiChatWidth);
  const selectedArticleId = useUi((s) => s.selectedArticleId);
  const query = useUi((s) => s.query);
  const queryLabel = useUi((s) => s.queryLabel);
  const activeSessionId = useUi((s) => s.aiActiveSessionId);
  const setActiveSessionId = useUi((s) => s.setAiActiveSessionId);
  const pinnedContexts = useUi((s) => s.aiPinnedContexts);
  const addAiContext = useUi((s) => s.addAiContext);
  const removeAiContext = useUi((s) => s.removeAiContext);
  const clearAiContexts = useUi((s) => s.clearAiContexts);

  // Current open article metadata (cached by Reader's React Query).
  const currentArticle = useQuery({
    queryKey: ["article", selectedArticleId],
    queryFn: () => api.getArticle(selectedArticleId!),
    enabled: selectedArticleId != null,
  });

  const feeds = useQuery({
    queryKey: ["feeds"],
    queryFn: api.listFeeds,
    enabled: open,
  });
  const folders = useQuery({
    queryKey: ["folders"],
    queryFn: api.listFolders,
    enabled: open,
  });
  const tags = useQuery({
    queryKey: ["tags"],
    queryFn: api.listTags,
    enabled: open,
  });
  const sessions = useQuery({
    queryKey: ["ai-sessions"],
    queryFn: api.aiChatListSessions,
    enabled: open,
  });

  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [loadingSession, setLoadingSession] = useState(false);
  const [failed, setFailed] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [pickerFilter, setPickerFilter] = useState("");
  const [dismissedAutoKey, setDismissedAutoKey] = useState<string | null>(null);

  const runRef = useRef(0);
  const sessionLoadRef = useRef(0);
  const activeRunIdRef = useRef<string | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const stickToBottomRef = useRef(true);

  // Auto-context tracks the currently open article, or the active timeline view
  // when no article is open — unless the user explicitly dismissed it.
  const autoContext: AiContextRef | null = useMemo(() => {
    if (selectedArticleId != null && currentArticle.data) {
      return {
        kind: "article",
        articleId: currentArticle.data.id,
        title: currentArticle.data.title,
      };
    }
    if (query.kind === "feed") {
      return { kind: "feed", feedId: query.value, title: queryLabel };
    }
    if (query.kind === "folder") {
      return { kind: "folder", folderId: query.value, name: queryLabel };
    }
    if (query.kind === "tag") {
      return { kind: "tag", tagId: query.value, name: queryLabel };
    }
    return { kind: "view", query, label: queryLabel };
  }, [selectedArticleId, currentArticle.data, query, queryLabel]);

  const autoKey = autoContext ? JSON.stringify(autoContext) : null;
  const activeAutoContext =
    autoContext &&
    autoKey !== dismissedAutoKey &&
    !pinnedContexts.some((c) => JSON.stringify(c) === autoKey)
      ? autoContext
      : null;

  // Effective contexts sent with the next user turn.
  const effectiveContexts: AiContextRef[] = useMemo(() => {
    const list: AiContextRef[] = [];
    if (activeAutoContext) list.push(activeAutoContext);
    for (const c of pinnedContexts) list.push(c);
    return list;
  }, [activeAutoContext, pinnedContexts]);

  // Focus the composer when the pane opens or when a selection quote is added.
  useEffect(() => {
    if (open) {
      const id = window.setTimeout(() => inputRef.current?.focus(), 30);
      return () => window.clearTimeout(id);
    }
  }, [open, pinnedContexts.length]);

  useEffect(() => {
    return () => {
      runRef.current = 0;
      if (activeRunIdRef.current) {
        api.aiChatAbort(activeRunIdRef.current).catch(() => {});
        activeRunIdRef.current = null;
      }
    };
  }, []);

  useEffect(() => {
    const el = scrollRef.current;
    if (el && stickToBottomRef.current) el.scrollTop = el.scrollHeight;
  }, [messages, busy, failed]);

  const onThreadScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    stickToBottomRef.current =
      el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };

  const abortActiveRun = () => {
    if (activeRunIdRef.current) {
      api.aiChatAbort(activeRunIdRef.current).catch(() => {});
      activeRunIdRef.current = null;
    }
    runRef.current += 1;
    setBusy(false);
  };

  const loadSession = async (sessionId: string) => {
    abortActiveRun();
    const loadId = ++sessionLoadRef.current;
    setLoadingSession(true);
    try {
      const rows = await api.aiChatGetMessages(sessionId);
      if (sessionLoadRef.current !== loadId) return;
      setActiveSessionId(sessionId);
      setMessages(
        rows.map((r) => ({
          role: r.role,
          content: r.content,
          contexts: safeParseJson<AiContextRef[]>(r.contextsJson),
          tools: safeParseJson<AiToolTrace[]>(r.toolsJson),
        })),
      );
      setFailed(false);
      setHistoryOpen(false);
    } catch (e) {
      if (sessionLoadRef.current === loadId) reportError(e);
    } finally {
      if (sessionLoadRef.current === loadId) setLoadingSession(false);
    }
  };

  const startNewChat = () => {
    abortActiveRun();
    sessionLoadRef.current += 1;
    setLoadingSession(false);
    setFailed(false);
    setActiveSessionId(null);
    setMessages([]);
    clearAiContexts();
    setDismissedAutoKey(null);
    setHistoryOpen(false);
    inputRef.current?.focus();
  };

  const deleteSession = async (id: string) => {
    if (useUi.getState().aiActiveSessionId === id) {
      abortActiveRun();
      sessionLoadRef.current += 1;
      setLoadingSession(false);
    }
    try {
      await api.aiChatDeleteSession(id);
      // Re-read the live activeSessionId after await so switching to another
      // session mid-delete never wipes the newly loaded session.
      if (useUi.getState().aiActiveSessionId === id) {
        setActiveSessionId(null);
        setMessages([]);
      }
      await qc.invalidateQueries({ queryKey: ["ai-sessions"] });
    } catch (e) {
      reportError(e);
    }
  };

  const run = (
    history: ChatTurn[],
    question: string,
    contexts: AiContextRef[],
    sessionId: string,
  ) => {
    setBusy(true);
    setFailed(false);
    const runId = ++runRef.current;
    const wireRunId = `chat-${Date.now()}-${runId}`;
    activeRunIdRef.current = wireRunId;
    let sawErrorEvent = false;

    const patchLast = (patch: (m: ChatMessage) => ChatMessage) =>
      setMessages((m) => {
        if (m.length === 0) return m;
        const next = [...m];
        next[next.length - 1] = patch(next[next.length - 1]);
        return next;
      });

    api
      .aiGlobalChat(wireRunId, sessionId, contexts, history, question, (ev) => {
        if (runRef.current !== runId) return;
        if (ev.type === "delta") {
          patchLast((m) => ({ ...m, content: m.content + ev.data }));
        } else if (ev.type === "toolCall") {
          patchLast((m) => ({
            ...m,
            tools: [
              ...(m.tools ?? []),
              { id: ev.data.id, name: ev.data.name, args: ev.data.args },
            ],
          }));
        } else if (ev.type === "toolResult") {
          patchLast((m) => ({
            ...m,
            tools: (m.tools ?? []).map((tr) =>
              tr.id === ev.data.id ? { ...tr, summary: ev.data.summary } : tr,
            ),
          }));
        } else if (ev.type === "error") {
          sawErrorEvent = true;
          setFailed(true);
          toast.error(ev.data);
        }
      })
      .catch((e) => {
        if (runRef.current !== runId || sawErrorEvent) return;
        setFailed(true);
        reportError(e);
      })
      .finally(() => {
        if (runRef.current === runId) {
          activeRunIdRef.current = null;
          setBusy(false);
          qc.invalidateQueries({ queryKey: ["ai-sessions"] });
        }
      });
  };

  const sendQuestion = (rawQuestion: string) => {
    const question = rawQuestion.trim();
    if (!question || busy || loadingSession) return;
    sessionLoadRef.current += 1;
    setDraft("");
    setPickerOpen(false);
    stickToBottomRef.current = true;

    const sid = activeSessionId ?? newSessionId();
    if (!activeSessionId) setActiveSessionId(sid);

    const history: ChatTurn[] = messages
      .filter((m) => m.content.trim().length > 0)
      .map((m) => ({
        role: m.role,
        content: m.content,
      }));
    const ctxSnapshot = [...effectiveContexts];
    // Clear one-shot selection quote after sending so follow-up turns stay clean.
    if (pinnedContexts.some((c) => c.kind === "selection")) {
      const idx = pinnedContexts.findIndex((c) => c.kind === "selection");
      if (idx >= 0) removeAiContext(idx);
    }

    setMessages((m) => [
      ...m,
      { role: "user", content: question, contexts: ctxSnapshot },
      { role: "assistant", content: "", tools: [] },
    ]);
    run(history, question, ctxSnapshot, sid);
  };

  const send = () => sendQuestion(draft);

  const stop = () => {
    if (activeRunIdRef.current) {
      api.aiChatAbort(activeRunIdRef.current).catch(() => {});
    }
  };

  const retry = () => {
    if (busy || loadingSession) return;
    sessionLoadRef.current += 1;
    const lastUserIdx = messages.map((m) => m.role).lastIndexOf("user");
    if (lastUserIdx < 0) return;
    const sid = activeSessionId ?? newSessionId();
    if (!activeSessionId) setActiveSessionId(sid);
    const history: ChatTurn[] = messages
      .slice(0, lastUserIdx)
      .filter((m) => m.content.trim().length > 0)
      .map((m) => ({ role: m.role, content: m.content }));
    const lastUser = messages[lastUserIdx];
    setMessages((m) => {
      const next = [...m];
      if (next.length > 0 && next[next.length - 1].role === "assistant") {
        next[next.length - 1] = { role: "assistant", content: "", tools: [] };
      }
      return next;
    });
    run(history, lastUser.content, lastUser.contexts ?? effectiveContexts, sid);
  };

  const rendered = useMemo(
    () =>
      messages.map((m) =>
        m.role === "assistant" && m.content ? renderMarkdown(m.content) : "",
      ),
    [messages],
  );

  const onKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      send();
    }
  };

  // Starter prompt suggestions (adapt to whether an article is open).
  const starters = useMemo(() => {
    if (selectedArticleId != null && currentArticle.data) {
      return [
        t("ai.starterRecapArticle"),
        t("ai.starterExplainArticle"),
        t("ai.starterDailyBriefing"),
        t("ai.starterReviewHighlights"),
      ];
    }
    return [
      t("ai.starterDailyBriefing"),
      t("ai.starterUnreadSummary"),
      t("ai.starterTopTrends"),
      t("ai.starterReviewHighlights"),
    ];
  }, [selectedArticleId, currentArticle.data, t]);

  const pickerItems = useMemo(() => {
    const q = pickerFilter.trim().toLowerCase();
    const out: { key: string; icon: "folder" | "rss" | "tag"; label: string; ctx: AiContextRef }[] = [];
    for (const f of folders.data ?? []) {
      if (!q || f.name.toLowerCase().includes(q)) {
        out.push({
          key: `folder-${f.id}`,
          icon: "folder",
          label: f.name,
          ctx: { kind: "folder", folderId: f.id, name: f.name },
        });
      }
    }
    for (const f of feeds.data ?? []) {
      if (!q || f.title.toLowerCase().includes(q)) {
        out.push({
          key: `feed-${f.id}`,
          icon: "rss",
          label: f.title,
          ctx: { kind: "feed", feedId: f.id, title: f.title },
        });
      }
    }
    for (const tg of tags.data ?? []) {
      if (!q || tg.name.toLowerCase().includes(q)) {
        out.push({
          key: `tag-${tg.id}`,
          icon: "tag",
          label: `#${tg.name}`,
          ctx: { kind: "tag", tagId: tg.id, name: tg.name },
        });
      }
    }
    return out.slice(0, 24);
  }, [pickerFilter, folders.data, feeds.data, tags.data]);

  const empty = messages.length === 0;

  if (!open) return null;

  return (
    <aside
      className="ai-chat ai-chat--docked open"
      role="complementary"
      aria-label={t("ai.title")}
      style={{ width: aiChatWidth }}
    >
      <div className="resize-handle-slot resize-handle-slot--inline">
        <ResizeHandle
          width={aiChatWidth}
          side="left"
          min={PANEL_BOUNDS.aiChat.min}
          max={PANEL_BOUNDS.aiChat.max}
          onResize={(w) => useUi.getState().setPanel({ aiChatWidth: w })}
          label={t("reader.resizeAiChat")}
        />
      </div>

      <div className="ai-head">
        <span className="accent-ico">
          <Icon name="sparkle" size={15} />
        </span>
        <h3>{t("ai.title")}</h3>
        <div className="ai-head-actions">
          <button
            type="button"
            className={`tb-btn ${historyOpen ? "on" : ""}`}
            onClick={() => setHistoryOpen((v) => !v)}
            title={t("ai.history")}
            aria-label={t("ai.history")}
          >
            <Icon name="clock" size={14} />
          </button>
          <button
            type="button"
            className="tb-btn"
            onClick={startNewChat}
            title={t("ai.newChat")}
            aria-label={t("ai.newChat")}
          >
            <Icon name="plus" size={14} />
          </button>
          <button
            type="button"
            className="tb-btn close"
            onClick={onClose}
            title={t("common.close")}
            aria-label={t("common.close")}
          >
            <Icon name="x" size={14} />
          </button>
        </div>
      </div>

      {historyOpen && (
        <div className="ai-history-drawer">
          <div className="ai-history-head">
            <span>{t("ai.history")}</span>
            <button type="button" className="ai-history-new" onClick={startNewChat}>
              <Icon name="plus" size={11} />
              {t("ai.newChat")}
            </button>
          </div>
          {(sessions.data ?? []).length === 0 ? (
            <div className="ai-history-empty">{t("ai.noHistory")}</div>
          ) : (
            <div className="ai-history-list">
              {(sessions.data ?? []).map((s) => (
                <div
                  key={s.id}
                  className={`ai-history-row ${activeSessionId === s.id ? "active" : ""}`}
                  role="button"
                  tabIndex={0}
                  onClick={() => void loadSession(s.id)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      void loadSession(s.id);
                    }
                  }}
                >
                  <span className="ai-history-title">{s.title || t("ai.untitledSession")}</span>
                  <button
                    type="button"
                    className="ai-history-del"
                    title={t("common.delete")}
                    aria-label={t("common.delete")}
                    onClick={(e) => {
                      e.stopPropagation();
                      void deleteSession(s.id);
                    }}
                  >
                    <Icon name="trash" size={12} />
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      <div
        className="ai-chat-thread"
        ref={scrollRef}
        onScroll={onThreadScroll}
        aria-live="polite"
        aria-busy={busy}
      >
        {empty && (
          <div className="ai-chat-empty">
            <Icon name="sparkle" size={22} />
            <p>{t("ai.emptyHint")}</p>
            <div className="ai-starters">
              {starters.map((prompt) => (
                <button
                  key={prompt}
                  type="button"
                  className="ai-starter-chip"
                  onClick={() => sendQuestion(prompt)}
                >
                  {prompt}
                </button>
              ))}
            </div>
          </div>
        )}

        {messages.map((m, i) =>
          m.role === "user" ? (
            <div className="ai-chat-msg user" key={i}>
              {m.contexts && m.contexts.length > 0 && (
                <div className="ai-msg-contexts">
                  {m.contexts.map((c, ci) => (
                    <span className="ai-msg-ctx-tag" key={ci}>
                      {contextLabel(c, t)}
                    </span>
                  ))}
                </div>
              )}
              <div>{m.content}</div>
            </div>
          ) : (
            <div className="ai-chat-msg assistant" key={i}>
              {m.tools && m.tools.length > 0 && (
                <div className="ai-tool-traces">
                  {m.tools.map((tr) => (
                    <div
                      key={tr.id}
                      className={`ai-tool-pill ${tr.summary ? "done" : "running"}`}
                    >
                      <Icon name="search" size={11} />
                      <span className="ai-tool-name">{toolDisplayName(tr.name, t)}</span>
                      {tr.summary ? (
                        <span className="ai-tool-summary">{tr.summary}</span>
                      ) : (
                        <span className="ai-tool-summary">…</span>
                      )}
                    </div>
                  ))}
                </div>
              )}
              {m.content ? (
                <div
                  className="ai-prose"
                  onClick={chatLinkClick}
                  dangerouslySetInnerHTML={{ __html: rendered[i] }}
                />
              ) : (
                busy &&
                i === messages.length - 1 && (
                  <div className="ai-loading">
                    <span className="ai-dot" />
                    <span className="ai-dot" />
                    <span className="ai-dot" />
                  </div>
                )
              )}
            </div>
          ),
        )}

        {failed && !busy && (
          <div className="ai-chat-failed">
            <Icon name="alert" size={15} />
            <span>{t("reader.aiChatFailed")}</span>
            <button type="button" className="empty-retry" onClick={retry}>
              <Icon name="refresh" size={12} />
              {t("common.retry")}
            </button>
          </div>
        )}
      </div>

      {/* Context chips bar + `@` context picker */}
      <div className="ai-context-bar">
        {activeAutoContext && (
          <span className="ai-ctx-chip ai-ctx-chip--auto" title={contextLabel(activeAutoContext, t)}>
            <Icon
              name={activeAutoContext.kind === "article" ? "doc" : "rss"}
              size={11}
            />
            <span className="ai-ctx-label">{contextLabel(activeAutoContext, t)}</span>
            <button
              type="button"
              className="ai-ctx-x"
              onClick={() => setDismissedAutoKey(autoKey)}
              aria-label={t("common.close")}
            >
              <Icon name="x" size={10} />
            </button>
          </span>
        )}
        {pinnedContexts.map((ctx, idx) => (
          <span
            key={idx}
            className={`ai-ctx-chip ${ctx.kind === "selection" ? "ai-ctx-chip--quote" : ""}`}
            title={contextLabel(ctx, t)}
          >
            <Icon
              name={
                ctx.kind === "selection"
                  ? "text"
                  : ctx.kind === "folder"
                    ? "folder"
                    : ctx.kind === "tag"
                      ? "tag"
                      : ctx.kind === "briefing"
                        ? "sparkle"
                        : "rss"
              }
              size={11}
            />
            <span className="ai-ctx-label">{contextLabel(ctx, t)}</span>
            <button
              type="button"
              className="ai-ctx-x"
              onClick={() => removeAiContext(idx)}
              aria-label={t("common.close")}
            >
              <Icon name="x" size={10} />
            </button>
          </span>
        ))}
        <button
          type="button"
          className="ai-ctx-add"
          onClick={() => {
            setPickerOpen((v) => !v);
            setPickerFilter("");
          }}
          title={t("ai.addContext")}
          aria-label={t("ai.addContext")}
        >
          <Icon name="plus" size={11} />
          <span>{t("ai.contextBtn")}</span>
        </button>
      </div>

      {pickerOpen && (
        <div className="ai-ctx-picker">
          <input
            type="text"
            className="ai-ctx-picker-input"
            value={pickerFilter}
            onChange={(e) => setPickerFilter(e.target.value)}
            placeholder={t("ai.searchContextPlaceholder")}
            autoFocus
          />
          <div className="ai-ctx-picker-list">
            {pickerItems.map((it) => (
              <button
                key={it.key}
                type="button"
                className="ai-ctx-picker-item"
                onClick={() => {
                  addAiContext(it.ctx);
                  setPickerOpen(false);
                }}
              >
                <Icon name={it.icon} size={12} />
                <span>{it.label}</span>
              </button>
            ))}
          </div>
        </div>
      )}

      <div className="ai-chat-composer">
        <textarea
          ref={inputRef}
          value={draft}
          placeholder={t("ai.placeholder")}
          aria-label={t("ai.placeholder")}
          rows={1}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKey}
        />
        {busy ? (
          <button
            type="button"
            className="tb-btn send stop"
            onClick={stop}
            title={t("ai.stop")}
            aria-label={t("ai.stop")}
          >
            <Icon name="x" size={14} />
          </button>
        ) : (
          <button
            type="button"
            className="tb-btn send"
            onClick={send}
            disabled={loadingSession || draft.trim() === ""}
            title={t("reader.aiChatSend")}
            aria-label={t("reader.aiChatSend")}
          >
            <Icon name="send-fill" size={14} />
          </button>
        )}
      </div>
      <div className="ai-card-foot">{t("ai.disclaimer")}</div>
    </aside>
  );
}
