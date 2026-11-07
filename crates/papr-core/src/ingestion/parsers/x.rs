//! Built-in X (Twitter) parser: turns a user profile URL into feed data by
//! talking to x.com's GraphQL API directly — the site offers no RSS.
//!
//! Requires the user's own session cookies (`auth_token` + `ct0`, configured
//! in Settings → Parsers): the public web Bearer plus those cookies drive the
//! same `UserByScreenName` / `UserTweets` GraphQL operations the x.com web
//! client calls. There is deliberately no unauthenticated fallback — the
//! logged-out profile HTML exposes only a handful of tweets and no reliable
//! media attribution, so the parser refuses to run without cookies.
//!
//! Engineering notes ported from battle-tested usage of the same API:
//! - `queryId`s rotate with every x.com release. Resolution order: cached
//!   values → re-extract from the `main.<hash>.js` bundle referenced by the
//!   logged-in home page → built-in fallbacks (a stale id fails with a 404
//!   that triggers re-extraction and one retry).
//! - x.com's edge WAF fingerprints TLS stacks. Rust's `reqwest` passes today
//!   (verified 2026-11: `GET x.com` 200, guest activation 200), but an API
//!   403 is reported as a distinct "blocked" error so users can route
//!   through the app's network proxy setting.
//! - The `rest_id` (numeric user id) is stable per handle and cached; the
//!   handle itself is *not* (renames happen), so the cache is a best-effort
//!   accelerator only.
//!
//! Phase 1 always fetches page 1 (the latest ~20 tweets). The timeline's
//! pagination cursor is parsed but discarded: the periodic-refresh model
//! re-pulls the newest page each cycle and dedupes by tweet id, which needs
//! no cursor.

use crate::db;
use crate::error::AppError;
use chrono::DateTime;
use reqwest::{Client, StatusCode};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;
use url::Url;

/// Public web-client Bearer token. Public by design: it ships with the x.com
/// frontend to every visitor and carries no privileges; authenticating users
/// are identified by their cookies.
pub const X_BEARER_TOKEN: &str =
    "AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

/// Browser-like UA. x.com's API is pickier about this than most sites: the
/// generic papr feed UA works for HTML, but the GraphQL endpoint expects a
/// recognisable browser string.
pub const X_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

/// Tweets requested per UserTweets call (the web client's own page size).
pub const USER_TWEETS_PAGE_SIZE: i64 = 20;

/// Pause between per-handle fetches so a wall of X feeds never trips x.com's
/// per-account rate limits. Mirrors the throttle the reference implementation
/// settled on empirically.
pub const HANDLE_THROTTLE_MS: u64 = 500;

/// Coded errors the frontend localises under `error.<code>`. Coded errors
/// carry no detail payload, so each message must be self-contained.
pub const CODE_AUTH_REQUIRED: &str = "xAuthRequired";
pub const CODE_AUTH_INVALID: &str = "xAuthInvalid";
pub const CODE_ACCOUNT_UNAVAILABLE: &str = "xAccountUnavailable";
pub const CODE_BLOCKED: &str = "xBlocked";
pub const CODE_HANDLE_INVALID: &str = "xHandleInvalid";

/// Fallback GraphQL queryIds (verified 2026-09). Only used when the live
/// bundle extraction fails; stale ids surface as a 404 that re-extracts.
const FALLBACK_USER_TWEETS_ID: &str = "OeFjWKHutsuyWXZGmLr02A";
const FALLBACK_USER_BY_SCREEN_NAME_ID: &str = "KybxDj9RrADIITXlGG8kpw";

/// The `features` blob the web client sends with every UserTweets call. Its
/// exact contents must match what the frontend sends or the API rejects the
/// request, so it is kept as a verbatim object, serialised once.
static X_GRAPHQL_FEATURES: LazyLock<String> = LazyLock::new(|| {
    serde_json::json!({
        "rweb_tipjar_consumption_enabled": true,
        "responsive_web_graphql_exclude_directive_enabled": true,
        "verified_phone_label_enabled": false,
        "creator_subscriptions_tweet_preview_api_enabled": true,
        "responsive_web_graphql_timeline_navigation_enabled": true,
        "responsive_web_graphql_skip_user_profile_metadata_extensions_enabled": true,
        "communities_web_enable_tweet_community_results_fetch": true,
        "c9s_tweet_anatomy_moderator_badge_enabled": true,
        "articles_preview_enabled": true,
        "tweetypie_unmention_optimization_enabled": true,
        "responsive_web_edit_tweet_api_enabled": true,
        "graphql_is_translatable_rweb_tweet_is_translatable_enabled": true,
        "view_counts_everywhere_api_enabled": true,
        "longform_notetweets_consumption_enabled": true,
        "responsive_web_twitter_article_tweet_consumption_enabled": true,
        "tweet_awards_web_tipping_enabled": false,
        "creator_subscriptions_quote_tweet_preview_enabled": false,
        "freedom_of_speech_not_reach_fetch_enabled": true,
        "standardized_nudges_misinfo": true,
        "tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled": true,
        "rweb_video_timestamps_enabled": true,
        "longform_notetweets_rich_text_read_enabled": true,
        "longform_notetweets_inline_media_enabled": true,
    })
    .to_string()
});

/* ── errors ──────────────────────────────────────────────────────────── */

/// Everything that can go wrong while fetching one X timeline. The variants
/// that map to a stable, localisable code expose it; everything else is
/// rendered verbatim by the caller.
#[derive(Debug, thiserror::Error)]
pub enum XError {
    /// GraphQL error codes 32/239/89/326 or an HTTP 401 — the cookies are
    /// wrong or expired.
    #[error("auth invalid")]
    Auth,
    /// The account is gone, restricted or unavailable (`UserUnavailable`).
    #[error("account unavailable: {0}")]
    Unavailable(String),
    /// HTTP 403 — edge WAF challenge, IP reputation or account restriction.
    #[error("blocked")]
    Blocked,
    /// Any other non-2xx upstream status.
    #[error("upstream status {0}")]
    Upstream(u16),
    /// Network-layer failure (including a redirect off the allow-list).
    #[error("transport: {0}")]
    Transport(String),
    /// A response whose shape we could not walk.
    #[error("parse: {0}")]
    Parse(String),
}

impl XError {
    /// The frontend i18n key for this failure, when one exists.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            XError::Auth => Some(CODE_AUTH_INVALID),
            XError::Unavailable(_) => Some(CODE_ACCOUNT_UNAVAILABLE),
            XError::Blocked => Some(CODE_BLOCKED),
            XError::Upstream(_) | XError::Transport(_) | XError::Parse(_) => None,
        }
    }

    /// Convert into the shared `AppError`: coded errors keep their code (the
    /// frontend localises the whole sentence), the rest surface verbatim.
    pub fn into_app_error(self) -> AppError {
        match self.code() {
            Some(code) => AppError::code(code),
            None => AppError::other(self.to_string()),
        }
    }
}

impl From<reqwest::Error> for XError {
    fn from(e: reqwest::Error) -> Self {
        XError::Transport(e.to_string())
    }
}

impl From<XError> for AppError {
    fn from(e: XError) -> Self {
        e.into_app_error()
    }
}

pub type XResult<T> = Result<T, XError>;

