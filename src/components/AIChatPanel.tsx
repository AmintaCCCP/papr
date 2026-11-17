import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import * as api from "../api";
import type { ChatTurn } from "../api";
import { PANEL_BOUNDS, useUi } from "../store";
import { renderMarkdown } from "../lib/markdown";
import { reportError, toast } from "../toast";
import Icon from "./Icon";
import ResizeHandle from "./ResizeHandle";
import type { ArticleDetail } from "../types";

/** One exchange in the sidebar conversation. The assistant's `content`
 *  accumulates stream deltas live. */
interface ChatMessage {
  role: "user" | "assistant";
  content: string;
}

/** Click handler for links inside an assistant reply: everything opens in the
 *  external browser (a bare click would navigate the Tauri webview away).
 *  Chat replies carry no in-page anchors, so the reader's fragment-scrolling
 *  handler isn't needed here. */
function chatLinkClick(e: React.MouseEvent) {
  const link = (e.target as HTMLElement).closest("a");
  if (!link) return;
  const href = link.getAttribute("href");
  if (!href) return;
  e.preventDefault();
  openUrl(link.href).catch(() => {});
}

/** The reader's AI-chat sidebar: a right-edge sliding pane for asking
 *  questions about the article being read. Deliberately separate from the
 *  inline AI summary card — that card's interaction is untouched; this pane
 *  only ever hosts the conversation. The parent keys it by article id, so a
 *  switch remounts it with a fresh conversation; while closed the pane stays
 *  mounted (off-screen, inert) so a toggle doesn't lose the thread. */
