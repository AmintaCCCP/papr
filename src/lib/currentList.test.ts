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

import {
  applyIdOrder,
  countNewRows,
  currentListKey,
  dedupeById,
  scrollCompensation,
} from "./currentList";
import type { ArticleQuery } from "../types";

const row = (id: number) => ({ id, title: `row ${id}` });

describe("dedupeById (stitched offset pages)", () => {
  it("keeps the first occurrence of each id, preserving order", () => {
    // A page fetched while a background refresh is parked can overlap the
    // stale cached pages (offset shifted by rows inserted before it).
    const pages = [row(1), row(2), row(3), row(2), row(4), row(1)];
    expect(dedupeById(pages)).toEqual([row(1), row(2), row(3), row(4)]);
  });

  it("returns rows unchanged when there are no duplicates", () => {
    const pages = [row(5), row(2)];
    expect(dedupeById(pages)).toEqual([row(5), row(2)]);
    expect(dedupeById([])).toEqual([]);
  });
});

describe("countNewRows (prepend scroll compensation)", () => {
  it("counts only ids absent from the loaded pages when the page partially overlaps them", () => {
    // Rows inserted before the fetched page's offset shifted it back onto
    // cached territory: its tail (100..119) duplicates the cached page, its
    // head (60..99) is genuinely new — the list gains 40 rows, not 60.
    const known = Array.from({ length: 60 }, (_, i) => 100 + i);
    const page = [
      ...Array.from({ length: 40 }, (_, i) => ({ id: 60 + i })),
      ...Array.from({ length: 20 }, (_, i) => ({ id: 100 + i })),
    ];
    expect(countNewRows(known, page)).toBe(40);
  });

  it("returns 0 when the whole page is already loaded (all duplicates)", () => {
    const known = [1, 2, 3];
    expect(countNewRows(known, [row(3), row(1), row(2)])).toBe(0);
    expect(countNewRows([7], [row(7)])).toBe(0);
  });

  it("counts the full page when nothing overlaps", () => {
    const known = [8, 9];
    expect(countNewRows(known, [row(1), row(2), row(3)])).toBe(3);
    expect(countNewRows([], [row(1), row(2)])).toBe(2);
    expect(countNewRows(known, [])).toBe(0);
  });

  it("counts a row duplicated within the page itself only once", () => {
    expect(countNewRows([1], [row(2), row(3), row(2)])).toBe(2);
  });
});

describe("scrollCompensation (anchor-based prepend compensation)", () => {
  it("equals the genuinely-new row count under the default ordering (prepend sits wholly above the anchor)", () => {
    // Default ordering: the fetched page holds newer rows, so the deduped
    // flat list grows at the very top and every existing row — including the
    // viewport-top anchor — shifts down by exactly the number of new rows.
    const known = [10, 11, 12];
    const page = [row(1), row(2), row(3)];
    const after = dedupeById([...page, ...known.map(row)]);
    const fallback = countNewRows(known, page);
    expect(fallback).toBe(3);
    expect(scrollCompensation(0, 10, after, fallback)).toBe(3);
  });

  it("returns 0 when the importance ordering keeps the scored anchor in place", () => {
    // Display [A,B,C] with A,B scored, C not; scrolling up loads unscored
    // [X,Y]. The recomputed display order is [A,B,X,Y,C] — the anchor A never
    // moved, so compensating by the new-row count (2) would jump the viewport
    // by two rows. An unscored anchor below the insertion point does shift by
    // the full count.
    const after = applyIdOrder(
      [row(201), row(202), row(101), row(102), row(103)],
      [101, 102],
    );
    expect(after.map((r) => r.id)).toEqual([101, 102, 201, 202, 103]);
    expect(scrollCompensation(0, 101, after, 2)).toBe(0);
    expect(scrollCompensation(2, 103, after, 2)).toBe(2);
  });

  it("falls back to the given new-row count when the anchor is missing or unknown", () => {
    const after = [row(1), row(2), row(3)];
    expect(scrollCompensation(null, null, after, 5)).toBe(5);
    expect(scrollCompensation(0, 999, after, 4)).toBe(4);
  });

  it("compensates by the anchor's real shift when it genuinely moves in the importance order", () => {
    // Anchor 3 is unscored and sits right behind the scored block [1,2]; the
    // fetched page brings unscored rows 4,5 that sort between the block and
    // the anchor → [1,2,4,5,3], a two-row shift for the anchor.
    const after = applyIdOrder(
      [row(4), row(5), row(1), row(2), row(3)],
      [1, 2],
    );
    expect(after.map((r) => r.id)).toEqual([1, 2, 4, 5, 3]);
    expect(scrollCompensation(2, 3, after, 2)).toBe(2);
  });
});

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
