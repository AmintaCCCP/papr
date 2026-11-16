// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import AIChatPanel from "./AIChatPanel";
import { useUi } from "../store";

// The chat sidebar against the production call seam: the panel invoking
// api.aiArticleChat with the conversation-so-far, deltas landing in the
// assistant bubble, and a failed send offering a retry that re-asks the same
// question with the prior turns as history.

const h = vi.hoisted(() => ({
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
  aiArticleChat: vi.fn(),
}));

vi.mock("../api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api")>();
  return {
    ...actual,
    aiArticleChat: h.aiArticleChat,
  };
});

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn(async () => {}) }));

const renderPanel = (open = true) => {
  const utils = render(<AIChatPanel article={h.article} open={open} onClose={() => {}} />);
  return utils;
};

async function ask(question: string, container: HTMLElement) {
  const box = container.querySelector("textarea") as HTMLTextAreaElement;
  fireEvent.change(box, { target: { value: question } });
  fireEvent.keyDown(box, { key: "Enter", shiftKey: false });
  await waitFor(() => expect(h.aiArticleChat).toHaveBeenCalled());
  // The call the send produced: (articleId, history, question, onToken).
  return h.aiArticleChat.mock.calls[h.aiArticleChat.mock.calls.length - 1];
}

describe("AIChatPanel", () => {
  beforeEach(() => {
    h.aiArticleChat.mockReset();
    useUi.setState({ aiChatWidth: 360 });
  });
  afterEach(() => cleanup());

  it("sends the question and streams the answer into the assistant bubble", async () => {
    h.aiArticleChat.mockImplementation(
      (_id: number, _history: unknown, _q: string, onToken: (e: { type: string; data?: string }) => void) => {
        onToken({ type: "delta", data: "Hel" });
        onToken({ type: "delta", data: "lo there" });
        onToken({ type: "done" });
        return Promise.resolve();
      },
    );
    const { container } = renderPanel();
    const [, history, question, onToken] = await ask("这篇文章讲了什么？", container);
    expect(history).toEqual([]);
    expect(question).toBe("这篇文章讲了什么？");
    expect(h.article.id).toBe(7);
    // Deltas land in the (markdown-rendered) assistant bubble; the composer
    // is cleared for the next question.
    await waitFor(() => expect(container.textContent).toContain("Hello there"));
    const box = container.querySelector("textarea") as HTMLTextAreaElement;
    expect(box.value).toBe("");
    // The channel promise resolved — busy cleared, the done event was not an
    // error.
    expect(onToken).toBeDefined();
    expect(container.textContent).not.toContain("失败");
  });

  it("keeps the user's question and offers a retry when the stream errors", async () => {
    // First exchange answers fine, so the retry after the failed second
    // question proves prior turns ride along as history.
    h.aiArticleChat.mockImplementation(
      (_id: number, _history: unknown, _q: string, onToken: (e: { type: string; data?: string }) => void) => {
        onToken({ type: "delta", data: "first answer" });
        onToken({ type: "done" });
        return Promise.resolve();
      },
    );
    const { container } = renderPanel();
    await ask("第一问", container);
    await waitFor(() => expect(container.textContent).toContain("first answer"));

    h.aiArticleChat.mockImplementation(
      (_id: number, _history: unknown, _q: string, onToken: (e: { type: string; data?: string }) => void) => {
        onToken({ type: "error", data: "provider down" });
        return Promise.resolve();
      },
    );
    await ask("第二问", container);
    // The question stays on the record and the failure banner appears (the
    // banner is localized — assert the element, not the copy).
    await waitFor(() => expect(container.textContent).toContain("第二问"));
    expect(container.textContent).not.toContain("provider down");
    expect(container.querySelector(".ai-chat-failed")).not.toBeNull();
    const retry = container.querySelector(".ai-chat-failed .empty-retry") as HTMLButtonElement;
    expect(retry).not.toBeNull();

    // Retry re-asks the same question; the prior exchange is now history.
    h.aiArticleChat.mockImplementation(
      (_id: number, _history: unknown, _q: string, onToken: (e: { type: string; data?: string }) => void) => {
        onToken({ type: "delta", data: "现在好了" });
        onToken({ type: "done" });
        return Promise.resolve();
      },
    );
    await act(async () => {
      fireEvent.click(retry);
    });
    await waitFor(() => expect(h.aiArticleChat).toHaveBeenCalledTimes(3));
    const [, history, question] = h.aiArticleChat.mock.calls[2];
    expect(question).toBe("第二问");
    expect(history).toEqual([
      { role: "user", content: "第一问" },
      { role: "assistant", content: "first answer" },
    ]);
    await waitFor(() => expect(container.textContent).toContain("现在好了"));
  });

  it("clears a partial reply before retrying, so the new answer stands alone", async () => {
    // First request streams a fragment, then dies mid-answer. The retry must
    // NOT append the new stream to the fragment — that would splice two
    // answers together, and the next question would send the concatenation
    // as history.
    h.aiArticleChat.mockImplementation(
      (_id: number, _history: unknown, _q: string, onToken: (e: { type: string; data?: string }) => void) => {
        onToken({ type: "delta", data: "旧片段" });
        onToken({ type: "error", data: "stream died" });
        return Promise.resolve();
      },
    );
    const { container } = renderPanel();
    await ask("这个概念怎么理解？", container);
    await waitFor(() => expect(container.textContent).toContain("旧片段"));
    expect(container.querySelector(".ai-chat-failed")).not.toBeNull();

    h.aiArticleChat.mockImplementation(
      (_id: number, _history: unknown, _q: string, onToken: (e: { type: string; data?: string }) => void) => {
        onToken({ type: "delta", data: "完整新回答" });
        onToken({ type: "done" });
        return Promise.resolve();
      },
    );
    const retry = container.querySelector(".ai-chat-failed .empty-retry") as HTMLButtonElement;
    await act(async () => {
      fireEvent.click(retry);
    });
    await waitFor(() => expect(h.aiArticleChat).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(container.textContent).toContain("完整新回答"));
    // The fragment is gone: the bubble shows ONLY the new answer.
    expect(container.textContent).not.toContain("旧片段");
    // And the next question must not send the concatenation as history.
    h.aiArticleChat.mockImplementation(() => Promise.resolve());
    await ask("下一问", container);
    const [, history] = h.aiArticleChat.mock.calls[h.aiArticleChat.mock.calls.length - 1];
    expect(history).toEqual([
      { role: "user", content: "这个概念怎么理解？" },
      { role: "assistant", content: "完整新回答" },
    ]);
  });

  it("parks the conversation off-screen while closed, inert", () => {
    const { container } = renderPanel(false);
    const pane = container.querySelector(".ai-chat") as HTMLElement;
    expect(pane.className).not.toContain("open");
    expect(pane.getAttribute("inert")).not.toBeNull();
    const opened = render(<AIChatPanel article={h.article} open={true} onClose={() => {}} />);
    const openPane = opened.container.querySelector(".ai-chat") as HTMLElement;
    expect(openPane.className).toContain("open");
  });
});
