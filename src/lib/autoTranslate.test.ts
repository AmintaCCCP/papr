import { describe, expect, it } from "vitest";
import { decideAutoTranslate } from "./autoTranslate";

// The reader's saved translate configuration, per the review scenario: the
// user configured ja/google while the UI runs in zh — the loading fallbacks
// would start the job in the wrong language/engine.
const saved = { targetLang: "ja", engine: "google" };
const fallbacks = { targetLang: "zh", engine: "llm" };

const base = {
  requested: true,
  articleId: 1 as number | null,
  openModeKnown: true,
  rulesFetching: false,
  configReady: true,
  extractionBlocking: false,
  canTranslate: true,
  alreadyMarked: false,
  hasCachedTranslation: false,
  ...saved,
};

describe("decideAutoTranslate", () => {
  it("starts with the saved configuration once it has loaded", () => {
    expect(decideAutoTranslate(base)).toEqual({
      proceed: true,
      start: true,
      lang: "ja",
      engine: "google",
    });
  });

  it("holds off while the settings are loading, despite the fallback args", () => {
    const d = decideAutoTranslate({ ...base, configReady: false, ...fallbacks });
    expect(d.proceed).toBe(false);
    expect(d.start).toBe(false);
  });

  it("uses the saved configuration — never the fallbacks — when the settings resolve after the article and rules", () => {
    // The delayed-loading scenario: article + rules arrive first, the
    // settings afterwards. The first decision (settings in flight) must not
    // start; the second (settings loaded) must carry the saved values.
    expect(
      decideAutoTranslate({ ...base, configReady: false, ...fallbacks }).proceed,
    ).toBe(false);
    expect(decideAutoTranslate(base)).toEqual({
      proceed: true,
      start: true,
      lang: "ja",
      engine: "google",
    });
  });

  it("skips auto-translate when the settings query failed (config never ready), leaving the toolbar path untouched", () => {
    // A failed query never reaches isSuccess, so configReady stays false and
    // the decision stays blocked for the session.
    const d = decideAutoTranslate({ ...base, configReady: false, ...fallbacks });
    expect(d.proceed).toBe(false);
  });

  it("surfaces a cached translation in the target language without starting a new job", () => {
    const d = decideAutoTranslate({ ...base, hasCachedTranslation: true });
    expect(d.proceed).toBe(true);
    expect(d.start).toBe(false);
    expect(d.lang).toBe("ja");
    expect(d.engine).toBe("google");
  });

  it("blocks on every other precondition", () => {
    const cases: Array<[string, Partial<typeof base>]> = [
      ["nothing asks for translation", { requested: false }],
      ["no article is open", { articleId: null }],
      ["the open mode is unknown (feed list still loading)", { openModeKnown: false }],
      ["the rule-action fetch is in flight", { rulesFetching: true }],
      ["a full-text extraction is blocking", { extractionBlocking: true }],
      ["there is no body to translate", { canTranslate: false }],
      ["this open already attempted translation", { alreadyMarked: true }],
    ];
    for (const [name, over] of cases) {
      const d = decideAutoTranslate({ ...base, ...over });
      expect(d.proceed, name).toBe(false);
      expect(d.start, name).toBe(false);
    }
  });
});
