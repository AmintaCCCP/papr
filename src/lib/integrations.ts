// Integration configuration model + activation rules.
//
// The whole configuration persists as one JSON blob in the backend settings
// table (key `integrations`). Built-ins are singleton service configs toggled
// on with their credentials; custom integrations are a list of webhook /
// URL-scheme templates the reader runs per article. The Rust side
// (`src-tauri/src/integrations.rs`) executes each run — this module only
// shapes, validates and activates what the settings UI edits and the reader
// toolbar renders.

import type { IconName } from "../components/Icon";

/** The built-in services, in settings-page display order. */
export type BuiltinKey =
  | "cubox"
  | "readwise"
  | "instapaper"
  | "outline"
  | "readeck"
  | "zotero"
  | "obsidian";

export const BUILTIN_KEYS: BuiltinKey[] = [
  "cubox",
  "readwise",
  "instapaper",
  "outline",
  "readeck",
  "zotero",
  "obsidian",
];

/** The two custom-integration kinds. */
export type CustomKind = "webhook" | "urlScheme";

export type IntegrationKind = BuiltinKey | CustomKind;

/** One built-in service's configuration. Only the fields its kind needs are
 *  ever written; the rest stay undefined. */
export interface BuiltinConfig {
  enabled: boolean;
  /** Cubox personal API URL. */
  apiUrl?: string;
  /** Readwise / Outline / Readeck / Zotero token. */
  token?: string;
  /** Instapaper credentials. */
  username?: string;
  password?: string;
  /** Outline / Readeck server root. */
  endpoint?: string;
  /** Outline collection name or UUID. */
  collection?: string;
  /** Zotero user id. */
  userId?: string;
  /** Obsidian vault directory. */
  vaultPath?: string;
}

/** A user-defined webhook or URL-scheme integration. */
export interface CustomIntegration {
  id: string;
  name: string;
  kind: CustomKind;
  enabled: boolean;
  /** Webhook: HTTP method, URL template, headers, body template. */
  method?: string;
  url?: string;
  headers?: Record<string, string>;
  body?: string;
  /** URL scheme: the scheme template (e.g. `obsidian://new?…`). */
  scheme?: string;
}

export interface IntegrationSettings {
  /** Use the article's AI summary as the description services receive. */
  useAiSummary: boolean;
  builtins: Record<BuiltinKey, BuiltinConfig>;
  customs: CustomIntegration[];
}

/** Display metadata: localized name/description keys, toolbar icon, brand
 *  colour (the settings logo chip only — toolbar icons stay monochrome). */
export const BUILTIN_META: Record<
  BuiltinKey,
  { nameKey: string; descKey: string; icon: IconName; color: string }
> = {
  cubox: {
    nameKey: "settings.integrations.cubox.name",
    descKey: "settings.integrations.cubox.desc",
    icon: "box",
    color: "#3B82F6",
  },
  readwise: {
    nameKey: "settings.integrations.readwise.name",
    descKey: "settings.integrations.readwise.desc",
    icon: "book",
    color: "#E4573D",
  },
  instapaper: {
    nameKey: "settings.integrations.instapaper.name",
    descKey: "settings.integrations.instapaper.desc",
    icon: "info",
    color: "#1F2328",
  },
  outline: {
    nameKey: "settings.integrations.outline.name",
    descKey: "settings.integrations.outline.desc",
    icon: "doc",
    color: "#3E5EEA",
  },
  readeck: {
    nameKey: "settings.integrations.readeck.name",
    descKey: "settings.integrations.readeck.desc",
    icon: "book-open",
    color: "#2E7D4F",
  },
  zotero: {
    nameKey: "settings.integrations.zotero.name",
    descKey: "settings.integrations.zotero.desc",
    icon: "flask",
    color: "#CC2936",
  },
  obsidian: {
    nameKey: "settings.integrations.obsidian.name",
    descKey: "settings.integrations.obsidian.desc",
    icon: "gem",
    color: "#8B5CF6",
  },
};

