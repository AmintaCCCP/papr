import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import * as api from "../api";
import { useUi } from "../store";
import { renderMarkdown } from "../lib/markdown";
import { isMac } from "../lib/platform";
import { reportError, toast } from "../toast";
import type { AiBriefing } from "../types";
import Icon from "./Icon";

const DAY_OPTIONS = [1, 3, 7] as const;

function handleBriefingLinkClick(e: React.MouseEvent) {
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

export default function AIBriefingView() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const askAiAboutBriefing = useUi((s) => s.askAiAboutBriefing);

  const folders = useQuery({ queryKey: ["folders"], queryFn: api.listFolders });
  const briefings = useQuery({
    queryKey: ["ai-briefings"],
    queryFn: api.aiListBriefings,
  });

  const [days, setDays] = useState<number>(1);
  const [scopeKey, setScopeKey] = useState<string>("all");
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [streaming, setStreaming] = useState(false);
  const [streamText, setStreamText] = useState("");
  const runIdRef = useRef<string | null>(null);

  const list = briefings.data ?? [];
  const activeBriefing: AiBriefing | null = useMemo(() => {
    if (selectedId != null) {
      return list.find((b) => b.id === selectedId) ?? list[0] ?? null;
    }
    return list[0] ?? null;
  }, [list, selectedId]);

  useEffect(() => {
    return () => {
      if (runIdRef.current) {
        api.aiChatAbort(runIdRef.current).catch(() => {});
        runIdRef.current = null;
      }
    };
  }, []);

  const resolveScope = (): {
    folderId: number | null;
    unreadOnly: boolean;
    starredOnly: boolean;
    label: string;
  } => {
    if (scopeKey === "unread") {
      return { folderId: null, unreadOnly: true, starredOnly: false, label: t("smart.unread") };
    }
    if (scopeKey === "starred") {
      return { folderId: null, unreadOnly: false, starredOnly: true, label: t("smart.starred") };
    }
    if (scopeKey.startsWith("folder:")) {
      const fid = Number(scopeKey.slice(7));
      const f = (folders.data ?? []).find((x) => x.id === fid);
      return {
        folderId: fid,
        unreadOnly: false,
        starredOnly: false,
        label: f?.name ?? t("smart.all"),
      };
    }
    return { folderId: null, unreadOnly: false, starredOnly: false, label: t("smart.all") };
  };

  const generate = async () => {
    if (streaming) return;
    const scope = resolveScope();
    const runId = `briefing-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    runIdRef.current = runId;
    setStreaming(true);
    setStreamText("");
    let sawError = false;
    try {
      const saved = await api.aiGenerateBriefing(
        {
          runId,
          days,
          folderId: scope.folderId,
          feedId: null,
          unreadOnly: scope.unreadOnly,
          starredOnly: scope.starredOnly,
          scopeLabel: scope.label,
        },
        (ev) => {
          if (runIdRef.current !== runId) return;
          if (ev.type === "delta") {
            setStreamText((prev) => prev + ev.data);
          } else if (ev.type === "error") {
            sawError = true;
            toast.error(ev.data);
          }
        },
      );
      if (saved) {
        await qc.invalidateQueries({ queryKey: ["ai-briefings"] });
        setSelectedId(saved.id);
      }
    } catch (e) {
      if (!sawError) reportError(e);
    } finally {
      if (runIdRef.current === runId) {
        runIdRef.current = null;
        setStreaming(false);
      }
    }
  };

  const stop = () => {
    if (runIdRef.current) {
      api.aiChatAbort(runIdRef.current).catch(() => {});
    }
  };

  const remove = async (id: number) => {
    try {
      await api.aiDeleteBriefing(id);
      if (selectedId === id) setSelectedId(null);
      await qc.invalidateQueries({ queryKey: ["ai-briefings"] });
    } catch (e) {
      reportError(e);
    }
  };

  const displayMarkdown = streaming ? streamText : activeBriefing?.content ?? "";
  const renderedHtml = useMemo(
    () => (displayMarkdown ? renderMarkdown(displayMarkdown) : ""),
    [displayMarkdown],
  );

  return (
    <div className="briefing-view" role="main" aria-label={t("briefing.title")}>
      <div
        className="briefing-header"
        {...(isMac && { "data-tauri-drag-region": true })}
      >
        <div className="briefing-title-group">
          <span className="accent-ico">
            <Icon name="sparkle" size={16} />
          </span>
          <h2>{t("briefing.title")}</h2>
        </div>

        <div className="briefing-controls">
          <div className="briefing-days" role="group" aria-label={t("briefing.timeRange")}>
            {DAY_OPTIONS.map((d) => (
              <button
                key={d}
                type="button"
                className={`briefing-pill ${days === d ? "active" : ""}`}
                onClick={() => setDays(d)}
                disabled={streaming}
              >
                {t(`briefing.days${d}`)}
              </button>
            ))}
          </div>

          <select
            className="s-select briefing-scope"
            value={scopeKey}
            disabled={streaming}
            aria-label={t("briefing.scope")}
            onChange={(e) => setScopeKey(e.target.value)}
          >
            <option value="all">{t("smart.all")}</option>
            <option value="unread">{t("smart.unread")}</option>
            <option value="starred">{t("smart.starred")}</option>
            {(folders.data ?? []).map((f) => (
              <option key={f.id} value={`folder:${f.id}`}>
                {f.name}
              </option>
            ))}
          </select>

          {streaming ? (
            <button type="button" className="s-btn" onClick={stop}>
              <Icon name="x" size={13} />
              {t("ai.stop")}
            </button>
          ) : (
            <button type="button" className="s-btn primary" onClick={generate}>
              <Icon name="sparkle" size={13} />
              {t("briefing.generate")}
            </button>
          )}
        </div>
      </div>

      <div className="briefing-body">
        <aside className="briefing-history" aria-label={t("briefing.history")}>
          <div className="briefing-history-head">{t("briefing.history")}</div>
          {list.length === 0 ? (
            <div className="briefing-history-empty">{t("briefing.noHistory")}</div>
          ) : (
            <div className="briefing-history-list">
              {list.map((b) => {
                const isSel = !streaming && activeBriefing?.id === b.id;
                return (
                  <div
                    key={b.id}
                    className={`briefing-history-item ${isSel ? "active" : ""}`}
                    role="button"
                    tabIndex={0}
                    onClick={() => setSelectedId(b.id)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        setSelectedId(b.id);
                      }
                    }}
                  >
                    <div className="briefing-history-title">{b.title}</div>
                    <div className="briefing-history-meta">
                      <span>{b.createdAt.slice(0, 16)}</span>
                      <span>·</span>
                      <span>{t("briefing.articleCount", { count: b.articleCount })}</span>
                    </div>
                    <button
                      type="button"
                      className="briefing-history-del"
                      title={t("common.delete")}
                      aria-label={t("common.delete")}
                      onClick={(e) => {
                        e.stopPropagation();
                        void remove(b.id);
                      }}
                    >
                      <Icon name="trash" size={12} />
                    </button>
                  </div>
                );
              })}
            </div>
          )}
        </aside>

        <div className="briefing-content-scroll">
          {!displayMarkdown && !streaming ? (
            <div className="briefing-empty">
              <div className="glyph">
                <Icon name="sparkle" size={24} />
              </div>
              <h3>{t("briefing.emptyTitle")}</h3>
              <p>{t("briefing.emptyDesc")}</p>
              <button type="button" className="s-btn primary" onClick={generate}>
                <Icon name="sparkle" size={13} />
                {t("briefing.generateNow")}
              </button>
            </div>
          ) : (
            <article className="article reader-content briefing-article">
              {!streaming && activeBriefing && (
                <div className="briefing-article-head">
                  <div>
                    <h1 className="article-title">{activeBriefing.title}</h1>
                    <div className="article-meta">
                      <span>{activeBriefing.createdAt.slice(0, 16)}</span>
                      <span>·</span>
                      <span>
                        {t("briefing.articleCount", { count: activeBriefing.articleCount })}
                      </span>
                    </div>
                  </div>
                  <button
                    type="button"
                    className="s-btn"
                    onClick={() => askAiAboutBriefing(activeBriefing)}
                  >
                    <Icon name="chat" size={13} />
                    {t("briefing.askFollowUp")}
                  </button>
                </div>
              )}
              {streaming && !streamText && (
                <div className="ai-loading" style={{ padding: "24px 0" }}>
                  <span className="ai-dot" />
                  <span className="ai-dot" />
                  <span className="ai-dot" />
                </div>
              )}
              {renderedHtml && (
                <div
                  className="ai-prose briefing-prose"
                  onClick={handleBriefingLinkClick}
                  dangerouslySetInnerHTML={{ __html: renderedHtml }}
                />
              )}
            </article>
          )}
        </div>
      </div>
    </div>
  );
}