/* ── URL / handle normalisation (pure) ───────────────────────────────── */

/// Path segments that are X pages, not user profiles — `x.com/home`,
/// `x.com/i/...`, `x.com/intent/...` and friends must never normalize to a
/// feed, however well-formed their names look.
const RESERVED_PATHS: &[&str] = &[
    "home",
    "explore",
    "notifications",
    "messages",
    "settings",
    "search",
    "i",
    "intent",
    "compose",
    "hashtag",
    "privacy",
    "tos",
    "login",
    "signup",
    "register",
    "welcome",
    "404",
];

/// True for a syntactically valid X handle: 1–15 ASCII letters, digits or
/// underscores (x.com's own rule).
pub fn is_valid_handle(handle: &str) -> bool {
    let len = handle.len();
    (1..=15).contains(&len)
        && handle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Normalize the host of an X-related URL down to `x.com` / `twitter.com`,
/// tolerating the `www.` / `mobile.` prefixes (pure helper).
fn is_x_host(host: &str) -> bool {
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("mobile."))
        .unwrap_or(host);
    host == "x.com" || host == "twitter.com"
}

/// Recognize a pasted X profile URL and rewrite it to the canonical feed URL
/// `https://x.com/<handle>` (handle lowercased — x.com handles are
/// case-insensitive, and one account must yield exactly one feed row).
/// Returns `None` for anything that is not a single-segment profile path.
///
/// Pure and unit-tested; the caller decides what to do with the result.
pub fn normalize_x_url(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_lowercase();
    if !is_x_host(&host) {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 1 {
        return None;
    }
    let handle = segments[0].to_lowercase();
    if RESERVED_PATHS.contains(&handle.as_str()) || !is_valid_handle(&handle) {
        return None;
    }
    Some(format!("https://x.com/{handle}"))
}

/// Recover the handle from a stored X feed URL (`https://x.com/<handle>`).
/// The inverse of [`normalize_x_url`] restricted to canonical URLs — used by
/// the refresh leg, which only ever sees what the add flow persisted.
pub fn handle_from_feed_url(feed_url: &str) -> Option<String> {
    let url = Url::parse(feed_url).ok()?;
    let host = url.host_str()?.to_lowercase();
    if host != "x.com" {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    // Exactly one segment: `/jack/status/1` is a permalink, not a profile.
    if segments.len() != 1 {
        return None;
    }
    let handle = segments[0];
    (is_valid_handle(handle) && !RESERVED_PATHS.contains(&handle)).then(|| handle.to_lowercase())
}

/// Accept free-form handle input from the settings test field: a bare handle,
/// an `@handle`, or a full profile URL. Returns the lowercased handle.
pub fn sanitize_handle_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if let Some(stripped) = trimmed.strip_prefix('@') {
        let handle = stripped.trim().to_lowercase();
        return is_valid_handle(&handle).then_some(handle);
    }
    if trimmed.contains('.') || trimmed.contains('/') {
        // Looks like a URL — let the normalizer find the handle in it.
        let with_scheme = crate::ingestion::discovery::normalize_query_url(trimmed);
        if let Ok(url) = Url::parse(&with_scheme) {
            return normalize_x_url(&url).and_then(|u| handle_from_feed_url(&u));
        }
    }
    let handle = trimmed.to_lowercase();
    is_valid_handle(&handle).then_some(handle)
}

/* ── GraphQL request builders (pure) ─────────────────────────────────── */

/// Cached GraphQL queryIds, persisted in the `parser.x.state` setting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueryIds {
    pub user_tweets: Option<String>,
    pub user_by_screen_name: Option<String>,
}

/// Parser runtime state persisted across refreshes under `parser.x.state`:
/// queryIds (rotated per x.com release) and the handle → rest_id map (the
/// UserByScreenName round-trip is one request per new subscription only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct XState {
    pub query_ids: QueryIds,
    pub user_ids: BTreeMap<String, String>,
}

impl XState {
    const SETTING_KEY: &'static str = "parser.x.state";

    /// Load persisted state. A missing or malformed blob degrades to a fresh
    /// state — the caches are accelerators, never correctness-critical.
    pub fn load(conn: &Connection) -> XState {
        db::get_setting(conn, Self::SETTING_KEY)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Persist the state back. Failures are swallowed: a lost cache costs one
    /// extra request on the next refresh, never data.
    pub fn save(&self, conn: &Connection) {
        if let Ok(json) = serde_json::to_string(self) {
            let _ = db::set_setting(conn, Self::SETTING_KEY, &json);
        }
    }
}

/// `UserByScreenName` request URL (handle → rest_id resolution).
pub fn build_user_by_screen_name_url(handle: &str, query_id: &str) -> String {
    let variables = serde_json::json!({ "screen_name": handle, "withGrokTranslatedBio": false });
    format!(
        "https://x.com/i/api/graphql/{query_id}/UserByScreenName?variables={}",
        encode_query_value(&variables.to_string())
    )
}

/// `UserTweets` request URL — always page 1 (latest page, no cursor).
pub fn build_user_tweets_url(user_id: &str, query_id: &str) -> String {
    let variables = serde_json::json!({
        "userId": user_id,
        "count": USER_TWEETS_PAGE_SIZE,
        "includePromotedContent": false,
        "withQuickPromoteEligibilityTweetFields": false,
        "withVoice": false,
    });
    format!(
        "https://x.com/i/api/graphql/{query_id}/UserTweets?variables={}&features={}",
        encode_query_value(&variables.to_string()),
        encode_query_value(&X_GRAPHQL_FEATURES),
    )
}

/// Percent-encode a query-string value (RFC 3986 unreserved set passes).
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/* ── parsed shapes ───────────────────────────────────────────────────── */

/// The user resolved via `UserByScreenName`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XUserProfile {
    // `display_name` is informational (retweets share the followed author's
    // name, so it must not become the article author); the title contract is
    // the handle.
    pub rest_id: String,
    pub display_name: String,
    pub bio: Option<String>,
    /// Avatar URL (the ~48px `_normal` variant — plenty for a favicon).
    pub avatar: Option<String>,
}

/// One tweet mapped for ingestion. `content_html` is already escaped and
/// entity-resolved (t.co short links → real anchors, media appended);
/// `text` is its plain-text form for summaries and full-text search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XTweet {
    /// Tweet snowflake id — the stable dedup key (`articles.guid`).
    pub id: String,
    pub handle: String,
    pub content_html: String,
    pub text: String,
    /// Permalink `https://x.com/<handle>/status/<id>`.
    pub url: String,
    pub published_at: Option<String>,
    /// First photo's CDN URL — the article-list thumbnail.
    pub image_url: Option<String>,
    /// Best-quality mp4 variant, when the tweet carries a video/GIF.
    pub video_url: Option<String>,
}

/// One user's timeline page: profile metadata plus the newest tweets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XUserPage {
    pub profile: XUserProfile,
    pub tweets: Vec<XTweet>,
}

/* ── dates ───────────────────────────────────────────────────────────── */

/// Twitter epoch: tweet snowflakes encode milliseconds since 2010-11-04.
const TWITTER_EPOCH_MS: i64 = 1_288_834_974_657;

