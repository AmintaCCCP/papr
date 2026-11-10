// Rebuild the bundled RSSHub route catalog (crates/papr-core/resources/rsshub-catalog.json).
//
// Downloads RSSHub's machine-readable route directory — the documentation
// site's build product (routes.json) — and trims it to the slim schema the
// marketplace ships:
//
//   { generatedAt, namespaces: { <ns>: { name, url, description, categories,
//       heat, routes: { <full path>: { path, name, example, parameters,
//       description, features } } } } }
//
// Everything the engine never needs (maintainers, radar, topFeeds, addedAt,
// location, view, the `zh` title mirror, …) is dropped and the output is
// compactly serialized, cutting ~9 MB of source data down to a bundleable
// payload. Run from the repo root:
//
//   node scripts/update-rsshub-catalog.mjs
//
// The raw source is fetched from the official docs repo, with the jsDelivr
// mirror as fallback (GitHub raw is unreachable from some networks).

import { writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const SOURCES = [
  "https://raw.githubusercontent.com/RSSNext/rsshub-docs/main/src/public/routes.json",
  "https://cdn.jsdelivr.net/gh/RSSNext/rsshub-docs@main/src/public/routes.json",
];

const OUTPUT = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  "crates",
  "papr-core",
  "resources",
  "rsshub-catalog.json",
);

/** Fetch the raw catalog from the first source that answers. */
async function fetchCatalog() {
  let lastError;
  for (const url of SOURCES) {
    try {
      const res = await fetch(url);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      return await res.json();
    } catch (e) {
      lastError = e;
      console.error(`source failed: ${url} (${e.message})`);
    }
  }
  throw lastError;
}

/** Route features we keep — the badges/gating the marketplace UI shows. */
function slimFeatures(raw) {  const f = raw?.features ?? {};
  // requireConfig is `false` or an array of { name, optional, description }.
  // Normalize `true` (never emitted by the docs build, but harmless) to false:
  // only a concrete list is actionable.
  let requireConfig = false;
  if (Array.isArray(f.requireConfig)) {
    requireConfig = f.requireConfig
      .filter((c) => c && typeof c.name === "string")
      .map((c) => ({
        name: c.name,
        optional: c.optional === true,
        ...(typeof c.description === "string" ? { description: c.description } : {}),
      }));
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

/**
 * Normalize one parameter description to a plain string. The docs build
 * usually emits a string, but some routes describe parameters as rich
 * objects ({ description, default, options: [{value, label}] }); flatten
 * those into one readable line for the marketplace's parameter hints.
 */
function paramText(v) {
  if (typeof v === "string") return v;
  if (v && typeof v === "object") {
    const parts = [];
    if (typeof v.description === "string") parts.push(v.description);
    if (typeof v.default === "string") parts.push(`default: ${v.default}`);
    if (Array.isArray(v.options) && v.options.length > 0) {
      const opts = v.options
        .map((o) => {
          if (!o || typeof o.value !== "string") return JSON.stringify(o);
          return typeof o.label === "string" && o.label !== o.value
            ? `${o.value} (${o.label})`
            : o.value;
        })
        .join(", ");
      parts.push(`options: ${opts}`);
    }
    if (parts.length > 0) return parts.join(" · ");
    return JSON.stringify(v);
  }
  return String(v);
}

function slimRoute(rawPath, raw) {
  return {
    path: typeof raw.path === "string" ? raw.path : rawPath,
    name: typeof raw.name === "string" ? raw.name : "",
    ...(typeof raw.example === "string" && raw.example ? { example: raw.example } : {}),
    ...(raw.parameters && typeof raw.parameters === "object"
      ? {
          parameters: Object.fromEntries(
            Object.entries(raw.parameters)
              .filter(([k]) => typeof k === "string" && k !== "")
              .map(([k, v]) => [k, paramText(v)]),
          ),
        }
      : {}),
    ...(typeof raw.description === "string" ? { description: raw.description } : {}),
    features: slimFeatures(raw),
  };
}

function slim(raw) {
  const namespaces = {};
  let routeCount = 0;
  let configCount = 0;
  for (const [ns, body] of Object.entries(raw)) {
    if (!body || typeof body !== "object" || !body.routes) continue;
    const routes = {};
    for (const [routePath, route] of Object.entries(body.routes)) {
      if (!route || typeof route !== "object") continue;
      const slimmed = slimRoute(routePath, route);
      routes[routePath] = slimmed;
      routeCount++;
      if (slimmed.features.requireConfig !== false) configCount++;
    }
    namespaces[ns] = {
      name: typeof body.name === "string" ? body.name : ns,
      ...(typeof body.url === "string" && body.url ? { url: body.url } : {}),
      ...(typeof body.description === "string" ? { description: body.description } : {}),
      ...(Array.isArray(body.categories) ? { categories: body.categories } : {}),
      ...(typeof body.heat === "number" ? { heat: body.heat } : {}),
      routes,
    };
  }
  return {
    generatedAt: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
    namespaces,
    _stats: { namespaces: Object.keys(namespaces).length, routes: routeCount, requireConfig: configCount },
  };
}

const rawCatalog = await fetchCatalog();
const slimmed = slim(rawCatalog);
const { _stats } = slimmed;
// The stats block is for the update log only — not part of the shipped schema.
delete slimmed._stats;

const json = JSON.stringify(slimmed);
await writeFile(OUTPUT, json, "utf8");
console.log(
  `rsshub-catalog.json written: ${_stats.namespaces} namespaces, ` +
    `${_stats.routes} routes (${_stats.requireConfig} require config), ` +
    `${(json.length / 1024 / 1024).toFixed(2)} MB`,
);