export default function AIChatPanel({
  article,
  open,
  onClose,
}: {
  article: ArticleDetail;
  open: boolean;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const aiChatWidth = useUi((s) => s.aiChatWidth);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  // The latest send failed — shown inline with a retry action, the user's
  // question stays on the record so retrying doesn't need retyping.
  const [failed, setFailed] = useState(false);
  // Identifies the latest send. A panel closed mid-stream keeps streaming in
  // the background (the backend runs to completion regardless); only the run
  // that is still current may touch `busy` / the message list on settle.
  const runRef = useRef(0);
  const scrollRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // Whether the thread should follow the newest message: tracked from the
  // scroll events BEFORE the layout changes (the autoscroll effect below runs
  // after the DOM already grew — deciding there would read the post-growth
  // geometry and drop the follow the moment a delta adds enough height).
  const stickToBottomRef = useRef(true);

  // Focus the composer when the pane opens (the closed pane keeps its
  // content, so the input follows the pane on reopen too).
  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  // Invalidate any in-flight run on unmount so delayed stream events or promise
  // rejections from this panel cannot update state or show global error toasts
  // after the user has navigated to another article.
  useEffect(() => {
    return () => {
      runRef.current = 0;
    };
  }, []);

  // Follow the newest message while it streams in — but only while the user
  // hasn't scrolled up to re-read. Unconditional autoscroll would yank them
  // straight back down on every delta; scrolling back to the bottom re-arms
  // the follow.
  useEffect(() => {
    const el = scrollRef.current;
    if (el && stickToBottomRef.current) el.scrollTop = el.scrollHeight;
  }, [messages, busy, failed]);

  // Track the pre-update position from the user's own scrolls: near the
  // bottom keeps the follow armed, scrolling up disarms it. (The autoscroll
  // effect itself does not re-arm — it runs after layout grew.)
  const onThreadScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    stickToBottomRef.current =
      el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };

  const run = (
    history: ChatTurn[],
    question: string,
  ) => {
    setBusy(true);
    setFailed(false);
    const runId = ++runRef.current;
    // An in-stream error surfaces twice (an `error` channel event, then the
    // command's rejected promise) — only the first marks the failure; the
    // `.catch` still reports failures that abort before streaming starts
    // (no key, bad config) and never emit an `error` event.
    let sawErrorEvent = false;
    const patchLast = (patch: (m: ChatMessage) => ChatMessage) =>
      setMessages((m) => {
        if (m.length === 0) return m;
        const next = [...m];
        next[next.length - 1] = patch(next[next.length - 1]);
        return next;
      });
    api
      .aiArticleChat(article.id, history, question, (ev) => {
        if (runRef.current !== runId) return;
        if (ev.type === "delta") {
          patchLast((m) => ({ ...m, content: m.content + ev.data }));
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
        if (runRef.current === runId) setBusy(false);
      });
  };

  const send = () => {
    const question = draft.trim();
    if (!question || busy) return;
    setDraft("");
    // The user asked for this — follow the reply from its first token.
    stickToBottomRef.current = true;
    // The prior turns (oldest first) ride along as context; the user's
    // question and an empty assistant reply land at once, the reply
    // accumulating deltas in place.
    const history: ChatTurn[] = messages.map((m) => ({
      role: m.role,
      content: m.content,
    }));
    setMessages((m) => [
      ...m,
      { role: "user", content: question },
      { role: "assistant", content: "" },
    ]);
    run(history, question);
  };

  const retry = () => {
    if (busy) return;
    // Resend the last user turn in place: everything before it is the
    // history, its reply stub is patched as the new stream arrives. The stub
    // is emptied first — a failed request may have streamed a partial
    // fragment before dying, and appending the new stream to it would splice
    // the two answers together (and then send that concatenation as history).
    const lastUserIdx = messages.map((m) => m.role).lastIndexOf("user");
    if (lastUserIdx < 0) return;
    const history: ChatTurn[] = messages
      .slice(0, lastUserIdx)
      .map((m) => ({ role: m.role, content: m.content }));
    setMessages((m) => {
      const next = [...m];
      if (next.length > 0 && next[next.length - 1].role === "assistant") {
        next[next.length - 1] = { role: "assistant", content: "" };
      }
      return next;
    });
    run(history, messages[lastUserIdx].content);
  };

  // Assistant replies render as markdown; reparse only when the text changes,
  // not on every panel re-render.
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

  const empty = messages.length === 0;

  return (
    <div
      className={`ai-chat ${open ? "open" : ""}`}
      role="complementary"
      aria-label={t("reader.aiChatTitle")}
      style={{ width: aiChatWidth }}
      // When closed the pane is only moved off-screen — `inert` keeps its
      // controls and content out of the tab order and the a11y tree.
      inert={!open}
    >
      {/* Left-edge handle: dragging left widens the pane. Hidden from the a11y
          tree while the pane is closed (the whole pane is `inert`). */}
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
          <Icon name="chat" size={15} />
        </span>
        <h3>{t("reader.aiChatTitle")}</h3>
        <button
          className="tb-btn close"
          onClick={onClose}
          title={t("common.close")}
          aria-label={t("common.close")}
        >
          <Icon name="x" size={14} />
        </button>
      </div>
      <div
        className="ai-chat-thread"
        ref={scrollRef}
        onScroll={onThreadScroll}
        aria-live="polite"
        aria-busy={busy}
      >
        {empty && (
          <div className="ai-chat-empty">
            <Icon name="chat" size={22} />
            <p>{t("reader.aiChatEmpty")}</p>
          </div>
        )}
        {messages.map((m, i) =>
          m.role === "user" ? (
            <div className="ai-chat-msg user" key={i}>
              {m.content}
            </div>
          ) : (
            <div className="ai-chat-msg assistant" key={i}>
              {m.content ? (
                <div
                  className="ai-prose"
                  onClick={chatLinkClick}
                  dangerouslySetInnerHTML={{ __html: rendered[i] }}
                />
              ) : (
                busy && (
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
            <button className="empty-retry" onClick={retry}>
              <Icon name="refresh" size={12} />
              {t("common.retry")}
            </button>
          </div>
        )}
      </div>
      <div className="ai-chat-composer">
        <textarea
          ref={inputRef}
          value={draft}
          placeholder={t("reader.aiChatPlaceholder")}
          aria-label={t("reader.aiChatPlaceholder")}
          rows={1}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKey}
        />
        <button
          className="tb-btn send"
          onClick={send}
          disabled={busy || draft.trim() === ""}
          title={t("reader.aiChatSend")}
          aria-label={t("reader.aiChatSend")}
        >
          <Icon name="send-fill" size={14} />
        </button>
      </div>
      <div className="ai-card-foot">{t("reader.aiChatDisclaimer")}</div>
    </div>
  );
}
