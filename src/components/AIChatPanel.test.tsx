// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import AIChatPanel from "./AIChatPanel";
import { useUi } from "../store";
import i18n from "../i18n";
import type { AiContextRef } from "../types";

const h = vi.hoisted(() => {
  if (
    typeof globalThis.localStorage === "undefined" ||
    typeof globalThis.localStorage?.getItem !== "function"
  ) {
    const store = new Map<string, string>();
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => {
          store.set(k, String(v));
        },
        removeItem: (k: string) => {
          store.delete(k);
        },
        clear: () => store.clear(),
      },
    });
  }

  return {
    article: {
      id: 7,
      feedId: 1,
      feedTitle: "Example Feed",
      sourceType: "rss" as const,
      title: "An Article",
      author: null,
      url: "https://example.com/a1",
      contentHtml: "<p>hello world</p>",
      extractedHtml: null,
      imageUrl: null,
      publishedAt: null,
      isRead: true,
      isStarred: false,
      readLater: false,
      aiSummary: null,
      translatedHtml: null,
      translatedLang: null,
      translatedTitle: null,
      enclosures: [],
      tags: [],
    },
    aiGlobalChat: vi.fn(),
    aiChatAbort: vi.fn(async () => {}),
    aiChatListSessions: vi.fn(async () => []),
    aiChatGetMessages: vi.fn(async () => []),
    aiChatDeleteSession: vi.fn(async () => {}),
    getArticle: vi.fn(),
    listFeeds: vi.fn(async () => []),
    listFolders: vi.fn(async () => []),
    listTags: vi.fn(async () => []),
    toastError: vi.fn(),
    reportError: vi.fn(),
  };
});

vi.mock("../api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api")>();
  return {
    ...actual,
    aiGlobalChat: h.aiGlobalChat,
    aiChatAbort: h.aiChatAbort,
    aiChatListSessions: h.aiChatListSessions,
    aiChatGetMessages: h.aiChatGetMessages,
    aiChatDeleteSession: h.aiChatDeleteSession,
    getArticle: h.getArticle,
    listFeeds: h.listFeeds,
    listFolders: h.listFolders,
    listTags: h.listTags,
  };
});

vi.mock("../toast", () => ({
  toast: { error: (...args: unknown[]) => h.toastError(...args) },
  reportError: (...args: unknown[]) => h.reportError(...args),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn(async () => {}) }));

const renderPanel = (open = true) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AIChatPanel open={open} onClose={() => {}} />
    </QueryClientProvider>,
  );
};

async function ask(question: string, container: HTMLElement) {
  const box = container.querySelector("textarea") as HTMLTextAreaElement;
  fireEvent.change(box, { target: { value: question } });
  fireEvent.keyDown(box, { key: "Enter", shiftKey: false });
  await waitFor(() => expect(h.aiGlobalChat).toHaveBeenCalled());
  return h.aiGlobalChat.mock.calls[h.aiGlobalChat.mock.calls.length - 1];
}

