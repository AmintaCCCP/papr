// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import Reader from "./Reader";
import { useUi } from "../store";

// The review scenario, executed against the real Reader component: the user
// saved ja/google while the UI runs in zh. The translate-settings query is
// held back until the article, feeds and rule actions have all settled, and
// the assertion is on the *production* call seam — the translation job store
// invoking api.aiTranslate — not on a test double.

// Runs before every import: jsdom's localStorage may be shadowed by node's
// (disabled) experimental global, and the i18n + prefs modules read it at
// module-init time.
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
  const article = {
    id: 1,
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
  };
  const feed = {
    id: 1,
    feedUrl: "https://example.com/feed.xml",
    siteUrl: null,
    title: "Example Feed",
    description: null,
    faviconUrl: null,
    folderId: null,
    sourceType: "rss" as const,
    lastFetchedAt: null,
    fetchError: null,
    unreadCount: 1,
    refreshIntervalMin: null,
    autoTranslate: false,
    openMode: null,
  };
  return {
    article,
    feed,
    aiTranslate: vi.fn(async () => {}),
    getRuleActions: vi.fn(async () => ["translate"]),
    // The gate the scenario hinges on: the saved target-language setting,
    // resolved manually after the rest of the page has settled.
    langGate: { resolve: undefined as undefined | ((lang: string) => void) },
  };
});

vi.mock("../api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api")>();
  return {
    ...actual,
    getArticle: vi.fn(async () => h.article),
    listFeeds: vi.fn(async () => [h.feed]),
    getSetting: vi.fn((key: string) => {
      if (key === "translate_target_lang") {
        return new Promise<string>((resolve) => {
          h.langGate.resolve = resolve;
        });
      }
      if (key === "translate_engine") return Promise.resolve("google");
      // `integrations` and everything else: empty.
      return Promise.resolve(null);
    }),
    getRuleActions: h.getRuleActions,
    listTags: vi.fn(async () => []),
    aiTranslate: h.aiTranslate,
    extractFulltext: vi.fn(),
  };
});

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn(async () => {}) }));

// jsdom gaps the reader's render path touches.
Element.prototype.scrollIntoView ??= () => {};
globalThis.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
} as unknown as typeof ResizeObserver;

describe("Reader auto-translate (delayed settings, real job store)", () => {
  beforeEach(() => {
    h.aiTranslate.mockClear();
    h.getRuleActions.mockClear();
    h.langGate.resolve = undefined;
    useUi.setState({ selectedArticleId: 1, aiOpen: false });
  });

  it("starts no job while the settings are pending, then calls aiTranslate with the saved ja/google", async () => {
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    render(
      <QueryClientProvider client={qc}>
        <Reader onToast={() => {}} />
      </QueryClientProvider>,
    );

    // Article, feeds and the rule action ("translate") have all resolved by
    // now; only the target-language setting is still in flight. Let every
    // immediate query and effect settle — nothing may start.
    await waitFor(() => expect(h.getRuleActions).toHaveBeenCalled());
    await act(async () => {
      await new Promise((r) => setTimeout(r, 25));
    });
    expect(h.aiTranslate).not.toHaveBeenCalled();

    // The settings resolve to the saved configuration: the production call
    // seam (job store → api.aiTranslate) fires exactly once with the saved
    // values — never the zh/llm fallbacks.
    await act(async () => {
      h.langGate.resolve!("ja");
    });
    await waitFor(() => expect(h.aiTranslate).toHaveBeenCalledTimes(1));
    expect(h.aiTranslate).toHaveBeenCalledWith(
      1,
      "ja",
      "google",
      expect.any(Function),
    );
  });

  it("surfaces no fallback-language job across settle windows while the settings stay pending", async () => {
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    render(
      <QueryClientProvider client={qc}>
        <Reader onToast={() => {}} />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(h.getRuleActions).toHaveBeenCalled());
    // Several settle windows pass — the pending setting keeps the gate shut.
    for (let i = 0; i < 3; i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 15));
      });
    }
    expect(h.aiTranslate).not.toHaveBeenCalled();
  });
});
