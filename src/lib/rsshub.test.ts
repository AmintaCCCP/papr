import { describe, expect, it } from "vitest";
import {
  catalogCategories,
  catalogHits,
  concretePath,
  emptyCatalog,
  extractParams,
  filterHits,
  paramsFromExample,
  parseCatalog,
  parseEnvJson,
  requiredEnv,
  templateMatches,
} from "./rsshub";
import type { RsshubCatalog } from "../types";

/** A tiny catalog shaped like the bundled snapshot (namespace-level
 *  categories, routes keyed by canonical full path). */
function fixture(): RsshubCatalog {
  return parseCatalog({
    generatedAt: "2026-01-01T00:00:00Z",
    namespaces: {
      github: {
        name: "GitHub",
        url: "https://github.com",
        description: "GitHub routes",
        categories: ["programming", "social-media"],
        heat: 100,
        routes: {
          "/github/activity/:user": {
            path: "activity/:user",
            name: "User activity",
            example: "/github/activity/DIYgod",
            parameters: { user: "Username" },
            description: "User activity feed",
            features: {
              requireConfig: false,
              requirePuppeteer: false,
              antiCrawler: false,
              supportBT: false,
              supportPodcast: false,
              supportScihub: false,
            },
          },
          "/github/issue/:user/:repo": {
            path: "issue/:user/:repo",
            name: "Issues",
            features: {
              requireConfig: [{ name: "GITHUB_TOKEN", optional: true, description: "a token" }],
              requirePuppeteer: false,
              antiCrawler: true,
              supportBT: false,
              supportPodcast: false,
              supportScihub: false,
            },
          },
        },
      },
      bilibili: {
        name: "哔哩哔哩",
        categories: ["social-media"],
        heat: 500,
        routes: {
          "/bilibili/user/video/:id": {
            path: "user/video/:id",
            name: "UP 主投稿",
            features: {
              requireConfig: false,
              requirePuppeteer: false,
              antiCrawler: false,
              supportBT: false,
              supportPodcast: false,
              supportScihub: false,
            },
          },
          "/bilibili/platform/:area?": {
            path: "platform/:area?",
            name: "会员购票务",
            features: {
              requireConfig: false,
              requirePuppeteer: false,
              antiCrawler: false,
              supportBT: false,
              supportPodcast: false,
              supportScihub: false,
            },
          },
        },
      },
      nsfwsource: {
        name: "NSFW",
        categories: ["nsfw"],
        heat: 900,
        routes: {
          "/nsfwsource/x": {
            path: "x",
            name: "X",
            features: {
              requireConfig: false,
              requirePuppeteer: false,
              antiCrawler: false,
              supportBT: false,
              supportPodcast: false,
              supportScihub: false,
            },
          },
        },
      },
    },
  });
}

describe("parseCatalog", () => {
  it("degrades missing, empty and malformed payloads to an empty catalog", () => {
    for (const bad of [null, undefined, 42, "junk", {}, { namespaces: null }]) {
      const parsed = parseCatalog(bad);
      expect(parsed).toEqual(emptyCatalog());
    }
  });

  it("parses namespaces, routes and features", () => {
    const cat = fixture();
    expect(cat.namespaces.github?.name).toBe("GitHub");
    expect(Object.keys(cat.namespaces.bilibili!.routes)).toHaveLength(2);
    expect(cat.namespaces.bilibili!.heat).toBe(500);
  });

  it("defaults the namespace name to its key and heat to 0", () => {
    const cat = parseCatalog({ namespaces: { weird: { routes: {} } } });
    expect(cat.namespaces.weird?.name).toBe("weird");
    expect(cat.namespaces.weird?.heat).toBe(0);
  });

  it("drops route keys that are not canonical full paths", () => {
    const cat = parseCatalog({
      namespaces: { a: { routes: { relative: {}, "/a/ok": {} } } },
    });
    expect(Object.keys(cat.namespaces.a!.routes)).toEqual(["/a/ok"]);
  });

  it("normalizes requireConfig: empty lists become false", () => {
    const cat = parseCatalog({
      namespaces: {
        a: {
          routes: {
            "/a/x": { features: { requireConfig: [] } },
            "/a/y": { features: { requireConfig: [{ name: "K" }] } },
          },
        },
      },
    });
    expect(requiredEnv(cat.namespaces.a!.routes["/a/x"]!.features)).toEqual([]);
    expect(
      requiredEnv(cat.namespaces.a!.routes["/a/y"]!.features),
    ).toEqual([{ name: "K", optional: false, description: null }]);
  });
});