describe("AIChatPanel (Global)", () => {
  beforeEach(() => {
    h.aiGlobalChat.mockReset();
    h.aiChatAbort.mockReset().mockResolvedValue(undefined);
    h.aiChatListSessions.mockReset().mockResolvedValue([]);
    h.getArticle.mockReset().mockResolvedValue(h.article);
    h.toastError.mockReset();
    h.reportError.mockReset();
    useUi.setState({
      aiChatWidth: 360,
      selectedArticleId: 7,
      aiActiveSessionId: null,
      aiPinnedContexts: [],
      aiPendingPrompt: null,
    });
  });
  afterEach(() => cleanup());

  it("streams deltas and tool traces, and attaches current article context", async () => {
    h.aiGlobalChat.mockImplementation(
      (
        _runId: string,
        _sid: string | null,
        _ctx: unknown,
        _history: unknown,
        _q: string,
        onToken: (e: { type: string; data?: unknown }) => void,
      ) => {
        onToken({
          type: "toolCall",
          data: { id: "t1", name: "search_articles", args: '{"query":"test"}' },
        });
        onToken({
          type: "toolResult",
          data: { id: "t1", name: "search_articles", summary: "3 articles" },
        });
        onToken({ type: "delta", data: "Hel" });
        onToken({ type: "delta", data: "lo there" });
        onToken({ type: "done" });
        return Promise.resolve();
      },
    );
    const { container } = renderPanel();
    await waitFor(() => expect(h.getArticle).toHaveBeenCalledWith(7));
    const [, , contexts, history, question] = await ask("这篇文章讲了什么？", container);
    expect(history).toEqual([]);
    expect(question).toBe("这篇文章讲了什么？");
    expect(contexts).toEqual([{ kind: "article", articleId: 7, title: "An Article" }]);
    await waitFor(() => expect(container.textContent).toContain("Hello there"));
    expect(container.textContent).toContain("3 articles");
  });

  it("keeps the user's question and offers a retry when the stream errors", async () => {
    h.aiGlobalChat.mockImplementation(
      (
        _runId: string,
        _sid: string | null,
        _ctx: unknown,
        _history: unknown,
        _q: string,
        onToken: (e: { type: string; data?: unknown }) => void,
      ) => {
        onToken({ type: "delta", data: "first answer" });
        onToken({ type: "done" });
        return Promise.resolve();
      },
    );
    const { container } = renderPanel();
    await ask("第一问", container);
    await waitFor(() => expect(container.textContent).toContain("first answer"));

    h.aiGlobalChat.mockImplementation(
      (
        _runId: string,
        _sid: string | null,
        _ctx: unknown,
        _history: unknown,
        _q: string,
        onToken: (e: { type: string; data?: unknown }) => void,
      ) => {
        onToken({ type: "error", data: "provider down" });
        return Promise.resolve();
      },
    );
    await ask("第二问", container);
    await waitFor(() => expect(container.textContent).toContain("第二问"));
    const retry = container.querySelector(".ai-chat-failed .empty-retry") as HTMLButtonElement;
    expect(retry).not.toBeNull();

    h.aiGlobalChat.mockImplementation(
      (
        _runId: string,
        _sid: string | null,
        _ctx: unknown,
        _history: unknown,
        _q: string,
        onToken: (e: { type: string; data?: unknown }) => void,
      ) => {
        onToken({ type: "delta", data: "现在好了" });
        onToken({ type: "done" });
        return Promise.resolve();
      },
    );
    await act(async () => {
      fireEvent.click(retry);
    });
    await waitFor(() => expect(h.aiGlobalChat).toHaveBeenCalledTimes(3));
    const [, , , history, question] = h.aiGlobalChat.mock.calls[2];
    expect(question).toBe("第二问");
    expect(history).toEqual([
      { role: "user", content: "第一问" },
      { role: "assistant", content: "first answer" },
    ]);
    await waitFor(() => expect(container.textContent).toContain("现在好了"));
  });

  it("renders nothing when closed and docks when open", () => {
    const { container } = renderPanel(false);
    expect(container.querySelector(".ai-chat")).toBeNull();
    const opened = renderPanel(true);
    expect(opened.container.querySelector(".ai-chat--docked")).not.toBeNull();
  });

  it("auto-sends a prompt queued while the panel is already open and idle", async () => {
    // The reader's empty-state "summarize this timeline" action sets
    // aiPendingPrompt without toggling open/busy — the queue itself must
    // trigger the send, or the prompt would sit until some later interaction.
    h.aiGlobalChat.mockResolvedValue(undefined);
    const { container } = renderPanel(true);
    await waitFor(() => expect(h.getArticle).toHaveBeenCalledWith(7));

    const queuedContexts: AiContextRef[] = [
      { kind: "view", query: { kind: "all" }, label: "All articles" },
    ];
    await act(async () => {
      useUi.setState({
        aiPendingPrompt: {
          text: "Summarize this timeline",
          contexts: queuedContexts,
        },
      });
    });
    await waitFor(() => expect(h.aiGlobalChat).toHaveBeenCalledTimes(1));
    const [, , contexts, history, question] = h.aiGlobalChat.mock.calls[0];
    expect(question).toBe("Summarize this timeline");
    expect(history).toEqual([]);
    // The queued item's contexts are sent verbatim — not the auto context of
    // the render that happened to fire the send (an article is open here).
    expect(contexts).toEqual(queuedContexts);
    // Consuming is one-shot: the queue is empty afterwards.
    expect(useUi.getState().aiPendingPrompt).toBeNull();
    expect(container.textContent).toContain("Summarize this timeline");
  });

  it("sends a queued prompt with the context captured at queue time", async () => {
    // The reader's empty-state action queues a timeline summary while a reply
    // is still streaming; the user then switches to another feed. When the
    // panel goes idle and the queued prompt finally sends, it must summarize
    // the timeline it was queued on — the snapshot — not the switched-to feed.
    let releaseFirst!: () => void;
    const firstRun = new Promise<void>((resolve) => {
      releaseFirst = resolve;
    });
    // The hanging first exchange; every later call (the queued send) resolves.
    h.aiGlobalChat.mockImplementationOnce(() => firstRun);
    h.aiGlobalChat.mockResolvedValue(undefined);
    useUi.setState({ selectedArticleId: null }); // no article → timeline context
    const { container } = renderPanel(true);
    await waitFor(() =>
      expect(container.querySelector("textarea")).not.toBeNull(),
    );

    // The first exchange occupies the panel; the timeline summary is queued
    // mid-run, while the "all" view is still active.
    await ask("first question", container);
    await act(async () => {
      useUi.getState().askAiSummarizeTimeline();
    });
    // The user switches timeline while the first reply is streaming.
    await act(async () => {
      useUi.setState({
        query: { kind: "feed", value: 3 },
        queryLabel: "Other feed",
      });
    });
    // First reply ends → the panel goes idle → the queued prompt sends.
    await act(async () => {
      releaseFirst();
    });
    await waitFor(() => expect(h.aiGlobalChat).toHaveBeenCalledTimes(2));
    const [, , contexts, , question] = h.aiGlobalChat.mock.calls[1];
    expect(question).toBe(i18n.t("ai.summarizeTimelinePrompt"));
    // Queue-time snapshot: the "all" view, NOT the feed switched to after.
    expect(contexts).toEqual([
      {
        kind: "view",
        query: { kind: "all" },
        label: i18n.t("smart.all"),
      },
    ]);
    expect(useUi.getState().aiPendingPrompt).toBeNull();
  });

  it("keeps the composer draft on a queued auto-send; editor submit still clears it", async () => {
    // Editor submission sends the draft itself, so it must clear the composer.
    h.aiGlobalChat.mockResolvedValue(undefined);
    const { container } = renderPanel(true);
    await waitFor(() => expect(h.getArticle).toHaveBeenCalledWith(7));
    const box = container.querySelector("textarea") as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "first typed question" } });
    fireEvent.keyDown(box, { key: "Enter", shiftKey: false });
    await waitFor(() => expect(h.aiGlobalChat).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(box.value).toBe(""));

    // A queued auto-send carries its own text, not the composer's — while a
    // reply streams (or right after one ends) the user may already be typing
    // the next question, and draining the queue must not wipe that draft.
    fireEvent.change(box, { target: { value: "next question I am typing" } });
    const queuedContexts: AiContextRef[] = [
      { kind: "view", query: { kind: "all" }, label: "All articles" },
    ];
    await act(async () => {
      useUi.setState({
        aiPendingPrompt: {
          text: "Summarize this timeline",
          contexts: queuedContexts,
        },
      });
    });
    await waitFor(() => expect(h.aiGlobalChat).toHaveBeenCalledTimes(2));
    expect(h.aiGlobalChat.mock.calls[1][4]).toBe("Summarize this timeline");
    // The queued send left the user's half-typed question untouched.
    expect(box.value).toBe("next question I am typing");
    expect(useUi.getState().aiPendingPrompt).toBeNull();
  });
});