/// Snowflake tweet id → RFC 3339 timestamp (the fallback when a tweet ships
/// no `created_at`, e.g. inside some promoted/reference shapes).
pub fn tweet_snowflake_to_rfc3339(tweet_id: &str) -> Option<String> {
    let id: i64 = tweet_id.parse().ok()?;
    if id <= 0 {
        return None;
    }
    let ms = (id >> 22) + TWITTER_EPOCH_MS;
    DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
}

/// X's `created_at` format (`Sun Sep 13 06:17:25 +0000 2026`) → RFC 3339.
pub fn tweet_created_at_to_rfc3339(value: &str) -> Option<String> {
    DateTime::parse_from_str(value, "%a %b %d %H:%M:%S %z %Y")
        .ok()
        .map(|t| t.to_rfc3339())
}

/* ── tweet text / HTML rendering (pure) ──────────────────────────────── */

fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A text-range entity in `full_text`. `href: Some(..)` renders an anchor
/// over the range; `None` drops the range (media t.co placeholders vanish
/// from the text — the media itself renders below).
struct TextEntity {
    start: usize,
    end: usize,
    href: Option<String>,
}

/// Convert UTF-16 code-unit offsets (the API's `indices` contract) to byte
/// offsets. An offset that lands inside an astral char (emoji) maps to that
/// char's start boundary; an offset at/past the text's end maps to the text
/// end (the terminal entry), so an over-long entity range clamps instead of
/// leaking the last character past the cut.
fn utf16_boundaries(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(text.len() + 1);
    let mut utf16 = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        out.push((utf16, byte_idx));
        utf16 += ch.len_utf16();
    }
    out.push((utf16, text.len()));
    out
}

fn utf16_to_byte(boundaries: &[(usize, usize)], offset: usize) -> Option<usize> {
    boundaries
        .iter()
        .rev()
        .find(|(u, _)| *u <= offset)
        .map(|(_, b)| *b)
}

/// Split `full_text` on `entities` (sorted by start, UTF-16 indices) into
/// segments. `render` turns each entity into its replacement fragment.
fn split_on_entities(
    full_text: &str,
    entities: &[TextEntity],
    render: impl Fn(&TextEntity, &str) -> String,
) -> String {
    let boundaries = utf16_boundaries(full_text);
    let mut sorted: Vec<&TextEntity> = entities.iter().collect();
    sorted.sort_by_key(|e| e.start);

    let mut parts: Vec<String> = Vec::new();
    let mut pos = 0usize; // byte offset
    for entity in sorted {
        let (Some(start), Some(end)) = (
            utf16_to_byte(&boundaries, entity.start),
            utf16_to_byte(&boundaries, entity.end),
        ) else {
            continue;
        };
        if start < pos || start >= end || end > full_text.len() {
            continue;
        }
        parts.push(escape_html_text(&full_text[pos..start]));
        parts.push(render(entity, &full_text[start..end]));
        pos = end;
    }
    parts.push(escape_html_text(&full_text[pos..]));
    parts.join("")
}

/// Build tweet HTML: `full_text` with t.co links resolved to their
/// `expanded_url`, media rendered below the text, newlines → `<br/>`.
/// Output still passes through `sanitize` at ingestion — this only escapes.
fn render_tweet_html(full_text: &str, entities: &[TextEntity], media: &[XMedia]) -> String {
    let mut html = split_on_entities(full_text, entities, |entity, segment| match &entity.href {
        Some(href) => format!(
            "<a href=\"{}\">{}</a>",
            escape_html_text(href),
            escape_html_text(segment)
        ),
        None => String::new(),
    })
    .replace('\n', "<br/>");

    for m in media {
        html.push_str("<br/>");
        let img = |url: &str| {
            format!(
                "<img src=\"{}\" loading=\"lazy\" alt=\"\">",
                escape_html_text(url)
            )
        };
        if m.kind == "photo" {
            if let Some(url) = &m.image_url {
                html.push_str(&img(url));
                continue;
            }
        }
        if let Some(url) = &m.video_url {
            html.push_str(&format!(
                "<video src=\"{}\" playsinline loop muted preload=\"none\"></video>",
                escape_html_text(url)
            ));
        } else if let Some(url) = &m.image_url {
            html.push_str(&img(url));
        }
    }
    html
}

/// The plain-text form of a tweet: expanded URLs in place of t.co, media
/// placeholders dropped. Feeds summaries and the full-text index. Trailing
/// whitespace collapses away — a media placeholder always trails a space in
/// `full_text`, and the summary should not end with one.
fn tweet_plain_text(full_text: &str, entities: &[TextEntity]) -> String {
    split_on_entities(full_text, entities, |entity, _segment| {
        entity.href.clone().unwrap_or_default()
    })
    .trim_end()
    .to_string()
}

/* ── response parsing (pure) ─────────────────────────────────────────── */

/// One resolved media attachment from `extended_entities.media`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct XMedia {
    kind: String,
    /// Photo CDN URL (`media_url_https`) — also the thumbnail candidate.
    image_url: Option<String>,
    /// Best mp4 variant for videos / animated GIFs.
    video_url: Option<String>,
}

/// Map GraphQL `errors` entries onto the error taxonomy. Codes 32/239/89/326
/// all mean "the session is no longer valid"; anything else is verbatim.
fn error_from_graphql(errors: &[serde_json::Value]) -> Option<XError> {
    let first = errors.first()?;
    match first.get("code").and_then(|c| c.as_i64()) {
        Some(32) | Some(239) | Some(89) | Some(326) => Some(XError::Auth),
        // 50 = "User not found" — the most common user error (a typo'd
        // handle); it must read as "account unavailable", not raw English.
        Some(50) => Some(XError::Unavailable("user not found".into())),
        _ => Some(XError::Parse(format!(
            "graphql: {}",
            first
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
        ))),
    }
}

/// Reject a GraphQL envelope that carries errors without any data.
fn check_graphql_errors(data: &serde_json::Value, operation: &str) -> XResult<()> {
    let errors = data
        .get("errors")
        .and_then(|e| e.as_array())
        .filter(|a| !a.is_empty());
    let no_data = data.get("data").map(|d| d.is_null()).unwrap_or(true);
    if let (Some(errors), true) = (errors, no_data) {
        return Err(error_from_graphql(errors)
            .unwrap_or_else(|| XError::Parse(format!("graphql: unknown {operation} error"))));
    }
    Ok(())
}

