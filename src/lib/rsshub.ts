// RSSHub route marketplace logic — the pure, testable half of the
// marketplace UI (mirrors `papr_core::ingestion::rsshub`).
//
// The backend ships the whole slim catalog over IPC once per session
// (`rsshub_marketplace_catalog`); everything here filters and shapes it
// client-side so search, category chips and heat sorting react instantly.
// Parsing is defensive throughout: a malformed catalog degrades to an empty
// one instead of breaking the dialog, the same trade-off parseParsers makes.

import type {
  RsshubCatalog,
  RsshubConfigItem,
  RsshubNamespace,
  RsshubRequireConfig,
  RsshubRoute,
  RsshubRouteFeatures,
} from "../types";

/** One flattened search result: a route plus its namespace context. */
export interface RsshubHit {
  namespace: string;
  namespaceName: string;
  namespaceUrl: string | null;
  /** Namespace category tags (the slim schema has no per-route tags). */
  categories: string[];
  /** Namespace popularity — the primary sort key (descending). */
  heat: number;
  /** The route's canonical full path (the subscription template). */
  routePath: string;
  route: RsshubRoute;
}

/** An empty catalog — the degraded shape for any malformed input. */
export function emptyCatalog(): RsshubCatalog {
  return { generatedAt: "", namespaces: {} };
}

function cleanStr(v: unknown): string | undefined {
  if (typeof v !== "string") return undefined;
  const t = v.trim();
  return t === "" ? undefined : t;
}

function parseFeatures(raw: unknown): RsshubRouteFeatures {
  const f =
    typeof raw === "object" && raw !== null
      ? (raw as Record<string, unknown>)
      : {};
  let requireConfig: RsshubRequireConfig = false;
  if (Array.isArray(f.requireConfig)) {
    const items: RsshubConfigItem[] = [];
    for (const item of f.requireConfig) {
      if (typeof item !== "object" || item === null) continue;
      const c = item as Record<string, unknown>;
      if (typeof c.name !== "string" || c.name === "") continue;
      items.push({
        name: c.name,
        optional: c.optional === true,
        description: cleanStr(c.description) ?? null,
      });
    }
    // An empty list means "no actionable requirement" — store false.
    requireConfig = items.length > 0 ? items : false;
  }
  return {
    requireConfig,
    requirePuppeteer: f.requirePuppeteer === true,
    antiCrawler: f.antiCrawler === true,
    supportBT: f.supportBT === true,
    supportPodcast: f.supportPodcast === true,
    supportScihub: f.supportScihub === true,
  };
}

function parseRoute(raw: unknown): RsshubRoute {
  const r =
    typeof raw === "object" && raw !== null
      ? (raw as Record<string, unknown>)
      : {};
  const parameters: Record<string, string> = {};
  if (typeof r.parameters === "object" && r.parameters !== null) {
    for (const [k, v] of Object.entries(
      r.parameters as Record<string, unknown>,
    )) {
      if (typeof k !== "string" || k === "") continue;
      if (typeof v === "string") parameters[k] = v;
    }
  }
  return {
    path: typeof r.path === "string" ? r.path : "",
    name: typeof r.name === "string" ? r.name : "",
    example: cleanStr(r.example) ?? null,
    parameters,
    description: cleanStr(r.description) ?? null,
    features: parseFeatures(r.features),
  };
}

/** Parse the backend's catalog payload defensively: any malformed shape
 *  degrades to an empty catalog rather than throwing. */