/** The eight template tokens a custom integration may interpolate, with the
 *  i18n key describing each. Mirrors the Rust placeholder engine. */
export const PLACEHOLDERS: { token: string; descKey: string }[] = [
  { token: "[title]", descKey: "settings.integrations.ph.title" },
  { token: "[url]", descKey: "settings.integrations.ph.url" },
  { token: "[content_html]", descKey: "settings.integrations.ph.contentHtml" },
  {
    token: "[content_markdown]",
    descKey: "settings.integrations.ph.contentMarkdown",
  },
  { token: "[summary]", descKey: "settings.integrations.ph.summary" },
  { token: "[description]", descKey: "settings.integrations.ph.description" },
  { token: "[author]", descKey: "settings.integrations.ph.author" },
  { token: "[published_at]", descKey: "settings.integrations.ph.publishedAt" },
];

/** The HTTP methods a webhook may declare (mirrors the Rust allowlist). */
export const WEBHOOK_METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE"] as const;

const emptyBuiltin = (): BuiltinConfig => ({ enabled: false });

/** Default configuration — everything off, no customs. */
export function defaultIntegrations(): IntegrationSettings {
  const builtins = {} as Record<BuiltinKey, BuiltinConfig>;
  for (const key of BUILTIN_KEYS) builtins[key] = emptyBuiltin();
  return { useAiSummary: false, builtins, customs: [] };
}

/** A non-empty trimmed string, or undefined — the shape configs persist. */
function cleanStr(v: unknown): string | undefined {
  if (typeof v !== "string") return undefined;
  const t = v.trim();
  return t === "" ? undefined : t;
}

/** Parse one built-in config defensively: only known keys survive, strings
 *  are trimmed, everything else falls back to the default shape. */
function parseBuiltin(raw: unknown): BuiltinConfig {
  const cfg = emptyBuiltin();
  if (typeof raw !== "object" || raw === null) return cfg;
  const o = raw as Record<string, unknown>;
  const out: BuiltinConfig = {
    enabled: o.enabled === true,
    apiUrl: cleanStr(o.apiUrl),
    token: cleanStr(o.token),
    username: cleanStr(o.username),
    password: cleanStr(o.password),
    endpoint: cleanStr(o.endpoint),
    collection: cleanStr(o.collection),
    userId: cleanStr(o.userId),
    vaultPath: cleanStr(o.vaultPath),
  };
  return { ...cfg, ...out };
}

/** Parse the customs list defensively — unknown kinds drop to `webhook`,
 *  malformed entries are skipped, the list is capped. */
function parseCustoms(raw: unknown): CustomIntegration[] {
  if (!Array.isArray(raw)) return [];
  const out: CustomIntegration[] = [];
  for (const item of raw.slice(0, 50)) {
    if (typeof item !== "object" || item === null) continue;
    const o = item as Record<string, unknown>;
    const id = typeof o.id === "string" && o.id ? o.id : null;
    if (!id) continue;
    const headers: Record<string, string> = {};
    if (typeof o.headers === "object" && o.headers !== null) {
      for (const [k, v] of Object.entries(o.headers as Record<string, unknown>)) {
        const key = k.trim();
        // Header values coerce from JSON scalars; anything else drops.
        const str =
          typeof v === "string" || typeof v === "number" || typeof v === "boolean"
            ? String(v)
            : undefined;
        if (key && str !== undefined) headers[key] = str;
      }
    }
    out.push({
      id,
      name: typeof o.name === "string" ? o.name.slice(0, 80) : "",
      kind: o.kind === "urlScheme" ? "urlScheme" : "webhook",
      enabled: o.enabled === true,
      method:
        typeof o.method === "string" &&
        (WEBHOOK_METHODS as readonly string[]).includes(o.method.toUpperCase())
          ? o.method.toUpperCase()
          : "POST",
      url: cleanStr(o.url),
      headers,
      body: typeof o.body === "string" ? o.body.slice(0, 100_000) : undefined,
      scheme: cleanStr(o.scheme),
    });
  }
  return out;
}

