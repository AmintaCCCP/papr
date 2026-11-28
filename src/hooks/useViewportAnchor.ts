import type { Virtualizer } from "@tanstack/react-virtual";
import { useCallback, useEffect, useRef, type RefObject } from "react";

/** The row under the viewport's top edge: its article id and its index in
 *  the *display* order (the same dedupe + importance ordering the list
 *  renders). Feeds the prepend scroll compensation in ArticleList, which
 *  measures how far this row shifted once a fetched page landed. */
export interface ViewportAnchor {
  id: number;
  index: number;
}

interface Options {
  /** The list's scroll container (`.list-scroll`). */
  scrollRef: RefObject<HTMLDivElement | null>;
  /** The list's virtualizer — the same instance the rows render through, so
   *  `getVirtualItemForOffset` resolves the exact row the user sees. */
  virtualizer: Virtualizer<HTMLDivElement, Element>;
  /** The display-order items the virtualizer indexes into. */
  items: readonly { id: number }[];
  /** True while a previous-page fetch is in flight. The scroll listener is
   *  installed synchronously by `beginTracking()` in the fetch-trigger tick;
   *  this flag settles it — when it flips back to false (or the component
   *  unmounts) the listener is removed. */
  tracking: boolean;
}

/** Capture and track the viewport's top-edge row for the previous-page scroll
 *  compensation.
 *
 *  A prepend shifts every loaded row down, so the list must nudge the scroll
 *  position by the anchor row's shift in the recomputed display order. The
 *  anchor is captured once when the fetch triggers — but the request takes
 *  time and the user can keep scrolling meanwhile, leaving the trigger-time
 *  capture pointing at a row far above the viewport by the time the page
 *  lands (the compensation would then hold the view at the wrong place).
 *  While the fetch is out, a scroll listener re-captures on every scroll
 *  event; when the page lands, the ref holds the anchor from the *last*
 *  scroll before the response — the row the user is actually looking at.
 *
 *  The listener's lifecycle starts synchronously: callers invoke
 *  `beginTracking()` in the same tick as the request, which installs the
 *  listener before any re-render. Waiting for the `tracking` prop's effect
 *  (the isFetchingPreviousPage flip React commits after paint) would leave a
 *  gap in which a scroll goes uncaptured — and if that was the user's last
 *  scroll before the page landed, the compensation would compute from the
 *  stale trigger-time anchor. The lifecycle effect only reconciles after the
 *  fact: re-asserting the install once React confirms the fetch is in flight,
 *  removing the listener when it settles, and detaching on unmount.
 *
 *  `captureAnchor` resolves with the same method the initial capture uses
 *  (`getVirtualItemForOffset(scrollTop)`), and writes only a *valid* anchor:
 *  an unresolvable viewport (no scroll element, no measurements yet) leaves
 *  the previous capture untouched, so a fast scroll that momentarily outruns
 *  the resolvable window can never blank out the anchor the compensation will
 *  need. Callers read `anchorRef` (compensation, list-context resets), call
 *  `captureAnchor()` to take the initial capture, and call `beginTracking()`
 *  in the same tick as the request. */
export function useViewportAnchor({
  scrollRef,
  virtualizer,
  items,
  tracking,
}: Options) {
  const anchorRef = useRef<ViewportAnchor | null>(null);

  const captureAnchor = useCallback((): ViewportAnchor | null => {
    const el = scrollRef.current;
    if (!el) return null;
    const top = virtualizer.getVirtualItemForOffset(el.scrollTop);
    const item = top ? items[top.index] : undefined;
    if (!item || !top) return null;
    const anchor = { id: item.id, index: top.index };
    anchorRef.current = anchor;
    return anchor;
  }, [scrollRef, virtualizer, items]);

  // The installed listener resolves rows through this ref, so a next-page
  // fetch landing mid-flight (swapping `items`, a next fetch of the same
  // list) is indexed against the freshest display order without having to
  // re-subscribe — the closure only ever reads the ref at event time.
  const captureLatestRef = useRef(captureAnchor);
  useEffect(() => {
    captureLatestRef.current = captureAnchor;
  });

  // Listener plumbing. `attach` is the single install point and is idempotent
  // via the stored listener, so the synchronous trigger-tick call
  // (beginTracking) and the lifecycle effect can both assert it without ever
  // stacking a second listener; `detach` is the single removal point.
  const scrollElRef = useRef<HTMLDivElement | null>(null);
  const onScrollRef = useRef<(() => void) | null>(null);
  const attach = useCallback(() => {
    if (onScrollRef.current) return;
    const el = scrollRef.current;
    if (!el) return;
    const onScroll = () => captureLatestRef.current();
    el.addEventListener("scroll", onScroll, { passive: true });
    scrollElRef.current = el;
    onScrollRef.current = onScroll;
  }, [scrollRef]);
  const detach = useCallback(() => {
    const el = scrollElRef.current;
    const onScroll = onScrollRef.current;
    if (el && onScroll) el.removeEventListener("scroll", onScroll);
    scrollElRef.current = null;
    onScrollRef.current = null;
  }, []);

  /** Begin in-flight anchor tracking synchronously, in the same tick the
   *  previous-page request goes out (see the lifecycle effect below for why
   *  the effect alone can't be the install point). Idempotent: while a
   *  listener is already installed — StrictMode's double effect run, a
   *  repeated call — this is a no-op. */
  const beginTracking = useCallback(() => {
    if (onScrollRef.current) return;
    attach();
  }, [attach]);

  // Lifecycle reconciliation. `beginTracking()` installed the listener
  // synchronously at the trigger tick; this effect (a) re-asserts the install
  // once React confirms the fetch is in flight — covering a trigger tick
  // whose scroll element wasn't resolvable yet — and (b) removes the listener
  // when the fetch settles (`tracking` back to false) or the component
  // unmounts. The cleanup runs on every `tracking` change and unmount, and
  // `attach` re-installs when `tracking` turns true, so the install/remove
  // stays symmetric no matter how the flips interleave with the synchronous
  // start (and StrictMode's double run can neither leak it nor strand it).
  useEffect(() => {
    if (tracking) attach();
    return detach;
  }, [tracking, attach, detach]);

  return { anchorRef, captureAnchor, beginTracking };
}