export function parseCatalog(raw: unknown): RsshubCatalog {
  if (typeof raw !== "object" || raw === null) return emptyCatalog();
  const obj = raw as Record<string, unknown>;
  if (typeof obj.namespaces !== "object" || obj.namespaces === null) {
    return emptyCatalog();
  }
  const namespaces: Record<string, RsshubNamespace> = {};
  for (const [ns, rawNs] of Object.entries(
    obj.namespaces as Record<string, unknown>,
  )) {
    if (typeof ns !== "string" || ns === "") continue;
    if (typeof rawNs !== "object" || rawNs === null) continue;
    const n = rawNs as Record<string, unknown>;
    const routes: Record<string, RsshubRoute> = {};
    if (typeof n.routes === "object" && n.routes !== null) {
      for (const [routePath, rawRoute] of Object.entries(
        n.routes as Record<string, unknown>,
      )) {
        // The route's key is its canonical full path — the registry and the
        // `rsshub://` links both use it, so a bad key is dropped outright.
        if (typeof routePath !== "string" || !routePath.startsWith("/")) {
          continue;
        }
        routes[routePath] = parseRoute(rawRoute);
      }
    }
    namespaces[ns] = {
      name: typeof n.name === "string" && n.name !== "" ? n.name : ns,
      url: cleanStr(n.url) ?? null,
      description: cleanStr(n.description) ?? null,
      categories: Array.isArray(n.categories)
        ? n.categories.filter((c): c is string => typeof c === "string")
        : [],
      heat: typeof n.heat === "number" && Number.isFinite(n.heat) ? n.heat : 0,
      routes,
    };
  }
  return {
    generatedAt: typeof obj.generatedAt === "string" ? obj.generatedAt : "",
    namespaces,
  };
}

/** Flatten a catalog into hits, hottest namespace first (route path as the
 *  deterministic tie-break) — the marketplace's default ordering. */
export function catalogHits(catalog: RsshubCatalog): RsshubHit[] {
  const hits: RsshubHit[] = [];
  const namespaces = Object.entries(catalog.namespaces);
  namespaces.sort(
    (a, b) => b[1].heat - a[1].heat || a[0].localeCompare(b[0]),
  );
  for (const [ns, n] of namespaces) {
    for (const [routePath, route] of Object.entries(n.routes)) {
      hits.push({
        namespace: ns,
        namespaceName: n.name,
        namespaceUrl: n.url ?? null,
        categories: n.categories,
        heat: n.heat,
        routePath,
        route,
      });
    }
  }
  return hits;
}

export interface CatalogFilter {
  query?: string;
  /** Namespace category tag; null/undefined shows everything. */
  category?: string | null;
  hideNsfw?: boolean;
  /** 0 / undefined = unlimited. */
  limit?: number;
}

/** True when `categories` carries the NSFW tag (case-insensitive). */
function isNsfw(categories: string[]): boolean {
  return categories.some((c) => c.toLowerCase() === "nsfw");
}

/** Client-side filter over the flattened hits — substring search across
 *  namespace key/name/url and route name/path/description, category tag
 *  match, NSFW hiding, heat order preserved, optional limit. */
export function filterHits(hits: RsshubHit[], filter: CatalogFilter): RsshubHit[] {
  const needle = (filter.query ?? "").trim().toLowerCase();
  const category = filter.category?.toLowerCase() ?? null;
  const out: RsshubHit[] = [];
  const seen = new Set<string>();
  for (const hit of hits) {
    if (filter.hideNsfw && isNsfw(hit.categories)) continue;
    if (category) {
      const tags = hit.categories.map((c) => c.toLowerCase());
      if (!tags.includes(category)) continue;
    }
    if (needle) {
      const match =
        hit.namespace.toLowerCase().includes(needle) ||
        hit.namespaceName.toLowerCase().includes(needle) ||
        (hit.namespaceUrl ?? "").toLowerCase().includes(needle) ||
        hit.route.name.toLowerCase().includes(needle) ||
        hit.routePath.toLowerCase().includes(needle) ||
        (hit.route.description ?? "").toLowerCase().includes(needle);
      if (!match) continue;
    }
    // The flatten already dedupes, but a defensive key keeps the contract
    // explicit for tests and future callers.
    if (seen.has(hit.routePath)) continue;
    seen.add(hit.routePath);
    out.push(hit);
    if (filter.limit && out.length >= filter.limit) break;
  }
  return out;
}

/** Distinct category tags with route counts, most-covered first — the
 *  category dropdown. */
export function catalogCategories(
  catalog: RsshubCatalog,
): { tag: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const ns of Object.values(catalog.namespaces)) {
    for (const raw of ns.categories) {
      const tag = raw.toLowerCase();
      counts.set(tag, (counts.get(tag) ?? 0) + Object.keys(ns.routes).length);
    }
  }
  return [...counts.entries()]
    .map(([tag, count]) => ({ tag, count }))
    .sort((a, b) => b.count - a.count || a.tag.localeCompare(b.tag));
}

