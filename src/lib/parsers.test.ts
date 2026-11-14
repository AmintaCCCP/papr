import { describe, expect, it } from "vitest";
import {
  PARSER_KEYS,
  PARSER_SOURCE_TYPES,
  defaultParsers,
  isParserSourceType,
  parseParsers,
  parserReady,
} from "./parsers";

describe("parseParsers", () => {
  it("degrades missing, empty and malformed blobs to the default shape", () => {
    const def = defaultParsers();
    expect(parseParsers(null)).toEqual(def);
    expect(parseParsers(undefined)).toEqual(def);
    expect(parseParsers("")).toEqual(def);
    expect(parseParsers("not json at all")).toEqual(def);
    expect(parseParsers('{"x": 3}')).toEqual(def);
    expect(parseParsers('{"y": {"enabled": true}}')).toEqual(def);
  });

  it("round-trips enabled and credential fields", () => {
    const raw = JSON.stringify({
      x: { enabled: true, authToken: " abc ", ct0: "123" },
      xiaohongshu: { enabled: true, cookie: " a1=1; web_session=2 " },
    });
    const parsed = parseParsers(raw);
    expect(parsed.x.enabled).toBe(true);
    expect(parsed.x.authToken).toBe("abc");
    expect(parsed.x.ct0).toBe("123");
    expect(parsed.xiaohongshu.enabled).toBe(true);
    expect(parsed.xiaohongshu.cookie).toBe("a1=1; web_session=2");
    expect(parsed.kuaishou.enabled).toBe(false);
  });

  it("never lets unknown shapes set enabled without an explicit true", () => {
    expect(parseParsers('{"x":{"enabled":"yes"}}').x.enabled).toBe(false);
    expect(parseParsers('{"x":{"enabled":1}}').x.enabled).toBe(false);
    // Blank credentials are stored as undefined, not empty strings.
    expect(parseParsers('{"x":{"authToken":"   "}}').x.authToken).toBeUndefined();
    expect(parseParsers('{"kuaishou":{"cookie":""}}').kuaishou.cookie).toBeUndefined();
  });

  it("covers every registered parser key", () => {
    expect(PARSER_KEYS).toContain("x");
    expect(PARSER_KEYS).toContain("xiaohongshu");
    expect(PARSER_KEYS).toContain("kuaishou");
    // The source-type list and the key list agree — the reader's extraction
    // skip depends on the same set the settings page renders.
    expect(PARSER_SOURCE_TYPES).toEqual(PARSER_KEYS);
  });
});

describe("parserReady", () => {
  it("requires both X cookies — no unauthenticated fallback", () => {
    expect(parserReady("x", { enabled: false })).toBe(false);
    expect(parserReady("x", { enabled: true, authToken: "a" })).toBe(false);
    expect(parserReady("x", { enabled: true, ct0: "c" })).toBe(false);
    expect(
      parserReady("x", { enabled: true, authToken: "a", ct0: "c" }),
    ).toBe(true);
  });

  it("treats the cookie as the credential for the cookie-based parsers", () => {
    expect(parserReady("xiaohongshu", { enabled: true })).toBe(false);
    expect(parserReady("xiaohongshu", { enabled: true, cookie: "a=1" })).toBe(
      true,
    );
    expect(parserReady("kuaishou", { enabled: true, cookie: "a=1" })).toBe(true);
    // authToken/ct0 never satisfy the cookie parsers.
    expect(
      parserReady("xiaohongshu", { enabled: true, authToken: "a", ct0: "c" }),
    ).toBe(false);
  });
});

describe("isParserSourceType", () => {
  it("matches parser kinds and nothing else", () => {
    for (const key of PARSER_SOURCE_TYPES) {
      expect(isParserSourceType(key)).toBe(true);
    }
    expect(isParserSourceType("rss")).toBe(false);
    expect(isParserSourceType("newsletter")).toBe(false);
    expect(isParserSourceType("rsshub")).toBe(false);
    expect(isParserSourceType("youtube")).toBe(false);
  });
});
