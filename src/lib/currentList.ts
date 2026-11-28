// Read the article list currently shown in the middle pane straight from the
// React Query cache — used by keyboard shortcuts and reader navigation so they
// always agree with what the user sees.

import type { QueryClient } from "@tanstack/react-query";
import { useUi } from "../store";
import type { ArticleQuery, ArticleSummary } from "../types";

/** Canonical signature of a list context (view + filters, not the paging
 *  anchor). Everything that asks "is this still the same list the user was
 *  looking at?" — the importance ordering, the pending-refresh backlog —
 *  compares against this string. The anchor is deliberately excluded: paging
 *  or a locate-jump slides the window without changing which timeline it is. */
export function currentListKey(
  query: ArticleQuery,
  unreadOnly: boolean,
  sortOldest: boolean,
): string {
  return `${JSON.stringify(query)}|${unreadOnly}|${sortOldest}`;
}

/** Reorder `items` into the id sequence `ids` (the AI importance ordering).
 *  Articles present in `ids` come first, in that order; anything else keeps
 *  its original relative order behind them — rows paged beyond the scored
 *  window (or that arrived since) still show up exactly once. */
export function applyIdOrder<T extends { id: number }>(
  items: T[],
  ids: number[],
): T[] {
  const rank = new Map<number, number>();
  ids.forEach((id, i) => {
    if (!rank.has(id)) rank.set(id, i);
  });
  return items
    .map((item, i) => ({ item, key: rank.get(item.id) ?? ids.length + i }))
    .sort((a, b) => a.key - b.key)
    .map((entry) => entry.item);
}

/** Flatten the cached pages of the current list into rows, deduplicated by
 *  article id — first occurrence wins, order preserved. With offset paging a
 *  row can genuinely land on two pages: while a background refresh is parked
 *  (pendingListRefresh) the already-loaded pages stay cached, but a page
 *  fetched later is read straight from the live database (`LIMIT ? OFFSET ?`),
 *  and rows inserted before that page's offset shift it back onto cached
 *  territory, so the same article comes back on both pages. Duplicates would
 *  render twice and make J/K cross the same article twice, so every consumer
 *  that stitches pages (ArticleList's `items` memo, `readCurrentItems`) goes
 *  through here; the raw `data.pages` cache stays untouched. The mirror
 *  defect — rows skipped when the same shift pushes them before the offset —
 *  is inherent to OFFSET paging and predates the deferred refresh. */
export function dedupeById<T extends { id: number }>(items: T[]): T[] {
  const seen = new Set<number>();
  const out: T[] = [];
  for (const item of items) {
    if (seen.has(item.id)) continue;
    seen.add(item.id);
    out.push(item);
  }
  return out;
}

/** How many rows of a freshly fetched page are genuinely new to the list —
 *  ids absent from the `knownIds` snapshot of the rows already loaded. Under
 *  OFFSET paging the fetched page can overlap the cached pages (see
 *  dedupeById), so the view gains fewer rows than the page holds; this count,
 *  not the raw page length, is the prepend size the scroll compensation in
 *  ArticleList must nudge by to keep the viewport put. */
export function countNewRows(
  knownIds: readonly number[],
  page: readonly { id: number }[],
): number {
  const known = new Set(knownIds);
  const fresh = new Set<number>();
  for (const row of page) {
    if (!known.has(row.id)) fresh.add(row.id);
  }
  return fresh.size;
}

/** Scroll compensation, in rows, for a freshly prepended page: the shift of
 *  the row that was under the viewport's top edge. `anchorId` and
 *  `anchorIndexBefore` are that row's id and its index in the display order
 *  captured just before the fetch; `displayItems` is the recomputed display
 *  order after the page landed (the same dedupe + importance ordering the
 *  render uses). Under the default ordering every genuinely new row is
 *  prepended above the anchor, so the shift equals their count; with the
 *  importance ordering active the new unscored rows sort *behind* the
 *  already-scored ones, so a scored anchor doesn't move at all and a raw
 *  new-row count would over-scroll. When the anchor wasn't captured or no
 *  longer exists in the new order, fall back to `fallbackNewRows` (the
 *  overlap-aware `countNewRows` figure). */
export function scrollCompensation(
  anchorIndexBefore: number | null,
  anchorId: number | null,
  displayItems: readonly { id: number }[],
  fallbackNewRows: number,
): number {
  if (anchorId == null || anchorIndexBefore == null || anchorIndexBefore < 0) {
    return fallbackNewRows;
  }
  const after = displayItems.findIndex((row) => row.id === anchorId);
  if (after < 0) return fallbackNewRows;
  return after - anchorIndexBefore;
}

/** The rows ArticleList currently has loaded for the store's list context, in
 *  the backend's own order (no importance reorder applied). Reads the same
 *  cache slice ArticleList renders — same query key, same pages — so actions
 *  on "the visible window" (the reader's importance scorer) rate rows that
 *  actually intersect what the user sees, never a fixed first page the
 *  paging anchor has moved away from. */
export function loadedListRows(qc: QueryClient): ArticleSummary[] {
  const { query, unreadOnly, sortOldest, listAnchor } = useUi.getState();
  const inf = qc.getQueryData([
    "articles",
    query,
    unreadOnly,
    sortOldest,
    listAnchor,
  ]) as { pages: ArticleSummary[][] } | undefined;
  return dedupeById(inf?.pages.flat() ?? []);
}

export function readCurrentItems(qc: QueryClient): ArticleSummary[] {
  const { query, unreadOnly, sortOldest, importanceOrder } = useUi.getState();
  const items = loadedListRows(qc);
  // J/K must follow what the user sees: when the importance ordering is
  // active for this list, navigation walks the reordered window too.
  if (
    importanceOrder &&
    importanceOrder.key === currentListKey(query, unreadOnly, sortOldest)
  ) {
    return applyIdOrder(items, importanceOrder.ids);
  }
  return items;
}
