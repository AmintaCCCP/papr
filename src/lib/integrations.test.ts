import { describe, expect, it } from "vitest";
import {
  activeIntegrations,
  builtinReady,
  customReady,
  defaultIntegrations,
  parseIntegrations,
} from "./integrations";

describe("parseIntegrations", () => {
  it("returns defaults for absent / corrupt blobs", () => {
    for (const raw of [null, "", "not json", "[]", "42", '{"builtins":"oops"}']) {
      const parsed = parseIntegrations(raw);
      expect(parsed).toEqual(defaultIntegrations());
    }
  });

  it("parses a full blob and drops unknown fields", () => {
    const blob = JSON.stringify({
      useAiSummary: true,
      builtins: {
        cubox: { enabled: true, apiUrl: " https://cubox.pro/c/api/save/x ", junk: 1 },
      },
      customs: [
        {
          id: "a",
          name: "My hook",
          kind: "webhook",
          enabled: true,
          method: "put",
          url: "https://hook.test",
          headers: { "X-Auth": "t", "": "dropped", ok: 42 },
          body: '{"t":"[title]"}',
        },
        { id: "", enabled: true }, // malformed → skipped
        "junk", // skipped
      ],
    });
    const parsed = parseIntegrations(blob);
    expect(parsed.useAiSummary).toBe(true);
    expect(parsed.builtins.cubox).toEqual({
      enabled: true,
      apiUrl: "https://cubox.pro/c/api/save/x",
    });
    expect(parsed.builtins.readwise.enabled).toBe(false);
    expect(parsed.customs).toHaveLength(1);
    const hook = parsed.customs[0];
    expect(hook.method).toBe("PUT");
    expect(hook.headers).toEqual({ "X-Auth": "t", ok: "42" });
  });

  it("caps a bloated customs list", () => {
    const customs = Array.from({ length: 80 }, (_, i) => ({
      id: `c${i}`,
      name: `n${i}`,
      enabled: false,
    }));
    expect(parseIntegrations(JSON.stringify({ customs })).customs).toHaveLength(50);
  });
});

describe("builtinReady", () => {
  it("requires every credential the kind needs", () => {
    expect(
      builtinReady("cubox", { enabled: true, apiUrl: "https://x" }),
    ).toBe(true);
    expect(builtinReady("cubox", { enabled: true })).toBe(false);
    expect(
      builtinReady("outline", {
        enabled: true,
        endpoint: "https://w/api",
        token: "t",
        collection: "kb",
      }),
    ).toBe(true);
    expect(
      builtinReady("outline", { enabled: true, endpoint: "https://w/api", token: "t" }),
    ).toBe(false);
    expect(
      builtinReady("readwise", { enabled: false, token: "t" }),
    ).toBe(false);
  });
});

describe("customReady", () => {
  const hook = {
    id: "a",
    name: "hook",
    kind: "webhook" as const,
    enabled: true,
    method: "POST" as const,
    url: "https://hook.test",
  };
  it("webhook needs method + url", () => {
    expect(customReady(hook)).toBe(true);
    expect(customReady({ ...hook, url: undefined })).toBe(false);
    expect(customReady({ ...hook, enabled: false })).toBe(false);
  });
  it("url scheme needs a statically valid scheme", () => {
    const scheme = { id: "b", name: "s", kind: "urlScheme" as const, enabled: true };
    expect(customReady({ ...scheme, scheme: "obsidian://new" })).toBe(true);
    // dynamic template — the prefix contains a token, validated in Rust
    expect(customReady({ ...scheme, scheme: "obsidian://new?name=[title]" })).toBe(true);
    // rejected shapes
    expect(customReady({ ...scheme, scheme: "obsidian" })).toBe(false);
    expect(customReady({ ...scheme })).toBe(false);
    expect(customReady({ ...scheme, scheme: "javascript://alert(1)" })).toBe(false);
    expect(customReady({ ...scheme, scheme: "file:///etc" })).toBe(false);
    expect(customReady({ ...scheme, scheme: "x://" })).toBe(false);
    expect(customReady({ ...scheme, scheme: "://x" })).toBe(false);
    expect(customReady({ ...scheme, scheme: "a b://x" })).toBe(false);
  });
});

describe("activeIntegrations", () => {
  it("lists ready built-ins in display order then ready customs", () => {
    const settings = parseIntegrations(null);
    settings.builtins.zotero = { enabled: true, userId: "1", token: "t" };
    settings.builtins.cubox = { enabled: true, apiUrl: "https://cubox/x" };
    settings.builtins.readwise = { enabled: true }; // missing token → not active
    settings.customs.push(
      {
        id: "h1",
        name: "Hook",
        kind: "webhook",
        enabled: true,
        method: "POST",
        url: "https://hook.test",
      },
      {
        id: "h2",
        name: "Scheme",
        kind: "urlScheme",
        enabled: true,
        scheme: "obsidian://new",
      },
      {
        id: "h3",
        name: "Off",
        kind: "webhook",
        enabled: false,
        method: "POST",
        url: "https://hook.test",
      },
    );
    const active = activeIntegrations(settings);
    expect(active.map((a) => a.id)).toEqual(["cubox", "zotero", "h1", "h2"]);
    // Built-ins expose their credential fields (minus the enabled flag);
    // webhooks pass the template pieces through.
    expect(active[0].config).toEqual({ apiUrl: "https://cubox/x" });
    expect(active[2].config).toEqual({
      method: "POST",
      url: "https://hook.test",
      headers: {},
      body: undefined,
    });
    expect(active[3].config).toEqual({ scheme: "obsidian://new" });
  });
});
