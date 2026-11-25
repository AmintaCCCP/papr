// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import AIChatPanel from "./AIChatPanel";
import { useUi } from "../store";

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
});