/// Parse a `UserByScreenName` response into the profile.
pub fn parse_user_by_screen_name(body: &str) -> XResult<XUserProfile> {
    let data: serde_json::Value =
        serde_json::from_str(body).map_err(|e| XError::Parse(e.to_string()))?;
    check_graphql_errors(&data, "UserByScreenName")?;
    // A missing user (typo'd handle, suspended account) is the "account
    // unavailable" case, not a shape failure — the reference implementation
    // maps both onto the same user-facing error.
    let result = data
        .pointer("/data/user/result")
        .ok_or_else(|| XError::Unavailable("data.user.result missing".into()))?;
    if result.get("__typename").and_then(|t| t.as_str()) == Some("UserUnavailable") {
        return Err(XError::Unavailable("user unavailable".into()));
    }
    let rest_id = result
        .get("rest_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| XError::Parse("UserByScreenName: missing rest_id".into()))?;
    let display_name = result
        .pointer("/core/name")
        .or_else(|| result.pointer("/legacy/name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("user")
        .to_string();
    let bio = result
        .pointer("/profile_bio/description")
        .or_else(|| result.pointer("/legacy/description"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let avatar = result
        .pointer("/avatar/image_url")
        .or_else(|| result.pointer("/legacy/profile_image_url_https"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(XUserProfile {
        rest_id: rest_id.to_string(),
        display_name,
        bio,
        avatar,
    })
}

/// Pick the highest-bitrate mp4 variant from a `video_info.variants` array.
fn best_video_variant(variants: &serde_json::Value) -> Option<String> {
    let mut best: Option<(i64, String)> = None;
    for v in variants.as_array()?.iter() {
        let content_type = v.get("content_type").and_then(|c| c.as_str()).unwrap_or("");
        if content_type != "video/mp4" {
            continue;
        }
        let url = v.get("url").and_then(|u| u.as_str())?;
        let bitrate = v.get("bitrate").and_then(|b| b.as_i64()).unwrap_or(0);
        if best.as_ref().is_none_or(|(b, _)| bitrate > *b) {
            best = Some((bitrate, url.to_string()));
        }
    }
    best.map(|(_, url)| url)
}

/// Extract media attachments from the tweet's `extended_entities` (falling
/// back to `entities` — photos live in both, videos only in the former).
fn extract_media(legacy: &serde_json::Value) -> Vec<XMedia> {
    let entities = legacy
        .pointer("/extended_entities/media")
        .or_else(|| legacy.pointer("/entities/media"))
        .and_then(|m| m.as_array());
    let mut out = Vec::new();
    for media in entities.into_iter().flatten() {
        let kind = media
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let image_url = media
            .get("media_url_https")
            .and_then(|u| u.as_str())
            .map(str::to_string);
        let video_url = media
            .get("video_info")
            .and_then(|v| v.get("variants"))
            .and_then(best_video_variant);
        if image_url.is_some() || video_url.is_some() {
            out.push(XMedia {
                kind,
                image_url,
                video_url,
            });
        }
    }
    out
}

/// Build the text-range entities for one tweet: `urls` become anchors to
/// their `expanded_url`, media t.co placeholders become dropped ranges.
fn tweet_text_entities(legacy: &serde_json::Value) -> Vec<TextEntity> {
    let mut entities = Vec::new();
    if let Some(urls) = legacy.pointer("/entities/urls").and_then(|u| u.as_array()) {
        for url in urls {
            let Some(indices) = url.get("indices").and_then(|i| i.as_array()) else {
                continue;
            };
            let (Some(start), Some(end)) = (
                indices.first().and_then(|v| v.as_u64()),
                indices.get(1).and_then(|v| v.as_u64()),
            ) else {
                continue;
            };
            entities.push(TextEntity {
                start: start.min(u64::from(u32::MAX)) as usize,
                end: end.min(u64::from(u32::MAX)) as usize,
                href: url
                    .get("expanded_url")
                    .or_else(|| url.get("url"))
                    .and_then(|u| u.as_str())
                    .map(str::to_string),
            });
        }
    }
    // Media placeholders: the t.co link that stands for the attachment in
    // `full_text`. Dropped from the text; the media renders below instead.
    if let Some(media) = legacy
        .pointer("/extended_entities/media")
        .or_else(|| legacy.pointer("/entities/media"))
        .and_then(|m| m.as_array())
    {
        for m in media {
            let Some(indices) = m.get("indices").and_then(|i| i.as_array()) else {
                continue;
            };
            let (Some(start), Some(end)) = (
                indices.first().and_then(|v| v.as_u64()),
                indices.get(1).and_then(|v| v.as_u64()),
            ) else {
                continue;
            };
            entities.push(TextEntity {
                start: start.min(u64::from(u32::MAX)) as usize,
                end: end.min(u64::from(u32::MAX)) as usize,
                href: None,
            });
        }
    }
    entities
}

/// Map one timeline entry's tweet (`result` value) to an [`XTweet`], or
/// `None` when the entry is a tombstone / reference without full text.
fn tweet_from_result(result: &serde_json::Value, handle: &str, entry_id: &str) -> Option<XTweet> {
    if result.get("__typename").and_then(|t| t.as_str()) == Some("TweetTombstone") {
        return None;
    }
    // Quoted/retweeted shapes nest the real tweet under `tweet`.
    let legacy = result
        .get("legacy")
        .or_else(|| result.pointer("/tweet/legacy"))?;
    let tweet_id = result
        .get("rest_id")
        .or_else(|| result.pointer("/tweet/rest_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| entry_id.trim_start_matches("tweet-").to_string());
    let Some(full_text) = legacy.get("full_text").and_then(|t| t.as_str()) else {
        return None;
    };

    let media = extract_media(legacy);
    let text_entities = tweet_text_entities(legacy);
    let content_html = render_tweet_html(full_text, &text_entities, &media);
    let text = tweet_plain_text(full_text, &text_entities);
    let published_at = legacy
        .get("created_at")
        .and_then(|c| c.as_str())
        .and_then(tweet_created_at_to_rfc3339)
        .or_else(|| tweet_snowflake_to_rfc3339(&tweet_id));
    let image_url = media.iter().find_map(|m| m.image_url.clone());
    let video_url = media.iter().find_map(|m| m.video_url.clone());
    let url = format!("https://x.com/{handle}/status/{tweet_id}");

    Some(XTweet {
        id: tweet_id,
        handle: handle.to_string(),
        content_html,
        text,
        url,
        published_at,
        image_url,
        video_url,
    })
}

/// Parse a `UserTweets` response body into the mapped tweet list. Retweets
/// are included and attributed to the followed author (their `full_text`
/// starts with `RT @…`); tombstoned and malformed entries are skipped rather
/// than failing the page. Only top-level timeline entries are consumed —
/// nested reference tweets belong to other authors. Long-form note tweets
/// render from the truncated `full_text` (same limitation as the reference
/// implementation; the full note body would need a separate TweetDetail
/// round-trip per tweet, not worth it for a page-1 refresh).
pub fn parse_user_tweets_json(body: &str, handle: &str) -> XResult<Vec<XTweet>> {
    let data: serde_json::Value =
        serde_json::from_str(body).map_err(|e| XError::Parse(e.to_string()))?;
    check_graphql_errors(&data, "UserTweets")?;
    let result = data
        .pointer("/data/user/result")
        .ok_or_else(|| XError::Unavailable("data.user.result missing".into()))?;
    if result.get("__typename").and_then(|t| t.as_str()) == Some("UserUnavailable") {
        return Err(XError::Unavailable("user unavailable".into()));
    }
    let instructions = result
        .pointer("/timeline_v2/timeline/instructions")
        .or_else(|| result.pointer("/timeline/timeline/instructions"))
        .and_then(|i| i.as_array())
        .ok_or_else(|| XError::Parse("UserTweets: missing timeline instructions".into()))?;

    let mut tweets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for instruction in instructions {
        if instruction.get("type").and_then(|t| t.as_str()) != Some("TimelineAddEntries") {
            continue;
        }
        let Some(entries) = instruction.get("entries").and_then(|e| e.as_array()) else {
            continue;
        };
        for entry in entries {
            let entry_id = entry.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
            if !entry_id.starts_with("tweet-") {
                continue;
            }
            let Some(tweet) = entry
                .pointer("/content/itemContent/tweet_results/result")
                .and_then(|r| tweet_from_result(r, handle, entry_id))
            else {
                continue;
            };
            // The same tweet can surface twice (pinned + timeline); the DB
            // dedupes, but keep the in-memory page honest too.
            if seen.insert(tweet.id.clone()) {
                tweets.push(tweet);
            }
        }
    }
    Ok(tweets)
}

/// Map one fetched tweet to an article row — the single definition shared by
/// the add flow, the refresh leg (and any future consumer). guid is the tweet
/// id (stable across fetches, so the (feed_id, guid) dedup makes refreshes
/// idempotent); the title is the user's handle — the parser's contract — and
/// media ride along in content_html with the first photo promoted to the
/// thumbnail and the best mp4 exposed as a video enclosure.
pub fn tweet_to_article(t: &XTweet) -> crate::db::NewArticle {
    // The HTML is built from escaped fragments above and re-sanitized here
    // with the permalink as base, so the ammonia allow-list has the final say
    // on every tag and URL before storage.
    let content_html = crate::sanitize::sanitize(&t.content_html, Some(&t.url));
    crate::db::NewArticle {
        guid: t.id.clone(),
        url: Some(t.url.clone()),
        title: t.handle.clone(),
        author: None,
        summary: Some(t.text.clone()),
        content_html: Some(content_html),
        body_text: t.text.clone(),
        image_url: t.image_url.clone(),
        published_at: t.published_at.clone(),
        enclosures: t
            .video_url
            .as_ref()
            .map(|url| {
                vec![crate::models::Enclosure {
                    url: url.clone(),
                    mime_type: Some("video/mp4".into()),
                    length: None,
                }]
            })
            .unwrap_or_default(),
    }
}

/* ── network transport ───────────────────────────────────────────────── */

/// Hosts the parser is allowed to talk to. Every response's final URL is
/// checked against this set so a redirect can never carry the session
/// cookies somewhere else (reqwest already strips sensitive headers on
/// cross-host redirects; this is the belt under those braces).
fn is_allowed_final_url(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("x.com") | Some("twitter.com") | Some("abs.twimg.com") | Some("api.x.com")
    )
}

/// GET a URL with explicit headers and the WAF-friendly browser UA, mapping
/// the upstream status onto the error taxonomy. Cookies travel only inside
/// these explicit headers — never via a cookie jar — so they cannot leak to
/// an unexpected host even if x.com redirected.
async fn get_text(client: &Client, url: &str, headers: &[(String, String)]) -> XResult<String> {
    let mut req = client.get(url);
    for (name, value) in headers {
        req = req.header(name, value);
    }
    let resp = req.send().await?;
    if !is_allowed_final_url(resp.url()) {
        return Err(XError::Transport(format!(
            "redirect to non-X host blocked: {}",
            resp.url()
        )));
    }
    match resp.status() {
        StatusCode::UNAUTHORIZED => return Err(XError::Auth),
        StatusCode::FORBIDDEN => return Err(XError::Blocked),
        s if !s.is_success() => return Err(XError::Upstream(s.as_u16())),
        _ => {}
    }
    // Cap the body like every other fetch in the repo (16 MiB): the main.js
    // bundle is multi-MB and read whole, and an unbounded response is an OOM
    // vector.
    let bytes = crate::ingestion::fetch::read_capped(resp)
        .await
        .map_err(|e| XError::Transport(e.to_string()))?;
    String::from_utf8(bytes).map_err(|_| XError::Parse("response is not UTF-8".into()))
}

/// Headers for the GraphQL API: Bearer + CSRF + session cookies.
fn api_headers(auth_token: &str, ct0: &str) -> Vec<(String, String)> {
    vec![
        ("User-Agent".into(), X_USER_AGENT.into()),
        ("Accept".into(), "*/*".into()),
        ("Authorization".into(), format!("Bearer {X_BEARER_TOKEN}")),
        ("X-CSRF-Token".into(), ct0.to_string()),
        ("X-Twitter-Auth-Type".into(), "OAuth2Session".into()),
        ("X-Twitter-Active-User".into(), "yes".into()),
        (
            "Cookie".into(),
            format!("auth_token={auth_token}; ct0={ct0}"),
        ),
    ]
}

/// Headers for the logged-in HTML home page: UA + cookies only — the API
/// headers (Bearer et al.) on an HTML URL earn a 401 from x.com.
fn html_headers(auth_token: &str, ct0: &str) -> Vec<(String, String)> {
    vec![
        ("User-Agent".into(), X_USER_AGENT.into()),
        ("Accept".into(), "text/html,application/xhtml+xml".into()),
        (
            "Cookie".into(),
            format!("auth_token={auth_token}; ct0={ct0}"),
        ),
    ]
}

/// True for URL-safe id characters (bundle hashes share the handle charset
/// but not its length limit, so they get their own predicate).
fn is_url_safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Locate the `main.<hash>.js` bundle URL inside the home-page HTML (pure).
pub fn find_main_bundle_url(home_html: &str) -> Option<String> {
    const MARKER: &str = "https://abs.twimg.com/responsive-web/client-web/main.";
    let start = home_html.find(MARKER)?;
    let rest = &home_html[start..];
    // The hash cannot contain '.', so the first ".js" ends the URL.
    let end = rest.find(".js")? + 3;
    let url = &rest[..end];
    let hash = &url[MARKER.len()..url.len() - 3];
    is_url_safe_id(hash).then(|| url.to_string())
}

/// Extract the two operation queryIds from a main-bundle body (pure).
pub fn extract_query_ids_from_bundle(bundle: &str) -> Option<QueryIds> {
    let mut ids = QueryIds {
        user_tweets: find_query_id_in_bundle(bundle, "UserTweets"),
        user_by_screen_name: find_query_id_in_bundle(bundle, "UserByScreenName"),
    };
    if ids.user_tweets.is_none() {
        ids.user_tweets = Some(FALLBACK_USER_TWEETS_ID.to_string());
    }
    if ids.user_by_screen_name.is_none() {
        ids.user_by_screen_name = Some(FALLBACK_USER_BY_SCREEN_NAME_ID.to_string());
    }
    Some(ids)
}

/// Find the queryId registered for `operation` in the minified bundle. The
/// bundle uses either `queryId:"…",operationName:"X"` or the reverse order;
/// both argument shapes are searched.
fn find_query_id_in_bundle(bundle: &str, operation: &str) -> Option<String> {
    let forward_suffix = format!("\",operationName:\"{operation}\"");
    if let Some(id) = find_between(bundle, "queryId:\"", &forward_suffix) {
        return Some(id);
    }
    // Reversed order: anchor at the operation name, then look ahead only
    // within the same registration object — crossing a brace would read the
    // next operation's queryId, and a deterministic wrong id fails forever
    // (404 → re-extract → same wrong id), so the brace guard matters.
    let anchor = format!("operationName:\"{operation}\"");
    let anchor_start = bundle.find(&anchor)?;
    let window = &bundle[anchor_start..(anchor_start + 400).min(bundle.len())];
    let window = match window.find(['{', '}']) {
        Some(brace) => &window[..brace],
        None => window,
    };
    find_between(window, "queryId:\"", "\"")
}

/// First `prefix<capture>suffix` occurrence where the capture is limited to
/// URL-safe id characters (pure helper).
fn find_between(haystack: &str, prefix: &str, suffix: &str) -> Option<String> {
    let start = haystack.find(prefix)? + prefix.len();
    let rest = &haystack[start..];
    let end = rest.find(suffix)?;
    let captured = &rest[..end];
    captured
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        .then(|| captured.to_string())
}

/// Re-fetch the queryId bundle: the logged-in home page references
/// `main.<hash>.js`, which embeds `queryId:"…",operationName:"…"` pairs.
pub async fn extract_query_ids(client: &Client, auth_token: &str, ct0: &str) -> XResult<QueryIds> {
    let home = get_text(client, "https://x.com/home", &html_headers(auth_token, ct0)).await?;
    let main_js_url = find_main_bundle_url(&home)
        .ok_or_else(|| XError::Parse("queryId: home page has no main bundle".into()))?;
    // The bundle lives on abs.twimg.com — never sent cookies.
    let main_js = get_text(
        client,
        &main_js_url,
        &[("User-Agent".into(), X_USER_AGENT.into())],
    )
    .await?;
    Ok(extract_query_ids_from_bundle(&main_js)
        .ok_or_else(|| XError::Parse("queryId: bundle carries no operations".into()))?)
}

/// Resolve queryIds: cache → live extraction (writes the cache) → fallback.
async fn resolve_query_ids(
    client: &Client,
    auth_token: &str,
    ct0: &str,
    state: &mut XState,
) -> QueryIds {
    if state.query_ids.user_tweets.is_some() && state.query_ids.user_by_screen_name.is_some() {
        return state.query_ids.clone();
    }
    match extract_query_ids(client, auth_token, ct0).await {
        Ok(ids) => {
            state.query_ids = ids.clone();
            ids
        }
        Err(e) => {
            log::warn!("x parser: queryId extraction failed, using fallbacks: {e}");
            // Cache the fallbacks too — they are constants, and without this
            // a flaky extraction re-fetches home + bundle for every feed and
            // for both operations of the same run.
            let fallback = QueryIds {
                user_tweets: Some(FALLBACK_USER_TWEETS_ID.to_string()),
                user_by_screen_name: Some(FALLBACK_USER_BY_SCREEN_NAME_ID.to_string()),
            };
            state.query_ids = fallback.clone();
            fallback
        }
    }
}

/// Resolve the profile for `handle`. The rest_id comes from the state cache
/// when present; the bio / avatar come fresh from `UserByScreenName` on every
/// call (one request — they ride along at no extra cost). A 404 means the
/// queryId went stale — invalidate and re-extract once before giving up.
async fn resolve_user_profile(
    client: &Client,
    auth_token: &str,
    ct0: &str,
    handle: &str,
    state: &mut XState,
) -> XResult<XUserProfile> {
    let ids = resolve_query_ids(client, auth_token, ct0, state).await;
    let query_id = ids
        .user_by_screen_name
        .clone()
        .ok_or_else(|| XError::Parse("queryId: no UserByScreenName id".into()))?;

    let mut body = get_text(
        client,
        &build_user_by_screen_name_url(handle, &query_id),
        &api_headers(auth_token, ct0),
    )
    .await;
    if matches!(&body, Err(XError::Upstream(404))) {
        state.query_ids.user_by_screen_name = None;
        let fresh = resolve_query_ids(client, auth_token, ct0, state).await;
        let fresh_id = fresh
            .user_by_screen_name
            .clone()
            .ok_or_else(|| XError::Parse("queryId: re-extraction found nothing".into()))?;
        body = get_text(
            client,
            &build_user_by_screen_name_url(handle, &fresh_id),
            &api_headers(auth_token, ct0),
        )
        .await;
    }
    let mut profile = parse_user_by_screen_name(&body?)?;
    match state.user_ids.get(handle) {
        // The cached id wins: handle→id is immutable, while a *response* id
        // could in principle be shadowed by a renamed handle's stale cache.
        Some(cached) => profile.rest_id = cached.clone(),
        None => {
            state
                .user_ids
                .insert(handle.to_string(), profile.rest_id.clone());
        }
    }
    Ok(profile)
}

/// Fetch one user's latest timeline page: profile + mapped tweets. This is
/// the parser's single entry point — the add flow, the refresh leg and the
/// settings "test connection" all funnel through it.
pub async fn fetch_user_tweets(
    client: &Client,
    auth_token: &str,
    ct0: &str,
    handle: &str,
    state: &mut XState,
) -> XResult<XUserPage> {
    let profile = resolve_user_profile(client, auth_token, ct0, handle, state).await?;
    let ids = resolve_query_ids(client, auth_token, ct0, state).await;
    let query_id = ids
        .user_tweets
        .clone()
        .ok_or_else(|| XError::Parse("queryId: no UserTweets id".into()))?;

    let mut body = get_text(
        client,
        &build_user_tweets_url(&profile.rest_id, &query_id),
        &api_headers(auth_token, ct0),
    )
    .await;
    if matches!(&body, Err(XError::Upstream(404))) {
        state.query_ids.user_tweets = None;
        let fresh = resolve_query_ids(client, auth_token, ct0, state).await;
        let fresh_id = fresh
            .user_tweets
            .clone()
            .ok_or_else(|| XError::Parse("queryId: re-extraction found nothing".into()))?;
        body = get_text(
            client,
            &build_user_tweets_url(&profile.rest_id, &fresh_id),
            &api_headers(auth_token, ct0),
        )
        .await;
    }
    let tweets = parse_user_tweets_json(&body?, handle)?;
    Ok(XUserPage { profile, tweets })
}

/* ── tests ───────────────────────────────────────────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_html_from(url: &str) -> Option<String> {
        // Mirror production: the scheme-less case is prepended by the caller
        // (`normalize_source` → `discovery::normalize_query_url`).
        let with_scheme = crate::ingestion::discovery::normalize_query_url(url);
        Url::parse(&with_scheme)
            .ok()
            .and_then(|u| normalize_x_url(&u))
    }

    #[test]
    fn profile_urls_normalize_to_canonical_feed_url() {
        assert_eq!(
            parse_html_from("https://x.com/jack").as_deref(),
            Some("https://x.com/jack")
        );
        // Case-insensitive: one account, one feed row.
        assert_eq!(
            parse_html_from("https://x.com/JACK").as_deref(),
            Some("https://x.com/jack")
        );
        // twitter.com and the www/mobile prefixes all map onto x.com.
        assert_eq!(
            parse_html_from("https://twitter.com/jack").as_deref(),
            Some("https://x.com/jack")
        );
        assert_eq!(
            parse_html_from("https://www.x.com/jack").as_deref(),
            Some("https://x.com/jack")
        );
        assert_eq!(
            parse_html_from("https://mobile.twitter.com/jack").as_deref(),
            Some("https://x.com/jack")
        );
        // Scheme-less and trailing-slash forms still normalize.
        assert_eq!(
            parse_html_from("x.com/jack").as_deref(),
            Some("https://x.com/jack")
        );
        assert_eq!(
            parse_html_from("https://x.com/jack/").as_deref(),
            Some("https://x.com/jack")
        );
    }

    #[test]
    fn non_profile_urls_do_not_normalize() {
        assert_eq!(parse_html_from("https://x.com/jack/status/123"), None);
        assert_eq!(parse_html_from("https://x.com/i/lists/1"), None);
        assert_eq!(
            parse_html_from("https://x.com/intent/follow?screen_name=x"),
            None
        );
        assert_eq!(parse_html_from("https://x.com/home"), None);
        assert_eq!(parse_html_from("https://x.com/explore"), None);
        assert_eq!(parse_html_from("https://x.com/"), None);
        assert_eq!(parse_html_from("https://example.com/jack"), None);
        assert_eq!(parse_html_from("https://x.com/jack/art"), None);
        // Not a syntactically valid handle.
        assert_eq!(parse_html_from("https://x.com/has%20space"), None);
        assert_eq!(
            parse_html_from("https://x.com/thishandleiswaytoolongforx"),
            None
        );
    }

    #[test]
    fn feed_urls_round_trip_to_handles() {
        assert_eq!(
            handle_from_feed_url("https://x.com/jack"),
            Some("jack".into())
        );
        assert_eq!(handle_from_feed_url("https://x.com/jack/status/1"), None);
        assert_eq!(handle_from_feed_url("https://twitter.com/jack"), None);
        assert_eq!(handle_from_feed_url("not a url"), None);
    }

    #[test]
    fn handle_input_accepts_bare_at_and_url_forms() {
        assert_eq!(sanitize_handle_input(" jack "), Some("jack".into()));
        assert_eq!(sanitize_handle_input("@Jack"), Some("jack".into()));
        assert_eq!(
            sanitize_handle_input("https://x.com/Jack/").as_deref(),
            Some("jack")
        );
        assert_eq!(sanitize_handle_input("x.com/jack"), Some("jack".into()));
        assert_eq!(
            sanitize_handle_input("https://x.com/jack?foo=bar"),
            Some("jack".into())
        );
        assert_eq!(sanitize_handle_input(""), None);
        assert_eq!(sanitize_handle_input("@has space"), None);
    }

    #[test]
    fn graphql_urls_embed_encoded_variables_and_features() {
        let url = build_user_tweets_url("44196397", "QID");
        assert!(url.starts_with("https://x.com/i/api/graphql/QID/UserTweets?"));
        assert!(url.contains("userId%22%3A%2244196397%22"));
        assert!(url.contains("count%22%3A20"));
        assert!(url.contains("&features="));
        assert!(url.contains("longform_notetweets_inline_media_enabled"));

        let url = build_user_by_screen_name_url("jack", "QID");
        assert!(url.starts_with("https://x.com/i/api/graphql/QID/UserByScreenName?"));
        assert!(url.contains("screen_name%22%3A%22jack%22"));
    }

    #[test]
    fn detect_source_type_classifies_profile_urls_as_parser_served() {
        use crate::ingestion::parse::detect_source_type;
        use crate::models::SourceType;
        assert_eq!(detect_source_type("https://x.com/jack"), SourceType::X);
        assert_eq!(
            detect_source_type("https://twitter.com/jack"),
            SourceType::X
        );
        // Permalinks are not profiles — they stay generic.
        assert_eq!(
            detect_source_type("https://x.com/jack/status/1"),
            SourceType::Rss
        );
    }

    #[test]
    fn main_bundle_url_is_extracted_from_home_html() {
        let html = r#"<script src="https://abs.twimg.com/responsive-web/client-web/main.aBc_-123.js" defer></script>"#;
        let url = find_main_bundle_url(html).unwrap();
        assert_eq!(
            url,
            "https://abs.twimg.com/responsive-web/client-web/main.aBc_-123.js"
        );
        assert_eq!(find_main_bundle_url("<script src=\"/no/bundle.js\">"), None);
    }

    #[test]
    fn query_ids_are_extracted_from_both_bundle_argument_orders() {
        let bundle = r#"x=queryId:"AAAA1111",operationName:"UserTweets",a=1;register({operationName:"UserByScreenName",queryId:"BBBB2222"})"#;
        let ids = extract_query_ids_from_bundle(bundle).unwrap();
        assert_eq!(ids.user_tweets.as_deref(), Some("AAAA1111"));
        assert_eq!(ids.user_by_screen_name.as_deref(), Some("BBBB2222"));
        // Missing operations fall back to the pinned ids.
        let ids = extract_query_ids_from_bundle("nothing here").unwrap();
        assert_eq!(ids.user_tweets.as_deref(), Some(FALLBACK_USER_TWEETS_ID));
        assert_eq!(
            ids.user_by_screen_name.as_deref(),
            Some(FALLBACK_USER_BY_SCREEN_NAME_ID)
        );
    }

    #[test]
    fn user_by_screen_name_parses_profile_metadata() {
        let body = r#"{"data":{"user":{"result":{"__typename":"User","rest_id":"44196397","core":{"name":"Elon Musk"},"profile_bio":{"description":"bio here"},"avatar":{"image_url":"https://pbs.twimg.com/profile_images/x_normal.jpg"}}}}}"#;
        let profile = parse_user_by_screen_name(body).unwrap();
        assert_eq!(profile.rest_id, "44196397");
        assert_eq!(profile.display_name, "Elon Musk");
        assert_eq!(profile.bio.as_deref(), Some("bio here"));
        assert_eq!(
            profile.avatar.as_deref(),
            Some("https://pbs.twimg.com/profile_images/x_normal.jpg")
        );
    }

    #[test]
    fn user_by_screen_name_maps_auth_and_availability_errors() {
        let auth = r#"{"errors":[{"code":32,"message":"Could not authenticate you."}]}"#;
        assert!(matches!(parse_user_by_screen_name(auth), Err(XError::Auth)));
        let gone = r#"{"data":{"user":{"result":{"__typename":"UserUnavailable"}}}}"#;
        assert!(matches!(
            parse_user_by_screen_name(gone),
            Err(XError::Unavailable(_))
        ));
    }

    #[test]
    fn dates_round_trip_from_both_sources() {
        assert_eq!(
            tweet_created_at_to_rfc3339("Sun Sep 13 06:17:25 +0000 2026").as_deref(),
            Some("2026-09-13T06:17:25+00:00")
        );
        assert_eq!(tweet_created_at_to_rfc3339("nonsense"), None);
        // 1758132000000000000 >> 22 + epoch — a known tweet id's timestamp.
        assert!(tweet_snowflake_to_rfc3339("1758132000000000000").is_some());
        assert_eq!(tweet_snowflake_to_rfc3339("not-a-number"), None);
        assert_eq!(tweet_snowflake_to_rfc3339("-5"), None);
    }

    /// One timeline with: a text+link tweet, a photo tweet, a video tweet
    /// (two mp4 variants — the high-bitrate one must win), a tombstone and
    /// a cursor entry. Generated as compact JSON to keep the braces honest.
    const USER_TWEETS_FIXTURE: &str = r#"{"data": {"user": {"result": {"__typename": "User", "core": {"name": "Display Name"}, "timeline_v2": {"timeline": {"instructions": [{"type": "TimelineAddEntries", "entries": [{"entryId": "tweet-111", "content": {"entryType": "TimelineTimelineItem", "itemContent": {"tweet_results": {"result": {"__typename": "Tweet", "rest_id": "111", "legacy": {"full_text": "hello https://t.co/abc world", "created_at": "Sun Sep 13 06:17:25 +0000 2026", "entities": {"urls": [{"url": "https://t.co/abc", "expanded_url": "https://example.com/page", "indices": [6, 22]}]}}}}}}}, {"entryId": "tweet-222", "content": {"entryType": "TimelineTimelineItem", "itemContent": {"tweet_results": {"result": {"__typename": "Tweet", "rest_id": "222", "legacy": {"full_text": "pic https://t.co/pic", "created_at": "Sun Sep 13 06:17:26 +0000 2026", "entities": {}, "extended_entities": {"media": [{"type": "photo", "media_url_https": "https://pbs.twimg.com/media/abc.jpg", "url": "https://t.co/pic", "indices": [4, 20]}]}}}}}}}, {"entryId": "tweet-333", "content": {"entryType": "TimelineTimelineItem", "itemContent": {"tweet_results": {"result": {"__typename": "Tweet", "rest_id": "333", "legacy": {"full_text": "watch https://t.co/vid", "created_at": "Sun Sep 13 06:17:27 +0000 2026", "entities": {}, "extended_entities": {"media": [{"type": "video", "media_url_https": "https://pbs.twimg.com/media/vid.jpg", "url": "https://t.co/vid", "indices": [6, 22], "video_info": {"variants": [{"content_type": "video/mp4", "bitrate": 632000, "url": "https://video.twimg.com/low.mp4"}, {"content_type": "video/mp4", "bitrate": 2176000, "url": "https://video.twimg.com/high.mp4"}, {"content_type": "application/x-mpegURL", "url": "https://video.twimg.com/playlist.m3u8"}]}}]}}}}}}}, {"entryId": "tweet-444", "content": {"entryType": "TimelineTimelineItem", "itemContent": {"tweet_results": {"result": {"__typename": "TweetTombstone", "tombstone": {}}}}}}, {"entryId": "cursor-bottom-0", "content": {"entryType": "TimelineTimelineCursor", "value": "the-cursor"}}]}]}}}}}}"#;

    #[test]
    fn user_tweets_parse_into_mapped_articles() {
        let tweets = parse_user_tweets_json(USER_TWEETS_FIXTURE, "jack").unwrap();
        assert_eq!(tweets.len(), 3);

        // Text tweet: the t.co link is rewritten to its expanded_url.
        let t = &tweets[0];
        assert_eq!(t.id, "111");
        assert_eq!(t.handle, "jack");
        assert_eq!(t.url, "https://x.com/jack/status/111");
        assert!(t
            .content_html
            .contains(r#"<a href="https://example.com/page">https://t.co/abc</a>"#));
        assert!(t.content_html.starts_with("hello "));
        assert_eq!(t.text, "hello https://example.com/page world");
        assert_eq!(t.published_at.as_deref(), Some("2026-09-13T06:17:25+00:00"));
        assert!(t.image_url.is_none());

        // Photo tweet: media placeholder dropped from text, <img> appended.
        let t = &tweets[1];
        assert!(!t.content_html.contains("t.co/pic"));
        assert!(t
            .content_html
            .contains(r#"<img src="https://pbs.twimg.com/media/abc.jpg""#));
        assert_eq!(t.text, "pic");
        assert_eq!(
            t.image_url.as_deref(),
            Some("https://pbs.twimg.com/media/abc.jpg")
        );

        // Video tweet: highest-bitrate mp4 wins; m3u8 ignored.
        let t = &tweets[2];
        assert!(t
            .content_html
            .contains(r#"<video src="https://video.twimg.com/high.mp4""#));
        assert_eq!(
            t.video_url.as_deref(),
            Some("https://video.twimg.com/high.mp4")
        );
    }

    #[test]
    fn user_tweets_map_graphql_auth_errors() {
        let body = r#"{"errors":[{"code":239,"message":"Bad guest token."}]}"#;
        assert!(matches!(
            parse_user_tweets_json(body, "jack"),
            Err(XError::Auth)
        ));
        let gone = r#"{"data":{"user":{"result":{"__typename":"UserUnavailable"}}}}"#;
        assert!(matches!(
            parse_user_tweets_json(gone, "jack"),
            Err(XError::Unavailable(_))
        ));
    }

    #[test]
    fn quoted_tweets_read_the_nested_shape() {
        let body = r#"{"data": {"user": {"result": {"__typename": "User", "timeline_v2": {"timeline": {"instructions": [{"type": "TimelineAddEntries", "entries": [{"entryId": "tweet-555", "content": {"entryType": "TimelineTimelineItem", "itemContent": {"tweet_results": {"result": {"__typename": "Tweet", "tweet": {"rest_id": "555", "legacy": {"full_text": "nested text", "created_at": "Sun Sep 13 06:17:28 +0000 2026", "entities": {}}}}}}}}]}]}}}}}}"#;
        let tweets = parse_user_tweets_json(body, "jack").unwrap();
        assert_eq!(tweets.len(), 1);
        assert_eq!(tweets[0].id, "555");
        assert_eq!(tweets[0].content_html, "nested text");
    }

    #[test]
    fn utf16_indices_survive_emoji() {
        // 😀 is an astral char: 2 UTF-16 units, 4 UTF-8 bytes. The entity's
        // indices are UTF-16 — a byte-based slicer would garble the tweet.
        // UTF-16 map of "😀 hi there": 😀=0..2, ' '=2, h=3, i=4, ' '=5, …
        let full_text = "😀 hi there";
        let entities = vec![TextEntity {
            start: 3,
            end: 4,
            href: Some("https://example.com".into()),
        }];
        // Only a UTF-16-aware mapping slices the 'h' of "hi" (byte offset 5).
        let html = render_tweet_html(full_text, &entities, &[]);
        assert!(html.starts_with("😀 <a href=\"https://example.com\">h</a>i there"));
        let text = tweet_plain_text(full_text, &entities);
        assert_eq!(text, "😀 https://example.comi there");
    }

    #[test]
    fn html_and_text_escape_untrusted_content() {
        let full_text = "<script>alert(1)</script> & \"quotes\"\nnext line";
        let html = render_tweet_html(full_text, &[], &[]);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("<br/>"));
    }

    #[test]
    fn best_video_variant_prefers_highest_bitrate_mp4() {
        let variants = serde_json::json!([
            {"content_type":"video/mp4","bitrate":632000,"url":"low.mp4"},
            {"content_type":"video/mp4","bitrate":2176000,"url":"high.mp4"},
            {"content_type":"application/x-mpegURL","url":"hls.m3u8"}
        ]);
        assert_eq!(best_video_variant(&variants).as_deref(), Some("high.mp4"));
        assert_eq!(best_video_variant(&serde_json::json!([])), None);
    }

    #[test]
    fn state_round_trips_through_the_settings_table() {
        let conn = crate::db::test_conn();

        let mut state = XState::default();
        assert_eq!(state.query_ids.user_tweets, None);
        state.query_ids = QueryIds {
            user_tweets: Some("UT".into()),
            user_by_screen_name: Some("UB".into()),
        };
        state.user_ids.insert("jack".into(), "123".into());
        state.save(&conn);

        let loaded = XState::load(&conn);
        assert_eq!(loaded, state);
    }
}
