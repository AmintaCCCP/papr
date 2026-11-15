/** The reader's auto-translate decision, extracted as a pure function so the
 *  gating order — and above all *which* language/engine a job would start
 *  with — is unit-testable. The translate settings resolve asynchronously:
 *  while they are in flight, `targetLang` / `engine` in Reader.tsx are only
 *  the UI-language / "llm" fallbacks, and a job started with those would pin
 *  the wrong configuration via the once-per-open marker (the translation
 *  cache is keyed by language, not content). See the auto-translate effect in
 *  Reader.tsx, which consumes this decision and owns the marker.
 *
 *  `proceed` means every precondition holds — the caller may consume its
 *  once-per-open marker. `start` (only ever true when `proceed`) means a new
 *  job is needed; otherwise a cached translation in the target language
 *  should be surfaced without one. `lang` / `engine` are the job arguments:
 *  the *configured* values, never the loading fallbacks, because
 *  `configReady` gates the whole decision. */
export interface AutoTranslateInput {
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
  /** The caller's once-per-open marker is already consumed for this article. */
  alreadyMarked: boolean;
  /** A cached translation exists and is valid for the target language. */
  hasCachedTranslation: boolean;
  /** The configured target language (only meaningful when `configReady`). */
  targetLang: string;
  /** The configured engine (only meaningful when `configReady`). */
  engine: string;
}

export interface AutoTranslateDecision {
  /** Every precondition holds — consume the once-per-open marker. */
  proceed: boolean;
  /** Start a new job; false = surface the cached translation instead. */
  start: boolean;
  /** The job's target language. */
  lang: string;
  /** The job's engine. */
  engine: string;
}

export function decideAutoTranslate(
  i: AutoTranslateInput,
): AutoTranslateDecision {
  const blocked =
    !i.requested ||
    i.articleId == null ||
    !i.openModeKnown ||
    i.rulesFetching ||
    !i.configReady ||
    i.extractionBlocking ||
    !i.canTranslate ||
    i.alreadyMarked;
  if (blocked) {
    return { proceed: false, start: false, lang: i.targetLang, engine: i.engine };
  }
  return {
    proceed: true,
    start: !i.hasCachedTranslation,
    lang: i.targetLang,
    engine: i.engine,
  };
}
