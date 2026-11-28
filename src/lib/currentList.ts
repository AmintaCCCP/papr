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

export function readCurrentItems(qc: QueryClient): ArticleSummary[] {
  const { query, unreadOnly, sortOldest, listAnchor, importanceOrder } =
    useUi.getState();
  const inf = qc.getQueryData([
    "articles",
    query,
    unreadOnly,
    sortOldest,
    listAnchor,
  ]) as { pages: ArticleSummary[][] } | undefined;
  const items = inf?.pages.flat() ?? [];
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