/** One template parameter, parsed out of its path segment. */
export interface RouteParam {
  name: string;
  optional: boolean;
  /** A hono regex-constrained param (`:name{.+}`) — its value spans
   *  slashes, so the concrete path may carry several segments. */
  catchAll: boolean;
}

/** Split a route template into segments, treating a slash inside a `{…}`
 *  regex constraint as part of the segment rather than a separator: hono
 *  applies the brace regex to the remaining path, and the npm scoped-package
 *  template carries a literal slash inside the braces that must not split it
 *  into a param fragment plus a garbage literal. */
function splitTemplateSegments(template: string): string[] {
  const segments: string[] = [];
  let depth = 0;
  let current = "";
  for (const ch of template) {
    if (ch === "{") depth++;
    else if (ch === "}") depth = Math.max(0, depth - 1);
    if (ch === "/" && depth === 0) {
      segments.push(current);
      current = "";
    } else {
      current += ch;
    }
  }
  segments.push(current);
  return segments;
}

/** Parse one template segment into its parameter shape, or `null` when the
 *  segment is a literal. Handles hono's suffixes: the optional `?` trails
 *  everything (`:name?`, `:name{.+}?`) and the regex constraint sits in
 *  braces (as in the npm route's scoped-package pattern). */
function parseParamSegment(seg: string): RouteParam | null {
  if (!seg.startsWith(":")) return null;
  let body = seg.slice(1);
  let optional = false;
  if (body.endsWith("?")) {
    optional = true;
    body = body.slice(0, -1);
  }
  let catchAll = false;
  const brace = body.indexOf("{");
  if (brace >= 0) {
    catchAll = true;
    body = body.slice(0, brace);
  }
  return { name: body, optional, catchAll };
}

/** Extract the `:param` / `:param?` / `:param{.+}` placeholders of a route
 *  template, in order of appearance. */
export function extractParams(template: string): RouteParam[] {
  const out: RouteParam[] = [];
  for (const seg of splitTemplateSegments(template)) {
    const p = parseParamSegment(seg);
    if (p && p.name) out.push(p);
  }
  return out;
}

/** Substitute the user's parameter values into a route template, producing
 *  the concrete path to subscribe with. Returns `null` while any required
 *  parameter is still empty — the "install" buttons stay disabled then.
 *  Optional parameters left empty simply drop out of the path. A catch-all
 *  parameter's value may span segments: each slash-separated part is
 *  encoded, the separators themselves are kept. */
export function concretePath(
  template: string,
  values: Record<string, string>,
): string | null {
  const segments: string[] = [];
  for (const seg of splitTemplateSegments(template)) {
    const p = parseParamSegment(seg);
    if (!p) {
      segments.push(seg);
      continue;
    }
    const value = (values[p.name] ?? "").trim();
    if (p.catchAll) {
      // Split and rejoin with the separators intact — `encodeURIComponent`
      // leaves `/` alone, so each part round-trips and a `//` in the value
      // (e.g. the scheme of a URL for `/rss/:url{.+}`) survives.
      const parts = value.split("/");
      if (parts.some((s) => s !== "")) {
        segments.push(parts.map(encodeURIComponent).join("/"));
      } else if (!p.optional) {
        return null; // a required catch-all is missing
      }
      // An empty optional catch-all drops out of the path.
      continue;
    }
    if (value) {
      segments.push(encodeURIComponent(value));
    } else if (p.optional) {
      continue; // drop the empty optional segment
    } else {
      return null; // a required parameter is missing
    }
  }
  const path = "/" + segments.filter((s) => s !== "").join("/");
  return path === "/" ? null : path;
}

/** Parse a route's example URL to prefill the parameter form: template
 *  placeholder segments are matched positionally against the example's
 *  segments. The example may omit trailing optional parameters, but any
 *  bigger shape mismatch (different fixed segments) disables the prefill
 *  rather than filling wrong values — except around a catch-all parameter,
 *  which absorbs every example segment from its position on. */
