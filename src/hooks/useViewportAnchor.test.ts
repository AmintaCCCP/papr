// @vitest-environment jsdom
import { StrictMode, createElement, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { act, cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { scrollCompensation } from "../lib/currentList";
import { useViewportAnchor, type ViewportAnchor } from "./useViewportAnchor";

// scrollCompensation pulls the store chain (currentList → store → i18n), and
// i18n reads localStorage at import time, which this jsdom build does not
// ship — shim it before the imports run (same guard as currentList.test.ts).
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

// Uniform 100px rows, so an offset maps straight to a row index: scrollTop
// 0 → index 0, 200 → index 2. The virtualizer never measures real layout in
// jsdom, but getVirtualItemForOffset works off the count × estimateSize
// measurements, which is exactly the estimate-based resolution the list
// itself performs before rows are measured.
const ROW = 100;
const row = (id: number) => ({ id, title: `row ${id}` });

// The harness mounts a real scroll container + real useVirtualizer and
// exposes the hook's returns for assertions; `mount`'s rerender drives new
// props (ids / tracking) through component state.
interface Props {
  ids: number[];
  tracking: boolean;
}
interface Exposed {
  scrollRef: { current: HTMLDivElement | null };
  anchorRef: { current: ViewportAnchor | null };
  captureAnchor: () => ViewportAnchor | null;
  beginTracking: () => void;
}
const exposed: { current: Exposed | null } = { current: null };

function Harness({ ids, tracking }: Props) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const items = useMemo(() => ids.map(row), [ids]);
  const virt = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW,
    overscan: 5,
  });
  const { anchorRef, captureAnchor, beginTracking } = useViewportAnchor({
    scrollRef,
    virtualizer: virt,
    items,
    tracking,
  });
  exposed.current = { scrollRef, anchorRef, captureAnchor, beginTracking };
  return createElement("div", { ref: scrollRef, style: { height: 600 } });
}

const initial = { ids: [101, 102, 103], tracking: false };

function mount(props: Props = initial, strict = false) {
  let update: ((p: Props) => void) | null = null;
  const Root = () => {
    const [p, setP] = useState(props);
    update = setP;
    const harness = createElement(Harness, p);
    return strict ? createElement(StrictMode, null, harness) : harness;
  };
  render(createElement(Root));
  return {
    rerender: (next: Props) => {
      act(() => update!(next));
    },
  };
}

// The response that lands for the CodeRabbit scenario: X(201), Y(202) are new
// unscored rows that sort behind the scored block [101, 102] but ahead of the
// unscored 103 in the importance order.
const landed = [row(101), row(102), row(201), row(202), row(103)];

function scrollTo(el: HTMLDivElement, scrollTop: number) {
  act(() => {
    el.scrollTop = scrollTop;
    el.dispatchEvent(new Event("scroll"));
  });
}

afterEach(() => cleanup());

