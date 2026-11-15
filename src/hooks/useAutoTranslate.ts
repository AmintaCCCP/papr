import { useEffect, useRef } from "react";
import { decideAutoTranslate } from "../lib/autoTranslate";

export interface AutoTranslateHookInput {
  /** The feed's auto-translate flag or a matching rule's `translate` action. */
  requested: boolean;
  /** The article to translate; null = nothing open. */
  articleId: number | null;
  /** The effective open mode is known — while unknown, a full-text
   *  extraction may still be about to run, and the body could be a stub. */
  openModeKnown: boolean;
  /** This open's rule-action fetch is still in flight. */
  rulesFetching: boolean;
  /** Both translate-settings queries have succeeded; `targetLang` / `engine`
   *  are the user's saved configuration, not fallbacks. */
  configReady: boolean;
  /** A full-text extraction is planned-but-unsettled or in flight. */
  extractionBlocking: boolean;
  /** There is a body to translate. */
  canTranslate: boolean;
  /** A cached translation exists and is valid for the target language. */
  hasCachedTranslation: boolean;
  /** The configured target language (only meaningful when `configReady`). */
  targetLang: string;
  /** The configured engine (only meaningful when `configReady`). */
  engine: string;
  /** Start a background translation job — the reader passes the jobs store's
   *  `translate`, the same entry the toolbar toggle uses. */
  onStart: (articleId: number, lang: string, engine: string) => void;
  /** Surface the translation panel (cache shown or job started). */
  onSurface: () => void;
}

/** The reader's auto-translate-on-open behaviour as a hook, so the effect
 *  itself — not just its decision — is executable in tests
 *  (useAutoTranslate.test.ts). The gating order lives in
 *  [`decideAutoTranslate`]; this hook consumes its decision, owns the
 *  once-per-open marker and drives the caller's callbacks.
 *
 *  The marker follows the view's lifecycle: it resets on article change, so
 *  every open of an article gets exactly one attempt. Re-spending is
 *  prevented where it matters by the caller's `hasCachedTranslation` input —
 *  a cached translation in the target language surfaces without a new job. */
export function useAutoTranslateOnOpen(i: AutoTranslateHookInput): void {
  const attemptedRef = useRef<number | null>(null);
  // Reset the marker with the view: the reader hides the translation panel on
  // article change, so the guard must not survive it. Declared before the
  // decision effect below so it wins the same commit on a switch.
  useEffect(() => {
    attemptedRef.current = null;
  }, [i.articleId]);
  useEffect(() => {
    const decision = decideAutoTranslate({
      requested: i.requested,
      articleId: i.articleId,
      openModeKnown: i.openModeKnown,
      rulesFetching: i.rulesFetching,
      configReady: i.configReady,
      extractionBlocking: i.extractionBlocking,
      canTranslate: i.canTranslate,
      alreadyMarked: !!(
        i.articleId != null && attemptedRef.current === i.articleId
      ),
      hasCachedTranslation: i.hasCachedTranslation,
      targetLang: i.targetLang,
      engine: i.engine,
    });
    if (!decision.proceed || i.articleId == null) return;
    attemptedRef.current = i.articleId;
    // A fresh cached translation for the target language needs no new job;
    // just surface it. Otherwise start a background translation.
    if (decision.start) i.onStart(i.articleId, decision.lang, decision.engine);
    i.onSurface();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    i.requested,
    i.articleId,
    i.openModeKnown,
    i.rulesFetching,
    i.configReady,
    i.extractionBlocking,
    i.canTranslate,
    i.hasCachedTranslation,
    i.targetLang,
    i.engine,
  ]);
}