/** Parse the stored settings blob. Any corrupt or partial payload falls back
 *  field-by-field to defaults — a malformed blob must never crash Settings or
 *  the reader toolbar. */
export function parseIntegrations(raw: string | null): IntegrationSettings {
  const base = defaultIntegrations();
  if (!raw) return base;
  let o: unknown;
  try {
    o = JSON.parse(raw);
  } catch {
    return base;
  }
  if (typeof o !== "object" || o === null) return base;
  const obj = o as Record<string, unknown>;
  const builtins = {} as Record<BuiltinKey, BuiltinConfig>;
  for (const key of BUILTIN_KEYS) {
    builtins[key] = parseBuiltin(
      typeof obj.builtins === "object" && obj.builtins !== null
        ? (obj.builtins as Record<string, unknown>)[key]
        : undefined,
    );
  }
  return {
    useAiSummary: obj.useAiSummary === true,
    builtins,
    customs: parseCustoms(obj.customs),
  };
}

/** The credentials/URLs a built-in kind requires before it can run. Missing
 *  fields come back as undefined — `builtinReady` only checks presence. */
function builtinRequired(key: BuiltinKey, cfg: BuiltinConfig): (string | undefined)[] {
  switch (key) {
    case "cubox":
      return [cfg.apiUrl];
    case "readwise":
      return [cfg.token];
    case "instapaper":
      return [cfg.username, cfg.password];
    case "outline":
      return [cfg.endpoint, cfg.token, cfg.collection];
    case "readeck":
      return [cfg.endpoint, cfg.token];
    case "zotero":
      return [cfg.userId, cfg.token];
    case "obsidian":
      return [cfg.vaultPath];
  }
}

/** A built-in is usable when enabled and every required field is filled. */
export function builtinReady(key: BuiltinKey, cfg: BuiltinConfig): boolean {
  return cfg.enabled && builtinRequired(key, cfg).every((v) => !!v);
}

/** A custom integration is usable when enabled and its kind's template is
 *  complete (webhook needs method + URL; a scheme needs its scheme). */
export function customReady(c: CustomIntegration): boolean {
  if (!c.enabled) return false;
  return c.kind === "webhook"
    ? !!(c.method && c.url)
    : !!c.scheme && c.scheme.includes("://");
}

/** What the reader toolbar needs to render and run one integration. */
export interface ActiveIntegration {
  id: string;
  kind: IntegrationKind;
  /** Built-ins carry their i18n name key (localized at render time); customs
   *  carry the user's own label in `name`. */
  nameKey: string | null;
  name: string;
  icon: IconName;
  /** The config payload `run_integration` receives for this integration. */
  config: Record<string, unknown>;
}

/** Every enabled, fully-configured integration, built-ins first (in display
 *  order) then customs (in list order) — the toolbar renders this verbatim. */
export function activeIntegrations(settings: IntegrationSettings): ActiveIntegration[] {
  const out: ActiveIntegration[] = [];
  for (const key of BUILTIN_KEYS) {
    const cfg = settings.builtins[key];
    if (!builtinReady(key, cfg)) continue;
    const { enabled: _enabled, ...config } = cfg;
    const meta = BUILTIN_META[key];
    out.push({
      id: key,
      kind: key,
      nameKey: meta.nameKey,
      name: "",
      icon: meta.icon,
      config,
    });
  }
  for (const c of settings.customs) {
    if (!customReady(c)) continue;
    const config: Record<string, unknown> =
      c.kind === "webhook"
        ? { method: c.method, url: c.url, headers: c.headers ?? {}, body: c.body }
        : { scheme: c.scheme };
    out.push({
      id: c.id,
      kind: c.kind,
      nameKey: null,
      name: c.name,
      icon: c.kind === "webhook" ? "link" : "terminal",
      config,
    });
  }
  return out;
}

/** A fresh custom-integration id. */
export function newIntegrationId(): string {
  return typeof crypto !== "undefined" && crypto.randomUUID
    ? crypto.randomUUID()
    : `int-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}