describe("catalogHits + filterHits", () => {
  it("flattens hottest namespace first (one hit per route)", () => {
    const hits = catalogHits(fixture());
    expect(hits.map((h) => h.namespace)).toEqual([
      "nsfwsource",
      "bilibili",
      "bilibili",
      "github",
      "github",
    ]);
    // Routes within a namespace keep key order.
    expect(hits[1].routePath).toBe("/bilibili/user/video/:id");
  });

  it("searches namespace key, name, url, route name and path", () => {
    const hits = catalogHits(fixture());
    for (const query of ["bilibili", "哔哩", "UP 主", "issue", "/github/"]) {
      expect(filterHits(hits, { query }).length, query).toBeGreaterThan(0);
    }
    expect(filterHits(hits, { query: "no such thing" })).toEqual([]);
  });

  it("filters by category (case-insensitive) and hides nsfw", () => {
    const hits = catalogHits(fixture());
    const social = filterHits(hits, { category: "Social-Media" });
    expect(social.every((h) => h.categories.map((c) => c.toLowerCase()).includes("social-media"))).toBe(true);
    expect(social.some((h) => h.namespace === "github")).toBe(true);

    const hidden = filterHits(hits, { hideNsfw: true });
    expect(hidden.some((h) => h.namespace === "nsfwsource")).toBe(false);
    const shown = filterHits(hits, { hideNsfw: false });
    expect(shown.some((h) => h.namespace === "nsfwsource")).toBe(true);
  });

  it("combines query, category and limit, preserving heat order", () => {
    const hits = catalogHits(fixture());
    const all = filterHits(hits, { hideNsfw: true });
    const limited = filterHits(hits, { hideNsfw: true, limit: 2 });
    expect(limited).toEqual(all.slice(0, 2));
    const both = filterHits(hits, {
      query: "video",
      category: "social-media",
      hideNsfw: true,
    });
    expect(both.map((h) => h.routePath)).toEqual(["/bilibili/user/video/:id"]);
  });
});

describe("catalogCategories", () => {
  it("pools namespace tags with route counts, most-covered first", () => {
    const cats = catalogCategories(fixture());
    const tags = cats.map((c) => c.tag);
    expect(tags).toContain("programming");
    expect(tags).toContain("social-media");
    // github routes(2) + bilibili routes(2) for social-media, github 2 for
    // programming; nsfw routes are counted too.
    expect(cats.find((c) => c.tag === "social-media")!.count).toBe(4);
    expect(cats.find((c) => c.tag === "programming")!.count).toBe(2);
    expect(tags[0]).toBe("social-media");
  });
});

describe("extractParams / concretePath", () => {
  it("extracts :params in order, marking optionals", () => {
    expect(extractParams("/bilibili/user/video/:id")).toEqual([
      { name: "id", optional: false, catchAll: false },
    ]);
    expect(extractParams("/bilibili/platform/:area?/:type?")).toEqual([
      { name: "area", optional: true, catchAll: false },
      { name: "type", optional: true, catchAll: false },
    ]);
    expect(extractParams("/github/activity/:user")).toEqual([
      { name: "user", optional: false, catchAll: false },
    ]);
    expect(extractParams("/no/params")).toEqual([]);
  });

  it("parses hono regex params as catch-alls with clean names", () => {
    expect(extractParams("/81/81rc/:category{.+}?")).toEqual([
      { name: "category", optional: true, catchAll: true },
    ]);
    expect(extractParams("/npm/package/:name{(@[a-z0-9-~][a-z0-9-._~]*/)?[a-z0-9-~][a-z0-9-._~]*}"))
      .toEqual([{ name: "name", optional: false, catchAll: true }]);
  });

  it("substitutes values into the template", () => {
    expect(concretePath("/github/activity/:user", { user: "DIYgod" })).toBe(
      "/github/activity/DIYgod",
    );
    expect(
      concretePath("/slack/user/:id", { id: "W123" }),
    ).toBe("/slack/user/W123");
  });

  it("drops empty optionals and refuses missing requireds", () => {
    expect(concretePath("/bilibili/platform/:area?", {})).toBe("/bilibili/platform");
    expect(concretePath("/bilibili/platform/:area?", { area: "-1" })).toBe(
      "/bilibili/platform/-1",
    );
    expect(concretePath("/github/activity/:user", {})).toBeNull();
    // Whitespace-only counts as empty.
    expect(concretePath("/github/activity/:user", { user: "  " })).toBeNull();
  });

  it("encodes unsafe characters in values", () => {
    expect(concretePath("/x/keyword/:kw", { kw: "a b/c" })).toBe(
      "/x/keyword/a%20b%2Fc",
    );
  });

  it("handles a regex constraint containing a slash (npm scoped packages)", () => {
    // The npm template's regex carries a literal `/` inside the braces —
    // splitting there produced garbage literal fragments in the path.
    const npm = "/npm/package/:name{(@[a-z0-9-~][a-z0-9-._~]*/)?[a-z0-9-~][a-z0-9-._~]*}";
    expect(concretePath(npm, { name: "react" })).toBe("/npm/package/react");
    expect(concretePath(npm, { name: "@rsshub/app" })).toBe("/npm/package/%40rsshub/app");
    expect(paramsFromExample(npm, "/npm/package/react")).toEqual({ name: "react" });
    expect(templateMatches("/npm/package/react", npm)).toBe(true);
    expect(templateMatches("/npm/package/@rsshub/app", npm)).toBe(true);
    expect(templateMatches("/npm/package", npm)).toBe(false);
    expect(templateMatches("/npm/other/react", npm)).toBe(false);
  });

  it("keeps slashes in catch-all values, encoding each segment", () => {
    // The catalog's catch-all routes take a multi-segment value — the
    // separators must survive (encoded slashes made these routes unusable).
    expect(
      concretePath("/81/81rc/:category{.+}?", { category: "sy/gzdt_210283" }),
    ).toBe("/81/81rc/sy/gzdt_210283");
    expect(
      concretePath("/rss/:url{.+}", { url: "https://example.com/blog" }),
    ).toBe("/rss/https%3A//example.com/blog");
    // A required catch-all is still required; an optional one drops out.
    expect(concretePath("/rss/:url{.+}", {})).toBeNull();
    expect(concretePath("/1x/:category{.+}?", {})).toBe("/1x");
  });
});

