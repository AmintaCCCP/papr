import { describe, expect, it } from "vitest";
import {
  PARSER_KEYS,
  defaultParsers,
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
    });
    const parsed = parseParsers(raw);
    expect(parsed.x.enabled).toBe(true);
    expect(parsed.x.authToken).toBe("abc");
    expect(parsed.x.ct0).toBe("123");
  });

  it("never lets unknown shapes set enabled without an explicit true", () => {
    expect(parseParsers('{"x":{"enabled":"yes"}}').x.enabled).toBe(false);
    expect(parseParsers('{"x":{"enabled":1}}').x.enabled).toBe(false);
    // Blank credentials are stored as undefined, not empty strings.
    expect(parseParsers('{"x":{"authToken":"   "}}').x.authToken).toBeUndefined();
  });

  it("covers every registered parser key", () => {
    expect(PARSER_KEYS).toContain("x");
  });
});

describe("parserReady", () => {
  it("requires both cookies — no unauthenticated fallback", () => {
    expect(parserReady("x", { enabled: false })).toBe(false);
    expect(parserReady("x", { enabled: true, authToken: "a" })).toBe(false);
    expect(parserReady("x", { enabled: true, ct0: "c" })).toBe(false);
    expect(
      parserReady("x", { enabled: true, authToken: "a", ct0: "c" }),
    ).toBe(true);
  });
});
