// Type mirrors of the Rust domain model (see src-tauri/src/models.rs).

export type SourceType =
  | "rss"
  | "youtube"
  | "podcast"
  | "mastodon"
  | "bluesky"
  | "reddit"
  | "newsletter"
  /** An X (Twitter) user timeline served by the built-in X parser. */
  | "x"
  /** An RSSHub route executed by the bundled local engine. */
  | "rsshub";

/** ── RSSHub route marketplace (mirrors rsshub::catalog + db) ──────────
 *  The wire shape of the bundled catalog snapshot: one entry per namespace,
 *  routes keyed by their canonical full path (`/bilibili/user/video/:id`). */
export interface RsshubConfigItem {
  name: string;
  optional: boolean;
  description?: string | null;
}
/** `false` (nothing required) or the list of required env vars. */
export type RsshubRequireConfig = false | RsshubConfigItem[];
export interface RsshubRouteFeatures {
  requireConfig: RsshubRequireConfig;
  requirePuppeteer: boolean;
  antiCrawler: boolean;
  supportBT: boolean;
  supportPodcast: boolean;
  supportScihub: boolean;
}
export interface RsshubRoute {
  path: string;
  name: string;
  example?: string | null;
  parameters?: Record<string, string> | null;
  description?: string | null;
  features: RsshubRouteFeatures;
}
export interface RsshubNamespace {
  name: string;
  url?: string | null;
  description?: string | null;
  categories: string[];
  heat: number;
  routes: Record<string, RsshubRoute>;
}
export interface RsshubCatalog {
  generatedAt: string;
  namespaces: Record<string, RsshubNamespace>;
}
/** One installed route (mirrors db::RsshubRouteRow). `routePath` is the
 *  template; subscriptions instantiate it with concrete values. */
export interface RsshubRouteRow {
  routePath: string;
  enabled: boolean;
  /** JSON object of string environment variables. */
  envJson: string;
  installedAt: string;
}
/** What engine_status reports (mirrors rsshub::engine::EngineStatus). */
export interface RsshubEngineInfo {
  version: string;
  /** "standard" (Node + RSSHub only) or "browser" (also ships a headless
   *  Chromium for requirePuppeteer routes). Empty on pre-flavor engines. */
  flavor: string;
  rsshubRevision: string;
  nodeVersion: string;
  installedAt: string;
}
export interface RsshubEngineStatus {
  installed: boolean;
  running: boolean;
  port: number | null;
  info: RsshubEngineInfo | null;
}

/** A configured email-newsletter source (mirrors commands::NewsletterSource). */
export interface NewsletterSource {
  feedId: number;
  title: string;
  host: string;
  port: number;
  username: string;
  folder: string;
}

/** Payload for add_newsletter_source (mirrors commands::NewsletterInput). */
export interface NewsletterInput {
  title: string | null;
  host: string;
  port: number;
  username: string;
  password: string;
  folder: string;
}

/** Result of the settings "test connection" probe for one parser. */
export interface ParserTestResult {
  articleCount: number;
}

/** A feed-discovery result (mirrors discovery::DiscoveryResult). */
export interface DiscoveryResult {
  title: string;
  feedUrl: string;
  siteUrl: string | null;
  category: string | null;
  description: string | null;
  /** true → curated directory entry, false → live page scrape. */
  fromDirectory: boolean;
}

export interface Folder {
  id: number;
  name: string;
  position: number;
}

export interface Feed {
  id: number;
  feedUrl: string;
  siteUrl: string | null;
  title: string;
  description: string | null;
  faviconUrl: string | null;
  folderId: number | null;
  sourceType: SourceType;
  lastFetchedAt: string | null;
  fetchError: string | null;
  unreadCount: number;
  /** Per-feed refresh interval in minutes. `null` follows the global
   *  setting; the `525600` sentinel means "never". */
  refreshIntervalMin: number | null;
  /** When true, opening an article from this feed auto-translates it into the
   *  configured target language. Defaults to false (show the original). */
  autoTranslate: boolean;
  /** How articles from this feed open: reader view, auto-extracted full text,
   *  or the embedded web view of the original page. `null` follows the default
   *  behaviour (reader view, honouring the global auto-extract preference). */
  openMode: "reader" | "extracted" | "web" | null;
}

export interface Enclosure {
  url: string;
  mimeType: string | null;
  length: number | null;
}

export interface Tag {
  id: number;
  name: string;
  color: string;
  position: number;
  articleCount: number;
}

export type RuleField = "title" | "author" | "content" | "any";
export type RuleAction = "skip" | "read" | "star";

/** Dry-run result for a draft filter rule (see preview_rule command). */
export interface RulePreview {
  count: number;
  samples: string[];
}

export interface Rule {
  id: number;
  name: string;
  enabled: boolean;
  feedId: number | null;
  field: RuleField;
  query: string;
  action: RuleAction;
  position: number;
}

export interface ArticlePreviewTranslation {
  articleId: number;
  title: string;
  snippet: string;
  lang: string;
  engine: string;
}

export interface ArticleSummary {
  id: number;
  feedId: number;
  feedTitle: string;
  sourceType: SourceType;
  title: string;
  author: string | null;
  snippet: string | null;
  imageUrl: string | null;
  url: string | null;
  publishedAt: string | null;
  isRead: boolean;
  isStarred: boolean;
  readLater: boolean;
}

export interface ArticleDetail {
  id: number;
  feedId: number;
  feedTitle: string;
  sourceType: SourceType;
  title: string;
  author: string | null;
  url: string | null;
  contentHtml: string | null;
  extractedHtml: string | null;
  imageUrl: string | null;
  publishedAt: string | null;
  isRead: boolean;
  isStarred: boolean;
  readLater: boolean;
  aiSummary: string | null;
  /** Cached translated body HTML, if a translation has been generated. */
  translatedHtml: string | null;
  /** The target language code the cached translation was produced for. */
  translatedLang: string | null;
  /** Cached translated title, produced by the same translation run as
   *  `translatedHtml`. `null` on rows cached before title translation. */
  translatedTitle: string | null;
  enclosures: Enclosure[];
  tags: Tag[];
}

export interface SmartCounts {
  unread: number;
  starred: number;
  readLater: number;
}

/** A user highlight / annotation (mirrors models::Highlight). */
export interface Highlight {
  id: number;
  articleId: number;
  quote: string;
  prefix: string;
  suffix: string;
  textOffset: number;
  color: string;
  note: string;
  createdAt: string;
}

// Mirrors the adjacently-tagged Rust `ArticleQuery` enum.
export type ArticleQuery =
  | { kind: "all" }
  | { kind: "unread" }
  | { kind: "starred" }
  | { kind: "readLater" }
  | { kind: "feed"; value: number }
  | { kind: "folder"; value: number }
  | { kind: "tag"; value: number };

export type AiEvent =
  | { type: "delta"; data: string }
  | { type: "done" }
  | { type: "error"; data: string };

/** Batch-level translation progress (mirrors commands::TranslateEvent). */
export type TranslateEvent =
  | { type: "start"; data: { total: number } }
  | { type: "title"; data: { title: string } }
  | { type: "batch"; data: { html: string; done: number } }
  | { type: "done"; data: { html: string } };

export type RefreshProgress =
  | { event: "started"; data: { total: number } }
  | {
      event: "feedDone";
      data: { feedId: number; newArticles: number; error: string | null };
    }
  | { event: "finished"; data: { newArticles: number } };