describe("useViewportAnchor (in-flight previous-page anchor tracking)", () => {
  it("recaptures on scroll while the fetch is in flight, so the compensation follows the row the user landed on", () => {
    const { rerender } = mount();
    const h = exposed.current!;
    const el = h.scrollRef.current!;

    // Trigger time: the fetch fires with the viewport at A(101) — scored, so
    // under the importance ordering the old code expected it not to move.
    el.scrollTop = 0;
    expect(h.captureAnchor()).toEqual({ id: 101, index: 0 });

    // Request in flight; the user keeps scrolling and the viewport's top edge
    // ends on C(103) — unscored. The response has not landed yet, but the
    // tracker must already hold C, not the stale trigger-time anchor.
    rerender({ ...initial, tracking: true });
    scrollTo(el, 200);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });

    // Response lands (tracking off). A straggler scroll must not rewrite the
    // capture: the listener is gone, so the compensation still sees C.
    rerender({ ...initial, tracking: false });
    scrollTo(el, 0);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });

    // The landed page prepends X, Y which sort ahead of C: C shifts two rows
    // down, so compensating by C's shift keeps it under the viewport edge.
    // (The stale trigger-time anchor would have computed 0 and let C jump.)
    expect(scrollCompensation(2, 103, landed, 2)).toBe(2);
    expect(scrollCompensation(0, 101, landed, 2)).toBe(0);
  });

  it("keeps the last valid capture when a scroll cannot resolve a row", () => {
    const { rerender } = mount({ ...initial, tracking: true });
    const h = exposed.current!;
    const el = h.scrollRef.current!;
    el.scrollTop = 0;
    expect(h.captureAnchor()).toEqual({ id: 101, index: 0 });

    // The resolvable window goes empty mid-flight (e.g. the virtualizer
    // momentarily has no measurements under a fast scroll): the scroll fires
    // but nothing valid is resolved, and the previous capture must survive.
    rerender({ ids: [], tracking: true });
    scrollTo(el, 150);
    expect(h.anchorRef.current).toEqual({ id: 101, index: 0 });

    // Scrolling back into a resolvable window overwrites again.
    rerender({ ...initial, tracking: true });
    scrollTo(el, 100);
    expect(h.anchorRef.current).toEqual({ id: 102, index: 1 });
  });

  it("removes the scroll listener when the fetch settles (symmetric cleanup, unmount-safe)", () => {
    mount({ ...initial, tracking: true });
    const h = exposed.current!;
    const el = h.scrollRef.current!;
    scrollTo(el, 200);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });
    cleanup();
    // A leak here would re-capture against scrollTop 300 and rewrite the ref.
    expect(() => scrollTo(el, 300)).not.toThrow();
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });
  });

  it("behaves the same under StrictMode's double effect run", () => {
    const { rerender } = mount(initial, true);
    const h = exposed.current!;
    const el = h.scrollRef.current!;
    el.scrollTop = 0;
    expect(h.captureAnchor()).toEqual({ id: 101, index: 0 });
    rerender({ ...initial, tracking: true });
    scrollTo(el, 200);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });
    rerender({ ...initial, tracking: false });
    scrollTo(el, 0);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });
  });

  // The CodeRabbit timing gap: the trigger captures the anchor and fires the
  // request synchronously, but the scroll listener used to install only in
  // the tracking prop's effect — which React commits after the fetch's
  // re-render, i.e. after paint. A scroll landing in between went uncaptured,
  // and if it was the user's last one before the page landed, the
  // compensation computed from the stale trigger-time anchor. beginTracking()
  // must cover the gap by installing in the same synchronous tick.
  it("captures a scroll that lands between the synchronous trigger and the tracking effect", () => {
    const { rerender } = mount();
    const h = exposed.current!;
    const el = h.scrollRef.current!;

    // The trigger tick, exactly as ArticleList performs it: initial capture,
    // then the synchronous tracking start, then the request — all in one
    // block, before React has re-rendered or any effect has run.
    el.scrollTop = 0;
    expect(h.captureAnchor()).toEqual({ id: 101, index: 0 });
    h.beginTracking();
    h.beginTracking(); // idempotent — must not stack a second listener
    // The scroll lands in the gap (React has not re-rendered yet): the
    // synchronous install must already re-capture to where the user went.
    scrollTo(el, 200);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });

    // Now the in-flight render commits (the effect re-asserts the install),
    // and the user never scrolls again. The anchor must still be 103 — not
    // the stale trigger-time 101.
    rerender({ ...initial, tracking: true });
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });

    // Settle. The listener is gone, and a straggler scroll must not rewrite
    // the capture — which also proves the double beginTracking call never
    // stacked a second listener (a stacked one would leak past the single
    // detach and re-capture here).
    rerender({ ...initial, tracking: false });
    scrollTo(el, 50);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });

    // The compensation for the landed page follows 103's shift: two new rows
    // (201, 202) sort ahead of it under the importance ordering, so the view
    // nudges by 2. Computed from the stale trigger-time anchor (101, index 0)
    // it would be 0 and the view would jump.
    expect(scrollCompensation(2, 103, landed, 2)).toBe(2);
    expect(scrollCompensation(0, 101, landed, 2)).toBe(0);
  });

  // Same timing path under StrictMode: the double effect run around the
  // tracking flips must neither strand the synchronous install nor leak a
  // duplicate listener past the settle.
  it("keeps the synchronous trigger path working under StrictMode", () => {
    const { rerender } = mount(initial, true);
    const h = exposed.current!;
    const el = h.scrollRef.current!;
    el.scrollTop = 0;
    expect(h.captureAnchor()).toEqual({ id: 101, index: 0 });
    h.beginTracking();
    scrollTo(el, 200);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });
    rerender({ ...initial, tracking: true });
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });
    rerender({ ...initial, tracking: false });
    scrollTo(el, 0);
    expect(h.anchorRef.current).toEqual({ id: 103, index: 2 });
  });
});
