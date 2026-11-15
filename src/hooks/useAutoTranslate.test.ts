// @vitest-environment jsdom
import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useAutoTranslateOnOpen, type AutoTranslateHookInput } from "./useAutoTranslate";

// The review scenario: the user saved ja/google while the UI runs in zh —
// the loading fallbacks would start the job in the wrong language/engine.
const saved = { targetLang: "ja", engine: "google" };

const base: Omit<AutoTranslateHookInput, "onStart" | "onSurface"> = {
  requested: true,
  articleId: 1,
  openModeKnown: true,
  rulesFetching: false,
  configReady: true,
  extractionBlocking: false,
  canTranslate: true,
  hasCachedTranslation: false,
  ...saved,
};

describe("useAutoTranslateOnOpen", () => {
  it("starts no job while the settings load, then starts with the saved configuration when they resolve", () => {
    const onStart = vi.fn();
    const onSurface = vi.fn();
    // Article + rules already resolved; the translate settings are still in
    // flight, so targetLang/engine are the zh/llm fallbacks.
    const { rerender } = renderHook(
      (p: AutoTranslateHookInput) => useAutoTranslateOnOpen(p),
      {
        initialProps: {
          ...base,
          configReady: false,
          targetLang: "zh",
          engine: "llm",
          onStart,
          onSurface,
        },
      },
    );
    expect(onStart).not.toHaveBeenCalled();
    expect(onSurface).not.toHaveBeenCalled();
    // The settings resolve to the saved ja/google: the job starts with
    // exactly those arguments — never the fallbacks.
    rerender({
      ...base,
      configReady: true,
      targetLang: "ja",
      engine: "google",
      onStart,
      onSurface,
    });
    expect(onStart).toHaveBeenCalledTimes(1);
    expect(onStart).toHaveBeenCalledWith(1, "ja", "google");
    expect(onSurface).toHaveBeenCalledTimes(1);
    // Once-per-open: further renders neither re-start nor re-surface.
    rerender({
      ...base,
      configReady: true,
      targetLang: "ja",
      engine: "google",
      onStart,
      onSurface,
    });
    expect(onStart).toHaveBeenCalledTimes(1);
    expect(onSurface).toHaveBeenCalledTimes(1);
  });

  it("surfaces a cached translation without starting a job", () => {
    const onStart = vi.fn();
    const onSurface = vi.fn();
    renderHook((p: AutoTranslateHookInput) => useAutoTranslateOnOpen(p), {
      initialProps: { ...base, hasCachedTranslation: true, onStart, onSurface },
    });
    expect(onStart).not.toHaveBeenCalled();
    expect(onSurface).toHaveBeenCalledTimes(1);
  });

  it("re-attempts once per open of the same article", () => {
    const onStart = vi.fn();
    const onSurface = vi.fn();
    const props: AutoTranslateHookInput = { ...base, onStart, onSurface };
    const { rerender } = renderHook(
      (p: AutoTranslateHookInput) => useAutoTranslateOnOpen(p),
      { initialProps: props },
    );
    expect(onStart).toHaveBeenCalledTimes(1);
    // Switch to an unrelated article B.
    rerender({ ...props, articleId: 2, requested: false });
    expect(onStart).toHaveBeenCalledTimes(1);
    // Reopen A: the marker was reset with the view, so the rule asks again —
    // and without a cached translation, a new job starts (once).
    rerender({ ...props, articleId: 1, requested: true });
    expect(onStart).toHaveBeenCalledTimes(2);
    expect(onStart).toHaveBeenLastCalledWith(1, "ja", "google");
  });

  it("stays blocked while an extraction is in flight, starts when it settles", () => {
    const onStart = vi.fn();
    const onSurface = vi.fn();
    const { rerender } = renderHook(
      (p: AutoTranslateHookInput) => useAutoTranslateOnOpen(p),
      { initialProps: { ...base, extractionBlocking: true, onStart, onSurface } },
    );
    expect(onStart).not.toHaveBeenCalled();
    rerender({ ...base, extractionBlocking: false, onStart, onSurface });
    expect(onStart).toHaveBeenCalledTimes(1);
    expect(onStart).toHaveBeenCalledWith(1, "ja", "google");
  });
});
