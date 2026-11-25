//! Domain types shared between the database layer, ingestion, and the frontend.
//! All structs use camelCase when serialized so the React side stays idiomatic.

use serde::{Deserialize, Serialize};

/// The kind of source a feed represents. Drives differentiated rendering in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    Rss,
    Youtube,
    Podcast,
    Mastodon,
    Bluesky,
    /// A subreddit consumed via Reddit's `.rss` endpoint.
    Reddit,
    /// An email newsletter polled from an IMAP mailbox (see ingestion::newsletter).
    Newsletter,
    /// An X (Twitter) user timeline fetched by the built-in X parser (see
    /// `ingestion::parsers::x`) instead of a feed document. The feed's URL is
    /// the profile URL itself; authentication lives in the `parsers` setting.
    X,
    /// A Xiaohongshu (RED) blogger profile fetched by the built-in parser (see
    /// `ingestion::parsers::xiaohongshu`). Works anonymously from the SSR
    /// profile page; an optional session cookie upgrades articles to full
    /// note detail. The feed's URL is the profile URL itself.
    Xiaohongshu,
    /// A Kuaishou creator profile fetched by the built-in parser (see
    /// `ingestion::parsers::kuaishou`). The web API is captcha-gated for
    /// anonymous traffic, so a pasted login cookie is effectively required.
    Kuaishou,
    /// An RSSHub route executed by the bundled local engine (see
    /// `ingestion::rsshub`). The feed's URL is the `rsshub://<route path>`
    /// short link the user subscribed with; which routes are installed and
    /// their environment variables live in the `rsshub_routes` table.
    Rsshub,
}

impl SourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceType::Rss => "rss",
            SourceType::Youtube => "youtube",
            SourceType::Podcast => "podcast",
            SourceType::Mastodon => "mastodon",
            SourceType::Bluesky => "bluesky",
            SourceType::Reddit => "reddit",
            SourceType::Newsletter => "newsletter",
            SourceType::X => "x",
            SourceType::Xiaohongshu => "xiaohongshu",
            SourceType::Kuaishou => "kuaishou",
            SourceType::Rsshub => "rsshub",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: i64,
    pub name: String,
    pub position: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Feed {
    pub id: i64,
    pub feed_url: String,
    pub site_url: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub favicon_url: Option<String>,
    pub folder_id: Option<i64>,
    pub source_type: String,
    pub last_fetched_at: Option<String>,
    pub fetch_error: Option<String>,
    pub unread_count: i64,
    /// Per-feed refresh interval in minutes. `None` follows the global
    /// `refresh_interval_min` setting; the `525_600` sentinel means "never".
    pub refresh_interval_min: Option<i64>,
    /// When `true`, opening an article from this feed automatically starts a
    /// translation into the configured target language. Off by default, so
    /// existing feeds keep showing the original text.
    pub auto_translate: bool,
    /// How articles from this feed open in the reader pane: `"reader"` (RSS
    /// content), `"extracted"` (auto-run full-text extraction) or `"web"`
    /// (embedded web view of the original page). `None` follows the default
    /// behaviour (reader view, honouring the global auto-extract preference).
    pub open_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Enclosure {
    pub url: String,
    pub mime_type: Option<String>,
    pub length: Option<i64>,
}

/// A user-defined label that can be attached to any number of articles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub id: i64,
    pub name: String,
    /// A palette key (resolved to a colour by the frontend).
    pub color: String,
    pub position: i64,
    /// How many articles currently carry this tag.
    pub article_count: i64,
}

/// One matching condition inside a rule's When-document: `value` tested
/// against one article `field` with one `operator` (see [`RULE_FIELDS`] /
/// [`RULE_OPERATORS`]). Stored as JSON — strings, not enums, so a hand-edited
/// database row with an unknown value degrades to "never matches" instead of
/// failing to load; writes are validated against the constants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleCondition {
    pub field: String,
    pub operator: String,
    pub value: String,
}

/// A literal `from` → `to` replacement applied to an article's text by a rule
/// with the `rewrite` action. Deliberately *literal* (no regex): the Rust
/// ingestion path and the SQL backfill path must replace identically, and
/// SQLite's `REPLACE()` is literal — regex replacement would need two
/// implementations kept in lockstep for no real gain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleRewrite {
    pub from: String,
    pub to: String,
}

/// A user-defined filter rule. Ingestion-time actions apply the moment an
/// article lands; open-time actions take effect when a matching article is
/// opened in the reader (see [`RULE_ACTIONS`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    /// `None` applies the rule to every feed; otherwise scoped to one feed.
    pub feed_id: Option<i64>,
    /// The When-document: groups of [`RuleCondition`]s, ANDed within a group,
    /// ORed across groups — the rule fires when *every* condition of *any one*
    /// group matches. Legacy rules (v≤23) had no stored document; `row_to_rule`
    /// synthesises one from their `field`/`query` keyword columns.
    pub conditions: Vec<Vec<RuleCondition>>,
    /// What to do on a match: an ingestion-time action (`skip` | `read` |
    /// `star` | `rewrite`) or an open-time one (`translate` | `summarize` |
    /// `fulltext` | `open_original`).
    pub action: String,
    /// Literal from→to replacements applied when `action` is `rewrite`.
    pub rewrites: Vec<RuleRewrite>,
    pub position: i64,
}