export function paramsFromExample(
  template: string,
  example: string | null | undefined,
): Record<string, string> {
  const values: Record<string, string> = {};
  const cleaned = (example ?? "").trim();
  if (!cleaned) return values;
  let path = cleaned;
  // Examples are paths like `/bilibili/platform/-1`, sometimes with the
  // instance origin or the rsshub:// scheme in front.
  if (path.includes("://")) {
    try {
      path = new URL(path).pathname;
    } catch {
      path = path.slice(path.indexOf("://") + 3);
    }
  }
  const exampleSegments = path.split("/").filter((s) => s !== "");
  const templateSegments = splitTemplateSegments(template).filter(
    (s) => s !== "",
  );
  const parsed = templateSegments.map(parseParamSegment);
  // A catch-all parameter eats the rest of the example: everything from its
  // position on (joined back with slashes, the shape concretePath keeps).
  const catchIdx = parsed.findIndex((p) => p?.catchAll);
  if (catchIdx >= 0) {
    // The literal segments before the catch-all must match the example
    // exactly — a shape mismatch disables the prefill (parameters before it
    // match positionally like everywhere else).
    const literalsMatch =
      exampleSegments.length >= catchIdx &&
      parsed
        .slice(0, catchIdx)
        .every((p, i) => p !== null || exampleSegments[i] === templateSegments[i]);
    if (!literalsMatch) return values;
    parsed.forEach((p, i) => {
      if (!p || i > catchIdx) return;
      const value =
        i === catchIdx ? exampleSegments.slice(i).join("/") : exampleSegments[i];
      if (p.name && value) values[p.name] = value;
    });
    return values;
  }
  // How many trailing optional parameters the example is allowed to skip.
  let trailingOptional = 0;
  for (let i = templateSegments.length - 1; i >= 0; i--) {
    const seg = templateSegments[i];
    if (seg.startsWith(":") && seg.endsWith("?")) trailingOptional++;
    else break;
  }
  if (
    exampleSegments.length > templateSegments.length ||
    exampleSegments.length < templateSegments.length - trailingOptional
  ) {
    return values;
  }
  templateSegments.forEach((_seg, i) => {
    const p = parsed[i];
    if (!p) return;
    const value = exampleSegments[i];
    if (p.name && value) values[p.name] = value;
  });
  return values;
}

/** Mirror of the Rust `template_matches`: true when a concrete route path
 *  instantiates the template (`:seg` matches any non-empty segment; a
 *  `:seg{regex}` catch-all consumes the rest). A `?query` / `#fragment` on
 *  the concrete link travels to the engine but never participates in
 *  matching. Inner empty segments are preserved — an empty concrete segment
 *  matches an optional parameter, mirroring the Rust implementation. */
export function templateMatches(concrete: string, template: string): boolean {
  const path = (concrete.split(/[?#]/)[0] ?? "").replace(/^\/+|\/+$/g, "");
  const c = path === "" ? [""] : path.split("/");
  const t = splitTemplateSegments(template.replace(/^\/+|\/+$/g, ""));
  let i = 0;
  for (; i < t.length; i++) {
    const seg = t[i];
    if (!seg.startsWith(":")) {
      if (c[i] !== seg) return false;
      continue;
    }
    // A regex-constrained param spans slashes — it eats the rest of the
    // concrete path (at least one segment unless optional).
    if (seg.includes("{")) {
      if (i < c.length) return true;
      return seg.endsWith("?");
    }
    if (c[i] === undefined) {
      // The concrete path ended mid-template: only optional segments may
      // remain unmatched.
      return t.slice(i).every((s) => s.endsWith("?"));
    }
    if (c[i] === "" && !seg.endsWith("?")) {
      return false; // a required param segment must have content
    }
  }
  // Every remaining concrete segment must have been consumed — a concrete
  // path deeper than the template does not match.
  return i === c.length;
}

/** The env variables a route requires — an empty array when `requireConfig`
 *  is `false` (or not a non-empty list). Optional ones carry `optional`. */
export function requiredEnv(features: RsshubRouteFeatures): RsshubConfigItem[] {
  return Array.isArray(features.requireConfig) ? features.requireConfig : [];
}

/** Parse a route row's stored `env_json` defensively into a plain record. */
export function parseEnvJson(raw: string | null | undefined): Record<string, string> {
  if (typeof raw !== "string" || raw === "") return {};
  let obj: unknown;
  try {
    obj = JSON.parse(raw);
  } catch {
    return {};
  }
  if (typeof obj !== "object" || obj === null) return {};
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(obj as Record<string, unknown>)) {
    if (typeof k === "string" && k !== "" && typeof v === "string") {
      out[k] = v;
    }
  }
  return out;
}
