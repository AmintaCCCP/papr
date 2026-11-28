// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import ArticleList from "./ArticleList";
import { useUi } from "../store";
import {
  applyIdOrder,
  currentListKey,
  scrollCompensation,
} from "../lib/currentList";
import type { ArticleSummary } from "../types";

// scrollCompensation (via currentList → store) and the store itself read
// localStorage at import time, which this jsdom build does not ship — shim it
// before the imports run (same guard as useViewportAnchor.test.ts).
vi.hoisted(() => {
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
});

const h = vi.hoisted(() => ({
  getSetting: vi.fn(async () => null),
  listFeeds: vi.fn(async () => []),
  listArticles: vi.fn(),
  articleIndex: vi.fn(async () => null),
  markAllRead: vi.fn(async () => 0),
  setSetting: vi.fn(async () => {}),
}));

vi.mock("../api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api")>();
  return {
    ...actual,
    getSetting: h.getSetting,
    listFeeds: h.listFeeds,
    listArticles: h.listArticles,
    articleIndex: h.articleIndex,
    markAllRead: h.markAllRead,
    setSetting: h.setSetting,
  };
});

// Uniform estimates, mirroring how the component's own compensation math is
// estimate-based: the virtualizer never measures real layout in jsdom, so the
// rows it does render (which go through `virt.measureElement`) must measure
// back at exactly the estimate or the offset↔index arithmetic below breaks.
// jsdom reports offsetHeight 0 for everything, which would shrink every
// rendered row to 0px and collapse the measurements — stub it to the estimate.
const ROW = 98; // ArticleList's "cozy" density estimate
const PAGE = 60; // ArticleList's page size

// The list is anchored mid-feed: `listAnchor` = PAGE, so the first loaded page
// is rows 60–119 and a *previous* (newer) page — rows 0–59 — exists above it,
// which is the only configuration in which the component pages upward.
const makeArticle = (id: number): ArticleSummary => ({
  id,
  feedId: 1,
  feedTitle: "Feed",
  sourceType: "rss",
  title: `Article ${id}`,
  author: null,
  snippet: null,
  imageUrl: null,
  url: null,
  publishedAt: null,
  isRead: false,
  isStarred: false,
  readLater: false,
});
// OFFSET paging: the page at offset o holds ids o+1 … o+PAGE.
const pageFor = (offset: number) =>
  Array.from({ length: PAGE }, (_, k) => makeArticle(offset + 1 + k));
// The full display order once the newer page lands (default ordering — every
// new row prepends above the anchor; no importance reorder active).
const landed = [...pageFor(0), ...pageFor(PAGE)];

// The AI importance ordering preset for scenario 2, and the two display
// orders it produces — derived through the same `applyIdOrder` the component
// runs, so the expectations below come from the same source as the render.
// Scored ids pin to the top in this order; unscored rows keep their natural
// order behind them. 3/7/11 live on the incoming page; 61 and 65 are scored
// *from the already-loaded page* on purpose — the stale trigger-time anchor
// is row 0 (id 61), the re-captured one is id 71, and only a pinned row
// sitting between the two in the pre-page order (65) makes their shifts
// diverge: pinning only new-page ids would move both anchors down by the
// same 60 rows again (they are both unscored rows of the contiguous 61…120
// tail), and the scenario could not tell the captures apart. 61 is pinned
// first so display row 0 stays id 61 and the trigger-time capture matches
// the default-ordering scenario.
const SCORED = [3, 7, 11, 61, 65];
const landedImportance = applyIdOrder(landed, SCORED);
const prePageImportance = applyIdOrder(pageFor(PAGE), SCORED);

const rowId = (container: HTMLElement, index: number) =>
  container.querySelector(`[data-index="${index}"] .art`)?.id;

let resolvePrevPage: ((page: ArticleSummary[]) => void) | null = null;

const renderList = () => {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={qc}>
      <ArticleList onToast={() => {}} />
    </QueryClientProvider>,
  );
};

const scrollEl = (el: HTMLDivElement, scrollTop: number) => {
  act(() => {
    el.scrollTop = scrollTop;
    el.dispatchEvent(new Event("scroll"));
  });
};

// jsdom defines `offsetHeight` on HTMLElement.prototype itself, so the
// override below must be swapped back between tests.
let realOffsetHeight: PropertyDescriptor | undefined;

beforeEach(() => {
  realOffsetHeight = Object.getOwnPropertyDescriptor(
    window.HTMLElement.prototype,
    "offsetHeight",
  );
  Object.defineProperty(window.HTMLElement.prototype, "offsetHeight", {
    configurable: true,
    get: () => ROW,
  });

  resolvePrevPage = null;
  h.getSetting.mockReset().mockResolvedValue(null);
  h.listFeeds.mockReset().mockResolvedValue([]);
  h.listArticles.mockReset();
  // The first (anchored) page resolves immediately; the previous-page fetch
  // hangs on a gate the test controls — that is the "response in flight"
  // window the in-flight anchor tracking must cover.
  h.listArticles.mockImplementation(
    (_q, _u, _s, _o, _limit, offset: number) => {
      if (offset === 0) {
        return new Promise<ArticleSummary[]>((res) => {
          resolvePrevPage = res;
        });
      }
      return Promise.resolve(pageFor(offset));
    },
  );
  h.articleIndex.mockReset().mockResolvedValue(null);
  h.markAllRead.mockReset().mockResolvedValue(0);
  h.setSetting.mockReset().mockResolvedValue(undefined);

  useUi.setState({
    query: { kind: "all" },
    unreadOnly: false,
    sortOldest: false,
    listAnchor: PAGE,
    selectedArticleId: null,
    importanceOrder: null,
    viewMode: "list",
    density: "cozy",
    pendingListRefresh: false,
  });
});

