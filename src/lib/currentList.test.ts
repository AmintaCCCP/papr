// @vitest-environment jsdom
// The store chain (currentList → store → i18n) needs a DOM-ish runtime, so
// this file opts into jsdom even though the functions under test are pure.
// i18n reads localStorage at import time, which this jsdom build does not
// ship — shim it before the imports run (same guard as AIChatPanel.test.tsx).
import { describe, expect, it, vi } from "vitest";

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

import { applyIdOrder, currentListKey } from "./currentList";
import type { ArticleQuery } from "../types";

const row = (id: number) => ({ id, title: `row ${id}` });

describe("applyIdOrder (importance ordering)", () => {
  it("puts scored ids first in the given order, the rest after in original order", () => {
    const items = [row(1), row(2), row(3), row(4)];
    // Scores ranked 3 best, then 1; 2 and 4 were never scored (model skipped
    // them, or they paged in after scoring).
    expect(applyIdOrder(items, [3, 1])).toEqual([row(3), row(1), row(2), row(4)]);
  });

  it("ignores stale ids that are no longer in the list, without dropping rows", () => {
    const items = [row(1), row(2)];
    // 9 was scored but has since left the window (read, filtered, refreshed
    // away) — the ordering must still render every loaded row exactly once.
    expect(applyIdOrder(items, [9, 2])).toEqual([row(2), row(1)]);
  });

  it("keeps the first occurrence of a duplicated id in the ordering", () => {
    const items = [row(1), row(2), row(3)];
    expect(applyIdOrder(items, [3, 3, 1])).toEqual([row(3), row(1), row(2)]);
  });

  it("returns the original order for an empty or all-unknown id list", () => {
    const items = [row(2), row(1)];
    expect(applyIdOrder(items, [])).toEqual([row(2), row(1)]);
    expect(applyIdOrder(items, [7, 8])).toEqual([row(2), row(1)]);
  });
});

describe("currentListKey (list-context signature)", () => {
  const feed: ArticleQuery = { kind: "feed", value: 5 };

  it("is stable for identical arguments", () => {
    expect(currentListKey(feed, true, false)).toBe(currentListKey(feed, true, false));
  });

  it("distinguishes query, unread filter and sort order", () => {
    const base = currentListKey(feed, true, false);
    expect(currentListKey({ kind: "feed", value: 6 }, true, false)).not.toBe(base);
    expect(currentListKey(feed, false, false)).not.toBe(base);
    expect(currentListKey(feed, true, true)).not.toBe(base);
  });
});