describe("paramsFromExample", () => {
  it("prefills parameters positionally from the example", () => {
    expect(
      paramsFromExample("/github/activity/:user", "/github/activity/DIYgod"),
    ).toEqual({ user: "DIYgod" });
    expect(
      paramsFromExample(
        "/163/music/user/playrecords/:uid/:type?",
        "/163/music/user/playrecords/123",
      ),
    ).toEqual({ uid: "123" });
  });

  it("tolerates origins and scheme prefixes, and bails on shape mismatch", () => {
    expect(
      paramsFromExample("/github/activity/:user", "https://rsshub.app/github/activity/DIYgod"),
    ).toEqual({ user: "DIYgod" });
    expect(
      paramsFromExample("/github/activity/:user", "rsshub:///github/activity/DIYgod"),
    ).toEqual({ user: "DIYgod" });
    expect(paramsFromExample("/a/:x", "/b/c/d")).toEqual({});
    expect(paramsFromExample("/a/:x", null)).toEqual({});
  });

  it("absorbs the example's tail into a catch-all parameter", () => {
    // The example is deeper than the template — the catch-all takes it all.
    expect(
      paramsFromExample("/81/81rc/:category{.+}?", "/81/81rc/sy/gzdt_210283"),
    ).toEqual({ category: "sy/gzdt_210283" });
    // Literals before the catch-all must still line up.
    expect(paramsFromExample("/81/other/:category{.+}?", "/81/81rc/sy/x")).toEqual({});
    // An optional catch-all with nothing left simply stays empty.
    expect(paramsFromExample("/1x/:category{.+}?", "/1x")).toEqual({});
  });
});

describe("templateMatches", () => {
  it("mirrors the Rust gate: params match any non-empty segment", () => {
    expect(templateMatches("/bilibili/user/video/2267573", "/bilibili/user/video/:id")).toBe(true);
    expect(templateMatches("/github/issue/a/b", "/github/issue/:user/:repo")).toBe(true);
    expect(templateMatches("/bilibili/user/board/1", "/bilibili/user/video/:id")).toBe(false);
    expect(templateMatches("/bilibili/user/video/1/extra", "/bilibili/user/video/:id")).toBe(false);
    expect(templateMatches("/bilibili/user/video", "/bilibili/user/video/:id")).toBe(false);
    expect(templateMatches("/bilibili/platform/-1", "/bilibili/platform/:area?")).toBe(true);
  });

  it("ignores query strings and fragments", () => {
    expect(templateMatches("/bilibili/hot/search?keyword=x", "/bilibili/hot/search")).toBe(true);
    expect(templateMatches("github/activity/DIYgod#top", "/github/activity/:user")).toBe(true);
    expect(templateMatches("/bilibili/hot/other?keyword=x", "/bilibili/hot/search")).toBe(false);
  });

  it("lets catch-all params consume the rest of the path", () => {
    expect(templateMatches("/81/81rc/sy/gzdt_210283", "/81/81rc/:category{.+}?")).toBe(true);
    expect(templateMatches("/rss/https://example.com/feed.xml", "/rss/:url{.+}")).toBe(true);
    expect(templateMatches("/81/81rc", "/81/81rc/:category{.+}?")).toBe(true);
    expect(templateMatches("/rss", "/rss/:url{.+}")).toBe(false);
    expect(templateMatches("/81/other/sy", "/81/81rc/:category{.+}?")).toBe(false);
  });
});

describe("parseEnvJson", () => {
  it("parses stored env blobs defensively", () => {
    expect(parseEnvJson('{"A":"1"}')).toEqual({ A: "1" });
    expect(parseEnvJson("")).toEqual({});
    expect(parseEnvJson("not json")).toEqual({});
    expect(parseEnvJson('{"bad":3,"ok":"yes"}')).toEqual({ ok: "yes" });
    expect(parseEnvJson(null)).toEqual({});
  });
});