/// The article fields a [`RuleCondition`] can test.
pub const RULE_FIELDS: &[&str] = &["title", "author", "content", "any", "url"];

/// The operators a [`RuleCondition`] can use. `contains` / `eq` /
/// `not_contains` / `not_eq` fold case (Unicode-aware, like ingestion's
/// `to_lowercase()`); `regex` matches the raw text — case-insensitivity is the
/// pattern's own `(?i)` flag, since lowercasing a pattern would rewrite
/// classes like `\D`.
pub const RULE_OPERATORS: &[&str] =
    &["contains", "not_contains", "eq", "not_eq", "regex"];

/// What a rule can do when it fires. Two timing classes: the ingestion-time
/// actions (`skip` drops the article, `read` / `star` pre-set its flags,
/// `rewrite` replaces text) apply once, as the article lands; the open-time
/// actions (`translate` / `summarize` / `fulltext` / `open_original`) are
/// reader behaviours that take effect when a matching article is opened —
/// see [`RULE_OPEN_ACTIONS`]. Open-time actions need no backfill: evaluating
/// the stored rules against the row at open time (SQL side:
/// `rule_match_where`) covers existing articles automatically, and nothing is
/// spent (network / LLM calls) until the reader actually opens the article.
pub const RULE_ACTIONS: &[&str] = &[
    "skip",
    "read",
    "star",
    "rewrite",
    "translate",
    "summarize",
    "fulltext",
    "open_original",
];

/// The open-time subset of [`RULE_ACTIONS`]: the actions
/// `db::rule_actions_for_article` collects when the reader opens an article.
/// Ingestion-time actions are not listed here — by the time a row exists they
/// have already acted (or dropped it), so there is nothing left to re-derive.
pub const RULE_OPEN_ACTIONS: &[&str] = &["translate", "summarize", "fulltext", "open_original"];

impl RuleCondition {
    /// True when the condition is well-formed enough to save: a known field
    /// and operator, a non-empty value (an empty needle would either match
    /// everything — `contains` — or be meaningless), and, for `regex`, a
    /// pattern that compiles.
    pub fn is_valid(&self) -> bool {
        if !RULE_FIELDS.contains(&self.field.as_str())
            || !RULE_OPERATORS.contains(&self.operator.as_str())
            || self.value.trim().is_empty()
        {
            return false;
        }
        if self.operator == "regex" {
            return RuleCondition::compile(&self.value).is_some();
        }
        true
    }

    /// Compile `pattern`, memoised — the same patterns are re-tested for every
    /// incoming article, and the SQL `regexp()` helper shares this cache so
    /// both sides of the match see identical semantics.
    pub fn compile(pattern: &str) -> Option<regex::Regex> {
        use std::collections::HashMap;
        use std::sync::{LazyLock, Mutex};
        static CACHE: LazyLock<Mutex<HashMap<String, Option<regex::Regex>>>> =
            LazyLock::new(|| Mutex::new(HashMap::new()));
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        // The cache is keyed by raw pattern; rules carry a handful at most.
        // Preview typing can flood it with one-off patterns, so the cache is
        // capped — the cap is generous (1024) because a clear costs one
        // recompile per live rule pattern, and entries are tiny.
        if cache.len() > 1024 {
            cache.clear();
        }
        cache
            .entry(pattern.to_string())
            .or_insert_with(|| regex::Regex::new(pattern).ok())
            .clone()
    }
}

impl RuleRewrite {
    /// True when the replacement is saveable: a non-empty `from` (an empty
    /// needle would rewrite every character boundary).
    pub fn is_valid(&self) -> bool {
        !self.from.trim().is_empty()
    }
}

/// Validate a draft rule's When-document. Returns the first problem found as a
/// stable error code the frontend localises.
pub fn validate_conditions(
    conditions: &[Vec<RuleCondition>],
) -> Result<(), &'static str> {
    // At least one condition somewhere, and no group left empty: an empty
    // group is vacuously true to `all()`, so `[[valid], []]` would otherwise
    // match *everything* at ingestion while the SQL side (which skips empty
    // groups) previews a far smaller set.
    if conditions.iter().any(|g| g.is_empty()) || conditions.is_empty() {
        return Err("emptyRuleCondition");
    }
    if conditions.len() > 32 || conditions.iter().any(|g| g.len() > 32) {
        return Err("tooManyRuleConditions");
    }
    for group in conditions {
        for c in group {
            if !c.is_valid() {
                return Err("badRuleCondition");
            }
        }
    }
    Ok(())
}

