// Parser configuration model + readiness rules.
//
// The whole configuration persists as one JSON blob in the backend settings
// table (key `parsers`), mirroring the `integrations` blob: the settings UI
// writes it, and the Rust ingestion side (`papr_core::ingestion::parsers`)
// reads it when a feed add or refresh routes through a parser. A parser feed
// is an ordinary feeds row whose `source_type` names the parser's kind
// (`"x"`, `"xiaohongshu"`, `"kuaishou"`); enabling a parser simply makes its
// URL patterns subscribable.
//
// Credential models differ per parser: X needs its two session cookies (hard
// requirement), while xiaohongshu works anonymously from the SSR profile page
// (a pasted cookie is an upgrade) and kuaishou's web API is captcha-gated for
// anonymous traffic (a pasted cookie is the supported path) — the UI says so
// per parser instead of one hard rule.

/** The built-in parsers, in settings-page display order. */
export type ParserKey = "x" | "xiaohongshu" | "kuaishou";

export const PARSER_KEYS: ParserKey[] = ["x", "xiaohongshu", "kuaishou"];

/** The `source_type` strings the backend treats as parser-rendered — the
 *  reader uses this to skip web extraction for these articles (the parser
 *  markup is the final content; the sites offer no extractable web page). */
export const PARSER_SOURCE_TYPES: readonly string[] = [
  "x",
  "xiaohongshu",
  "kuaishou",
];

/** A parser `source_type`, or `false` for plain feeds. */
export function isParserSourceType(sourceType: string): boolean {
  return PARSER_SOURCE_TYPES.includes(sourceType);
}

/** One parser's configuration. Only the fields its kind needs are ever
 *  written; the rest stay undefined. */
export interface ParserConfig {
  enabled: boolean;
  /** X: the `auth_token` session cookie (from a logged-in x.com browser). */
  authToken?: string;
  /** X: the `ct0` CSRF cookie, sent back as `X-CSRF-Token`. */
  ct0?: string;
  /** Xiaohongshu / Kuaishou: the pasted `Cookie` header from a logged-in
   *  browser session. Optional — see the per-parser credential notes. */
  cookie?: string;
}

export interface ParsersSettings {
  x: ParserConfig;
  xiaohongshu: ParserConfig;
  kuaishou: ParserConfig;
}

/** Display metadata: localized name/description keys, brand colour, logo
 *  glyph, and the per-parser test-field wording. */
export const PARSER_META: Record<
  ParserKey,
  {
    nameKey: string;
    descKey: string;
    color: string;
    glyph: string;
    itemKey: string;
    testPlaceholderKey: string;
    cookieHintKey: string;
  }
> = {
  x: {
    nameKey: "settings.parsers.x.name",
    descKey: "settings.parsers.x.desc",
    color: "#1F2328",
    glyph: "X",
    itemKey: "settings.parsers.item.x",
    testPlaceholderKey: "settings.parsers.testHandlePlaceholder.x",
    cookieHintKey: "settings.parsers.cookieHint.x",
  },
  xiaohongshu: {
    nameKey: "settings.parsers.xiaohongshu.name",
    descKey: "settings.parsers.xiaohongshu.desc",
    color: "#FF2442",
    glyph: "红",
    itemKey: "settings.parsers.item.xiaohongshu",
    testPlaceholderKey: "settings.parsers.testHandlePlaceholder.generic",
    cookieHintKey: "settings.parsers.cookieHint.xiaohongshu",
  },
  kuaishou: {
    nameKey: "settings.parsers.kuaishou.name",
    descKey: "settings.parsers.kuaishou.desc",
    color: "#FE3666",
    glyph: "快",
    itemKey: "settings.parsers.item.kuaishou",
    testPlaceholderKey: "settings.parsers.testHandlePlaceholder.generic",
    cookieHintKey: "settings.parsers.cookieHint.kuaishou",
  },
};

/** The credential fields each parser shows, in display order. Only string
 *  fields — `enabled` is the card's toggle, never a form field. */
export const PARSER_FIELDS: Record<
  ParserKey,
  { key: "authToken" | "ct0" | "cookie"; password: boolean }[]
> = {
  x: [
    { key: "authToken", password: true },
    { key: "ct0", password: true },
  ],
  xiaohongshu: [{ key: "cookie", password: true }],
  kuaishou: [{ key: "cookie", password: true }],
};

/** Default configuration — everything off, no credentials. */
export function defaultParsers(): ParsersSettings {
  return {
    x: { enabled: false },
    xiaohongshu: { enabled: false },
    kuaishou: { enabled: false },
  };
}

/** A non-empty trimmed string, or undefined — the shape configs persist. */
function cleanStr(v: unknown): string | undefined {
  if (typeof v !== "string") return undefined;
  const t = v.trim();
  return t === "" ? undefined : t;
}

/** Parse the stored blob defensively: unknown JSON degrades to the default
 *  shape (the UI re-persists a clean blob on the next save). */
export function parseParsers(raw: unknown): ParsersSettings {
  const out = defaultParsers();
  if (typeof raw !== "string" || raw === "") return out;
  let obj: unknown;
  try {
    obj = JSON.parse(raw);
  } catch {
    return out;
  }
  if (typeof obj !== "object" || obj === null) return out;
  for (const key of PARSER_KEYS) {
    const rawCfg = (obj as Record<string, unknown>)[key];
    if (typeof rawCfg !== "object" || rawCfg === null) continue;
    const cfg = rawCfg as Record<string, unknown>;
    out[key] = {
      enabled: cfg.enabled === true,
      authToken: cleanStr(cfg.authToken),
      ct0: cleanStr(cfg.ct0),
      cookie: cleanStr(cfg.cookie),
    };
  }
  return out;
}

/** The parser's credential readiness. X has no unauthenticated fallback, so
 *  "ready" means both cookies; the other parsers work anonymously (cookie is
 *  an upgrade or the supported path), so "ready" mirrors the X card's signal
 *  only in spirit: it reports the credential as configured when present. */
export function parserReady(key: ParserKey, cfg: ParserConfig): boolean {
  if (key === "x") return Boolean(cfg.authToken && cfg.ct0);
  return Boolean(cfg.cookie);
}