afterEach(() => {
  if (realOffsetHeight) {
    Object.defineProperty(
      window.HTMLElement.prototype,
      "offsetHeight",
      realOffsetHeight,
    );
  }
  cleanup();
});

// Mount, let the anchored page render, and wait for the upward paging trigger
// (the top of the window is at the viewport edge, so ArticleList fetches the
// previous page by itself — the component's own trigger path).
const mountAndAwaitTrigger = async () => {
  const { container } = renderList();
  const el = container.querySelector(".list-scroll") as HTMLDivElement;
  await waitFor(() => {
    expect(h.listArticles).toHaveBeenCalledWith(
      { kind: "all" },
      false,
      null,
      false,
      PAGE,
      0,
    );
  });
  return { container, el };
};

describe("ArticleList (upward pagination scroll compensation)", () => {
  // CodeRabbit PRRT_kwDOVkHyXc69C8nv: the existing tests covered the anchor
  // helpers but never drove ArticleList's own pagination and asserted the
  // post-page scrollTop. Default ordering: the compensation must be the count
  // of genuinely new rows (the anchor row shifts by exactly that).
  it("keeps the viewport on the anchor row when the previous page lands", async () => {
    const { el } = await mountAndAwaitTrigger();

    // Trigger-time anchor: viewport at the top edge → row 0 of the anchored
    // page (id 61). Captured by capturePrevAnchor() in the trigger effect.
    const anchorId = pageFor(PAGE)[0].id;
    const anchorAfter = landed.findIndex((a) => a.id === anchorId);
    const addedRows = scrollCompensation(
      0,
      anchorId,
      landed,
      PAGE, // overlap-aware fallback — all PAGE rows are genuinely new here
    );
    expect(addedRows).toBe(anchorAfter); // 60 rows prepended above the anchor

    await act(async () => {
      resolvePrevPage!(pageFor(0));
    });

    // The prepend must have been compensated: the anchor row stays under the
    // viewport's top edge at its new index (estimate-based, so exact here).
    await waitFor(() => {
      expect(el.scrollTop).toBe(addedRows * ROW);
    });
    expect(el.scrollTop).toBe(anchorAfter * ROW);
  });

  // Scenario 2: while the response is in flight the user keeps scrolling; the
  // compensation must follow the LAST row they looked at (useViewportAnchor's
  // re-capture), not the stale trigger-time capture. Unlike scenario 1 this
  // runs under the AI importance ordering, because that is where the two
  // captures can be told apart: under the default ordering both the trigger-
  // time anchor (row 0) and the re-captured one shift down by exactly the
  // count of genuinely new rows, so a stale capture would land on the same
  // scrollTop and no assertion could say which anchor drove the compensation
  // (CodeRabbit follow-up on the original default-ordering version).
  it("compensates from the in-flight scroll position, not the trigger-time capture", async () => {
    // Importance ordering for this exact list context — `key` must equal
    // currentListKey(query, unreadOnly, sortOldest) or ArticleList silently
    // falls back to the default order (see the items memo). Scored ids pin to
    // the top in this order; unscored rows keep their natural order behind
    // them (see SCORED above for why 61 and 65 must be among them).
    useUi.setState({
      importanceOrder: {
        key: currentListKey({ kind: "all" }, false, false),
        ids: SCORED,
      },
    });

    const { container, el } = await mountAndAwaitTrigger();

    // The ordering is active before the page lands: the scored rows among the
    // loaded page pin to the top (61, then 65), the rest keep natural order.
    expect(rowId(container, 0)).toBe("option-article-61");
    expect(rowId(container, 1)).toBe("option-article-65");
    expect(rowId(container, 2)).toBe("option-article-62");

    // The user scrolls to display row 10 (id 71) while the page is still in
    // flight; the scroll listener beginPrevTracking() installed synchronously
    // re-captures the viewport anchor to that row.
    const viewportRow = 10;
    const anchorId = prePageImportance[viewportRow].id;
    expect(anchorId).toBe(71);
    const anchorAfter = landedImportance.findIndex((a) => a.id === anchorId);
    scrollEl(el, viewportRow * ROW);

    await act(async () => {
      resolvePrevPage!(pageFor(0));
    });

    // Same formula the component runs: scrollTop += scrollCompensation(
    // anchor.index, anchor.id, landedItems, fallback) × rowEstimate, with the
    // anchor captured in the pre-page display order. With the pinned rows
    // above everything and 65 pulled out from between 61 and 71, id 71 lands
    // at index 70 — a 60-row shift from its pre-page index 10.
    const added = scrollCompensation(
      viewportRow,
      anchorId,
      landedImportance,
      PAGE, // overlap-aware fallback — all PAGE rows are genuinely new here
    );
    const expected = viewportRow * ROW + added * ROW;
    await waitFor(() => {
      expect(el.scrollTop).toBe(expected);
    });
    // The user's row did not move visually: viewport top == its new offset.
    expect(el.scrollTop).toBe(anchorAfter * ROW);

    // Discrimination — what the default ordering could not provide: the stale
    // trigger-time anchor (row 0, id 61) computes a *different* compensation,
    // because 61 pins to landed index 3 while 71 lands at index 70. Had the
    // component used the stale capture, scrollTop would have stopped at 1274
    // and id 71 would sit far below the viewport top — the assertions above
    // would fail.
    const staleAdded = scrollCompensation(
      0,
      prePageImportance[0].id,
      landedImportance,
      PAGE,
    );
    expect(staleAdded).not.toBe(added);
    expect(viewportRow * ROW + staleAdded * ROW).not.toBe(expected);
  });
});