/// Validate a draft rule's full Then-parts (action + rewrites) on top of its
/// When-document. Open-time actions carry no Then-extras — their effect lives
/// in the action name itself, resolved against the reader at open time.
pub fn validate_rule_parts(
    conditions: &[Vec<RuleCondition>],
    action: &str,
    rewrites: &[RuleRewrite],
) -> Result<(), &'static str> {
    validate_conditions(conditions)?;
    if !RULE_ACTIONS.contains(&action) {
        return Err("badRuleAction");
    }
    if action == "rewrite" && rewrites.is_empty() {
        return Err("needRuleRewrite");
    }
    if rewrites.len() > 32 {
        return Err("tooManyRuleRewrites");
    }
    if rewrites.iter().any(|r| !r.is_valid()) {
        return Err("emptyRuleRewrite");
    }
    Ok(())
}

/// A row in the article list pane. Keeps the payload small (no full HTML body).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArticleSummary {
    pub id: i64,
    pub feed_id: i64,
    pub feed_title: String,
    pub source_type: String,
    pub title: String,
    pub author: Option<String>,
    pub snippet: Option<String>,
    pub image_url: Option<String>,
    pub url: Option<String>,
    pub published_at: Option<String>,
    pub is_read: bool,
    pub is_starred: bool,
    pub read_later: bool,
}

/// The full article shown in the reading pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArticleDetail {
    pub id: i64,
    pub feed_id: i64,
    pub feed_title: String,
    pub source_type: String,
    pub title: String,
    pub author: Option<String>,
    pub url: Option<String>,
    /// Sanitized HTML from the feed itself.
    pub content_html: Option<String>,
    /// Sanitized HTML from full-text extraction (dom_smoothie), if performed.
    pub extracted_html: Option<String>,
    pub image_url: Option<String>,
    pub published_at: Option<String>,
    pub is_read: bool,
    pub is_starred: bool,
    pub read_later: bool,
    pub ai_summary: Option<String>,
    /// Cached translated body HTML, if a translation has been generated.
    pub translated_html: Option<String>,
    /// The target language code the cached translation was produced for.
    pub translated_lang: Option<String>,
    /// Cached translated title, produced by the same translation run as
    /// `translated_html`. `None` on rows cached before title translation —
    /// the reader then keeps the original title.
    pub translated_title: Option<String>,
    pub enclosures: Vec<Enclosure>,
    /// Tags currently attached to this article.
    pub tags: Vec<Tag>,
}

/// A user highlight / annotation pinned to a span of an article's rendered
/// plain text (feature F7). `text_offset` plus `prefix` / `suffix` form a
/// resilient anchor: the offset is tried first, the context window second.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Highlight {
    pub id: i64,
    pub article_id: i64,
    /// The highlighted text itself.
    pub quote: String,
    /// A short window of text immediately before the quote (for re-anchoring).
    pub prefix: String,
    /// A short window of text immediately after the quote (for re-anchoring).
    pub suffix: String,
    /// Character offset of the quote within the article's plain-text render.
    pub text_offset: i64,
    /// Palette key resolved to a colour by the frontend.
    pub color: String,
    /// Optional user note; an empty string means no note.
    pub note: String,
    pub created_at: String,
}

/// Filters for the article list. Mirrors the sidebar selection in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum ArticleQuery {
    All,
    Unread,
    Starred,
    ReadLater,
    Feed(i64),
    Folder(i64),
    Tag(i64),
}

/// Live progress for a refresh run, streamed to the frontend over an ipc::Channel.
//
// `rename_all_fields` is required in addition to `rename_all`: the latter only
// camelCases the variant names, not the fields inside struct variants — so
// without it `feed_id` / `new_articles` would reach the frontend snake-cased,
// mismatching the camelCase RefreshProgress type in src/types.ts.
#[derive(Debug, Clone, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "event",
    content = "data"
)]
pub enum RefreshProgress {
    Started {
        total: usize,
    },
    FeedDone {
        feed_id: i64,
        new_articles: usize,
        error: Option<String>,
    },
    Finished {
        new_articles: usize,
    },
}

/// A persisted global AI chat session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatSession {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One persisted message in a global AI chat session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatMessageRow {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub contexts_json: Option<String>,
    pub tools_json: Option<String>,
    pub created_at: String,
}

/// Compact article hit returned to the AI assistant's read-only retrieval tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolArticleHit {
    pub id: i64,
    pub feed_id: i64,
    pub feed_title: String,
    pub title: String,
    pub author: Option<String>,
    pub url: Option<String>,
    pub published_at: Option<String>,
    pub is_read: bool,
    pub is_starred: bool,
    pub snippet: String,
}

/// Highlight + parent article info returned to the AI assistant's highlight search tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolHighlightHit {
    pub id: i64,
    pub article_id: i64,
    pub article_title: String,
    pub feed_title: String,
    pub quote: String,
    pub note: String,
    pub created_at: String,
}

/// A persisted AI daily/periodic briefing generated from recent articles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiBriefing {
    pub id: i64,
    pub title: String,
    pub scope_label: String,
    pub days: i64,
    pub article_count: i64,
    pub content: String,
    pub created_at: String,
}
