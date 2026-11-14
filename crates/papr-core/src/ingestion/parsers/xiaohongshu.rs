//! Built-in Xiaohongshu (RED) parser: turns a blogger profile URL into feed
//! data from the profile page's server-rendered state — the site offers no
//! RSS and its JSON APIs require the `x-s`/`x-t` request signatures the web
//! client computes in obfuscated JS, so the SSR document is the one surface a
//! plain HTTP client can read faithfully.
//!
//! Dual mode, cookie optional by design:
//! - **Anonymous (default)**: `GET https://www.xiaohongshu.com/user/profile/<id>`
//!   with the full browser document header group returns the complete SSR
//!   page — `window.__INITIAL_STATE__` carries `user.userPageData.basicInfo`
//!   (nickname / bio / avatar) and up to ~30 note cards (display title, cover,
//!   publish time, author, likes). The SSR state ships *without* note ids,
//!   so articles are card-level and their guids derive from the card itself
//!   (publish time + title hash), never from the array position.
//! - **Cookie (optional upgrade)**: a pasted logged-in `Cookie` header makes
//!   the same page include note ids and per-note `xsecToken`s, which unlocks
//!   the note detail page (`/explore/<id>?xsec_token=…`) — full text, images
//!   and video. Details are fetched only for new notes, throttled and capped
//!   per refresh; a failed detail still ingests its card, and the gap is
//!   retried on a later refresh via the pending list in `parser.xiaohongshu.state`.
//!
//! The SSR state is *not* JSON: it embeds bare `undefined` tokens and empty
//! `new Map([])` literals (Vue reactive serialization), which are normalized
//! to `null` before parsing — the same normalization RSSHub performs.
//!
//! Anti-ban posture: read-only, first page only, strictly serial per feed
//! leg, jittered pacing between detail fetches, hard cap on details per
//! refresh, cookies only in an explicit header, and every response's final
//! URL must still belong to xiaohongshu.com.

use crate::db;
use crate::error::AppError;
use reqwest::{Client, StatusCode};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use url::Url;

use super::transport::{
    browser_document_headers, is_host_or_subdomain, jittered_delay_ms, pace,
};

/// The site every xiaohongshu request must stay on.
const REGISTRABLE_DOMAIN: &str = "xiaohongshu.com";
/// The host profile / detail documents are requested from.
const WEB_HOST: &str = "www.xiaohongshu.com";

/// Pause between per-feed profile fetches (the refresh leg adds jitter).
pub const FEED_THROTTLE_MS: u64 = 900;
/// Pause between note-detail fetches inside one refresh cycle.
pub const DETAIL_THROTTLE_MS: u64 = 400;
/// Hard cap on note-detail fetches per refresh cycle. A wall of new notes
/// must never turn one refresh into a crawl of the whole profile.
pub const MAX_DETAILS_PER_REFRESH: usize = 10;

/// Coded errors the frontend localises under `error.<code>`.
pub const CODE_AUTH_INVALID: &str = "xiaohongshuAuthInvalid";
pub const CODE_ACCOUNT_UNAVAILABLE: &str = "xiaohongshuAccountUnavailable";
pub const CODE_BLOCKED: &str = "xiaohongshuBlocked";
pub const CODE_HANDLE_INVALID: &str = "xiaohongshuHandleInvalid";

/* ── errors ──────────────────────────────────────────────────────────── */

/// Everything that can go wrong while fetching one profile. The variants
/// that map to a stable, localisable code expose it; everything else is
/// rendered verbatim by the caller.
#[derive(Debug, thiserror::Error)]
pub enum XhsError {
    /// The response was a login wall or the stored cookie is no longer
    /// accepted (landing on `/login`, HTTP 401).
    #[error("auth invalid")]
    Auth,
    /// The profile is gone or gated (302 to the login page on a direct hit).
    #[error("account unavailable: {0}")]
    Unavailable(String),
    /// Risk-control challenge page (`fe-verify-box`), HTTP 403/461.
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

impl XhsError {
    /// The frontend i18n key for this failure, when one exists.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            XhsError::Auth => Some(CODE_AUTH_INVALID),
            XhsError::Unavailable(_) => Some(CODE_ACCOUNT_UNAVAILABLE),
            XhsError::Blocked => Some(CODE_BLOCKED),
            XhsError::Upstream(_) | XhsError::Transport(_) | XhsError::Parse(_) => None,
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

impl From<reqwest::Error> for XhsError {
    fn from(e: reqwest::Error) -> Self {
        XhsError::Transport(e.to_string())
    }
}

pub type XhsResult<T> = Result<T, XhsError>;

/* ── URL / profile-id normalisation (pure) ───────────────────────────── */

/// True for a syntactically valid Xiaohongshu profile id: exactly 24
/// lowercase hex characters (the site's own format).
pub fn is_valid_profile_id(id: &str) -> bool {
    id.len() == 24 && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Recognize a pasted profile URL — `https://www.xiaohongshu.com/user/profile/<id>`
/// (with or without `www.`, trailing slash, query junk like `xsec_token` /
/// channel attribution) — and rewrite it to the canonical feed URL
/// `https://www.xiaohongshu.com/user/profile/<id>`. Query parameters are
/// dropped: the anonymous SSR works without `xsec_token`, and one profile
/// must yield exactly one feed row regardless of which tokenized link the
/// user copied. Returns `None` for anything else.
///
/// Pure and unit-tested; the caller decides what to do with the result.
pub fn normalize_xiaohongshu_url(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_lowercase();
    if host != "www.xiaohongshu.com" && host != "xiaohongshu.com" {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 3 || segments[0] != "user" || segments[1] != "profile" {
        return None;
    }
    let id = segments[2].to_lowercase();
    if !is_valid_profile_id(&id) {
        return None;
    }
    Some(format!("https://{WEB_HOST}/user/profile/{id}"))
}

/// Recover the profile id from a stored canonical feed URL. The inverse of
/// [`normalize_xiaohongshu_url`] restricted to canonical URLs — used by the
/// refresh leg, which only ever sees what the add flow persisted.
pub fn profile_id_from_feed_url(feed_url: &str) -> Option<String> {
    let url = Url::parse(feed_url).ok()?;
    if url.host_str()?.to_lowercase() != WEB_HOST {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 3 || segments[0] != "user" || segments[1] != "profile" {
        return None;
    }
    let id = segments[2];
    (is_valid_profile_id(id) && url.query().is_none()).then(|| id.to_string())
}

/// Accept free-form input from the settings test field: a bare 24-char id
/// (case insensitive — ids are lowercased) or a profile URL (tokenized links
/// tolerated). Returns the profile id.
pub fn sanitize_profile_input(input: &str) -> Option<String> {
    let trimmed = input.trim().to_lowercase();
    if is_valid_profile_id(&trimmed) {
        return Some(trimmed);
    }
    if trimmed.contains('.') || trimmed.contains('/') {
        let with_scheme = crate::ingestion::discovery::normalize_query_url(&trimmed);
        if let Ok(url) = Url::parse(&with_scheme) {
            return normalize_xiaohongshu_url(&url)
                .as_deref()
                .and_then(profile_id_from_feed_url);
        }
    }
    None
}

/* ── __INITIAL_STATE__ extraction (pure) ─────────────────────────────── */

/// Extract the `window.__INITIAL_STATE__={…}` object literal from the SSR
/// HTML and normalize it into parseable JSON (pure).
///
/// The literal is *not* JSON: it contains bare `undefined` values and empty
/// `new Map([])` literals. The scanner walks the text with string-literal
/// awareness (escapes included), finds the balanced end of the object, and
/// rewrites the two foreign productions to `null` — but only outside string
/// literals, never inside them.
pub fn extract_initial_state(html: &str) -> Option<String> {
    const MARKER: &str = "window.__INITIAL_STATE__=";
    let start = html.find(MARKER)? + MARKER.len();
    let rest = &html[start..];
    // The literal opens with `{` (possibly after whitespace).
    let open_rel = rest.find('{')?;
    let body = &rest[open_rel..];

    // String-aware scan: track depth, find the closing brace of the object.
    let bytes = body.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let literal = &body[..end?];
    Some(normalize_state_literal(literal))
}

/// Rewrite bare `undefined` and empty `new Map([])` occurrences to `null`,
/// skipping anything inside string literals (pure).
fn normalize_state_literal(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut rest = literal;
    while !rest.is_empty() {
        let c = rest.chars().next().unwrap();
        // Walk by `char`, never by byte — the state carries CJK text whose
        // characters span several bytes, so slicing at +1 would panic (and
        // byte-level scanning would risk splitting inside a code point).
        let advance = c.len_utf8();
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            rest = &rest[advance..];
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            rest = &rest[advance..];
            continue;
        }
        if rest.starts_with("undefined")
            && !rest["undefined".len()..]
                .starts_with(|ch: char| ch.is_alphanumeric() || ch == '_' || ch == '$')
        {
            out.push_str("null");
            rest = &rest["undefined".len()..];
            continue;
        }
        if rest.starts_with("new Map([])") {
            out.push_str("null");
            rest = &rest["new Map([])".len()..];
            continue;
        }
        out.push(c);
        rest = &rest[advance..];
    }
    out
}

/* ── parsed shapes ───────────────────────────────────────────────────── */

/// The blogger resolved from `userPageData.basicInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XhsProfileMeta {
    pub nickname: String,
    pub desc: Option<String>,
    /// Avatar URL (the larger `imageb` variant when present).
    pub avatar: Option<String>,
}

/// One note mapped for ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XhsNote {
    /// The 24-hex note id — present only in cookie mode (the anonymous SSR
    /// ships cards without ids). `None` means "card only".
    pub note_id: Option<String>,
    /// Per-note `xsecToken` from the state; required to open the detail page.
    pub xsec_token: Option<String>,
    pub title: String,
    /// `"normal"` (image set) or `"video"`.
    pub kind: Option<String>,
    /// Publish time, epoch milliseconds, from the card.
    pub time_ms: Option<i64>,
    /// Cover image URL (`cover.urlDefault`).
    pub cover: Option<String>,
    /// Author nickname (the followed blogger; kept for completeness).
    pub author: Option<String>,
    /// Like counter as rendered (`"199"`, `"1千+"`).
    pub liked_count: Option<String>,
}

/// The parsed profile page: metadata plus the newest note cards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XhsUserPage {
    pub profile: XhsProfileMeta,
    pub notes: Vec<XhsNote>,
}

/* ── state parsing (pure) ────────────────────────────────────────────── */

/// `userPageData.basicInfo` as the SSR ships it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct XhsBasicInfo {
    nickname: Option<String>,
    desc: Option<String>,
    imageb: Option<String>,
    images: Option<String>,
}

/// One note card inside `user.notes[*][*]`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsNoteCard {
    note_id: Option<String>,
    display_title: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    time: Option<i64>,
    xsec_token: Option<String>,
    cover: Option<XhsCover>,
    user: Option<XhsCardUser>,
    interact_info: Option<XhsInteractInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsCover {
    #[serde(rename = "urlDefault")]
    url_default: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsCardUser {
    nick_name: Option<String>,
    /// Some payloads ship the lower-case key alongside; either may win.
    nickname: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsInteractInfo {
    #[serde(rename = "likedCount")]
    liked_count: Option<String>,
}

/// One `user.notes[*][*]` item. `id` and `xsecToken` live on the item, the
/// card fields on `noteCard`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsNoteItem {
    id: Option<String>,
    #[serde(rename = "xsecToken")]
    xsec_token: Option<String>,
    #[serde(rename = "noteCard")]
    note_card: Option<XhsNoteCard>,
}

/// Vue reactive wrappers (`_rawValue`) are unwrapped before typed parsing, so
/// the structs below see the plain shapes.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsUserState {
    #[serde(rename = "userPageData")]
    user_page_data: Option<Value>,
    notes: Option<Value>,
}

/// `userPageData` proper: `basicInfo` plus the loose arrays we don't need.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsUserPageData {
    #[serde(rename = "basicInfo")]
    basic_info: Option<XhsBasicInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct XhsInitialState {
    user: Option<XhsUserState>,
}

/// If `value` is an object carrying Vue's `_rawValue` (the serialized shape
/// of a reactive ref), return that inner value; otherwise return `value`.
fn unwrap_raw(value: Value) -> Value {
    match &value {
        Value::Object(map) => map
            .get("_rawValue")
            .filter(|inner| !inner.is_null())
            .cloned()
            .unwrap_or(value),
        _ => value,
    }
}

/// Parse the normalized JSON of one profile page into metadata + notes.
pub fn parse_profile_state(json: &str) -> XhsResult<XhsUserPage> {
    let state: XhsInitialState = serde_json::from_str(json)
        .map_err(|e| XhsError::Parse(format!("initial state: {e}")))?;
    let user = state.user.ok_or_else(|| XhsError::Parse("no `user` node".into()))?;

    let page_data = user
        .user_page_data
        .map(unwrap_raw)
        .map(serde_json::from_value::<XhsUserPageData>)
        .transpose()
        .map_err(|e| XhsError::Parse(format!("userPageData: {e}")))?
        .ok_or_else(|| XhsError::Parse("no `userPageData` node".into()))?;
    let basic = page_data
        .basic_info
        .ok_or_else(|| XhsError::Parse("no `basicInfo` node".into()))?;

    let nickname = basic
        .nickname
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| XhsError::Parse("basicInfo has no nickname (cookie expired or gated?)".into()))?;
    let profile = XhsProfileMeta {
        nickname,
        desc: basic.desc.filter(|d| !d.trim().is_empty()),
        avatar: basic.imageb.or(basic.images),
    };

    let notes_value = user.notes.map(unwrap_raw).unwrap_or(Value::Null);
    let mut notes = Vec::new();
    collect_notes(&notes_value, &mut notes);
    Ok(XhsUserPage { profile, notes })
}

/// Flatten `user.notes` — an array of pages (arrays of items), sometimes just
/// an array of items — into a typed note list. Items without a card, or with
/// neither a title nor an id, are skipped.
fn collect_notes(value: &Value, out: &mut Vec<XhsNote>) {
    let Value::Array(items) = value else {
        return;
    };
    for item in items {
        if item.is_array() {
            collect_notes(item, out);
            continue;
        }
        let Ok(parsed) = serde_json::from_value::<XhsNoteItem>(item.clone()) else {
            continue;
        };
        let Some(card) = parsed.note_card else {
            continue;
        };
                let note_id = parsed
                    .id
                    .filter(|s| !s.is_empty())
                    .or(card.note_id.filter(|s| !s.is_empty()));
                let xsec_token = parsed
                    .xsec_token
                    .filter(|s| !s.is_empty())
                    .or(card.xsec_token.filter(|s| !s.is_empty()));
                let title = card
                    .display_title
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_default();
                let cover = card
                    .cover
                    .and_then(|c| c.url_default)
                    .filter(|s| !s.is_empty());
                if note_id.is_none() && title.is_empty() && cover.is_none() {
                    continue;
                }
        out.push(XhsNote {
            note_id,
            xsec_token,
            title,
            kind: card.kind,
            time_ms: card.time,
            cover,
            author: card
                .user
                .and_then(|u| u.nick_name.or(u.nickname))
                .filter(|s| !s.trim().is_empty()),
            liked_count: card.interact_info.and_then(|i| i.liked_count),
        });
    }
}

/* ── note detail state (pure) ────────────────────────────────────────── */

/// The `note` node of a detail page (`noteDetailMap[firstNoteId].note`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsNoteDetail {
    title: Option<String>,
    desc: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    time: Option<i64>,
    #[serde(rename = "lastUpdateTime")]
    last_update_time: Option<i64>,
    #[serde(rename = "imageList")]
    image_list: Option<Value>,
    video: Option<Value>,
}

/// One image of an image-set note: `urlDefault` is the rendered variant.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsDetailImage {
    #[serde(rename = "urlDefault")]
    url_default: Option<String>,
    url: Option<String>,
}

/// A video stream entry (`media.stream.<codec>[*]`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct XhsVideoStream {
    #[serde(rename = "masterUrl")]
    master_url: Option<String>,
}

/// Parse the normalized JSON of a note detail page into the full note body.
/// Returns `(title, desc, kind, time_ms, images, video_urls)`.
pub fn parse_note_detail_state(
    json: &str,
    expect_note_id: &str,
) -> XhsResult<XhsNoteDetailParsed> {
    let state: Value = serde_json::from_str(json)
        .map_err(|e| XhsError::Parse(format!("detail state: {e}")))?;
    let note_node = state
        .get("note")
        .ok_or_else(|| XhsError::Parse("no `note` node".into()))?;
    let map = note_node
        .get("noteDetailMap")
        .and_then(|m| m.as_object())
        .ok_or_else(|| XhsError::Parse("no `noteDetailMap`".into()))?;
    // `firstNoteId` is the canonical key, but tolerate the id we came with.
    let key = note_node
        .get("firstNoteId")
        .and_then(|v| v.as_str())
        .filter(|s| map.contains_key(*s))
        .or_else(|| note_node.get("currentNoteId").and_then(|v| v.as_str()))
        .unwrap_or(expect_note_id);
    let entry = map
        .get(key)
        .and_then(|e| e.get("note"))
        .ok_or_else(|| XhsError::Parse("noteDetailMap has no entry for this note".into()))?;
    let detail: XhsNoteDetail = serde_json::from_value(entry.clone())
        .map_err(|e| XhsError::Parse(format!("note detail: {e}")))?;

    let mut images = Vec::new();
    if let Some(list) = detail.image_list.as_ref().and_then(|v| v.as_array()) {
        for img in list {
            if let Ok(img) = serde_json::from_value::<XhsDetailImage>(img.clone()) {
                let url = img.url_default.or(img.url).filter(|s| !s.is_empty());
                if let Some(url) = url {
                    images.push(url);
                }
            }
        }
    }

    let mut video_urls = Vec::new();
    if let Some(video) = detail.video.as_ref() {
        // `media.stream.<codec>[*].masterUrl`, codecs tried in the web
        // client's preference order (h264 first — widest player support).
        if let Some(streams) = video
            .pointer("/media/stream")
            .and_then(|s| s.as_object())
        {
            for codec in ["h264", "av1", "h265", "h266"] {
                if let Some(list) = streams.get(codec).and_then(|v| v.as_array()) {
                    for stream in list {
                        if let Ok(s) = serde_json::from_value::<XhsVideoStream>(stream.clone()) {
                            if let Some(url) = s.master_url.filter(|u| !u.is_empty()) {
                                video_urls.push(url);
                            }
                        }
                    }
                }
                if !video_urls.is_empty() {
                    break;
                }
            }
        }
        // Fallback: the original object key builds a direct CDN URL.
        if video_urls.is_empty() {
            if let Some(key) = video
                .pointer("/consumer/originVideoKey")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                video_urls.push(format!("https://sns-video-al.xhscdn.com/{key}"));
            }
        }
    }

    Ok(XhsNoteDetailParsed {
        title: detail.title.filter(|s| !s.trim().is_empty()),
        desc: detail.desc.filter(|s| !s.trim().is_empty()),
        kind: detail.kind,
        time_ms: detail.time.or(detail.last_update_time),
        images,
        video_urls,
    })
}

/// The typed content of one note detail page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct XhsNoteDetailParsed {
    pub title: Option<String>,
    pub desc: Option<String>,
    pub kind: Option<String>,
    pub time_ms: Option<i64>,
    pub images: Vec<String>,
    pub video_urls: Vec<String>,
}

/* ── dates / guids (pure) ────────────────────────────────────────────── */

/// Epoch milliseconds → RFC 3339.
pub fn ms_to_rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
}

/// First 8 hex chars of SHA-256 — the disambiguator inside synthetic guids.
fn short_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// The stable guid for a note card. The anonymous SSR ships no note ids, so
/// the guid derives from what the card itself guarantees: publish time plus
/// a title hash (both stable across refreshes, independent of array
/// position). `xhs-<profile>-<time>-<hash>` — two cards sharing a publish
/// millisecond still separate by title.
pub fn note_guid(profile_id: &str, note: &XhsNote) -> String {
    match note.time_ms {
        Some(ms) => format!("xhs-{profile_id}-{ms}-{}", short_hash(&note.title)),
        None => format!("xhs-{profile_id}-{}", short_hash(&note.title)),
    }
}

/* ── article builders (pure) ─────────────────────────────────────────── */

fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The ready-to-render card: cover image, title, and a light meta line.
pub fn note_card_html(note: &XhsNote, profile_url: &str) -> String {
    let mut html = String::new();
    if let Some(cover) = &note.cover {
        html.push_str(&format!(
            "<p><img src=\"{}\" alt=\"{}\"></p>",
            escape_html_text(cover),
            escape_html_text(&note.title)
        ));
    }
    html.push_str(&format!(
        "<div class=\"xhs-card\"><p class=\"x-xhs-card-title\">{}</p>",
        escape_html_text(&note.title)
    ));
    let mut meta = Vec::new();
    if let Some(rfc3339) = note.time_ms.and_then(ms_to_rfc3339) {
        // Date only — the card's own granularity is the publish moment.
        meta.push(rfc3339[..10.min(rfc3339.len())].to_string());
    }
    if let Some(likes) = &note.liked_count {
        meta.push(format!("♥ {}", escape_html_text(likes)));
    }
    if note.kind.as_deref() == Some("video") {
        meta.push("▶".into());
    }
    if !meta.is_empty() {
        html.push_str(&format!(
            "<p class=\"x-xhs-card-meta\">{}</p>",
            escape_html_text(&meta.join(" · "))
        ));
    }
    html.push_str(&format!(
        "<p class=\"x-xhs-card-link\"><a href=\"{}\">{}</a></p></div>",
        escape_html_text(profile_url),
        escape_html_text(profile_url)
    ));
    html
}

/// The full note body from a detail page: media first, then the text.
pub fn note_detail_html(detail: &XhsNoteDetailParsed, note_id: &str, token: &str) -> String {
    let mut html = String::new();
    if detail.kind.as_deref() == Some("video") {
        let poster = detail.images.first().map(|u| u.as_str()).unwrap_or("");
        if let Some(video) = detail.video_urls.first() {
            html.push_str(&format!(
                "<p><video controls preload=\"metadata\" poster=\"{}\"><source src=\"{}\" type=\"video/mp4\"></video></p>",
                escape_html_text(poster),
                escape_html_text(video)
            ));
        }
    }
    for image in &detail.images {
        html.push_str(&format!(
            "<p><img src=\"{}\"></p>",
            escape_html_text(image)
        ));
    }
    if let Some(desc) = &detail.desc {
        // Plain text with newlines → paragraphs; the sanitizer has the final
        // say on every tag before storage.
        let body = escape_html_text(desc).replace('\n', "<br>");
        html.push_str(&format!("<p>{body}</p>"));
    }
    let _ = (note_id, token);
    html
}

/// Map a note card to the ingestion shape. `detail` upgrades the card to the
/// full body when the detail page was fetched (cookie mode); the card alone
/// remains a complete, presentable article otherwise. `guid_override` pins
/// the guid for a pending-retry article — the retry must land on the exact
/// row the card created, and the detail page's own title/time may differ
/// from the card's, so the guid cannot be recomputed there.
pub fn note_to_article(
    profile_id: &str,
    profile_url: &str,
    note: &XhsNote,
    detail: Option<&XhsNoteDetailParsed>,
    guid_override: Option<&str>,
) -> crate::db::NewArticle {
    let url = match (&note.note_id, &note.xsec_token) {
        (Some(id), Some(token)) => format!(
            "https://{WEB_HOST}/explore/{id}?xsec_token={token}&xsec_source=pc_user"
        ),
        (Some(id), None) => format!("https://{WEB_HOST}/explore/{id}"),
        _ => profile_url.to_string(),
    };
    let title = if note.title.trim().is_empty() {
        // Untitled notes exist (pure video drops). A stable, readable title
        // from the date keeps the list row presentable.
        match note.time_ms.and_then(ms_to_rfc3339) {
            Some(ts) => format!("笔记 {}", &ts[..10.min(ts.len())]),
            None => "无标题笔记".into(),
        }
    } else {
        note.title.clone()
    };
    let content_html = match detail {
        Some(parsed) => note_detail_html(parsed, note.note_id.as_deref().unwrap_or(""), ""),
        None => note_card_html(note, profile_url),
    };
    let body_text = match detail.and_then(|d| d.desc.clone()) {
        Some(desc) => desc,
        None => note.title.clone(),
    };
    crate::db::NewArticle {
        guid: guid_override
            .map(String::from)
            .unwrap_or_else(|| note_guid(profile_id, note)),
        url: Some(url.clone()),
        title,
        author: note.author.clone(),
        summary: Some(note.title.clone()),
        content_html: Some(crate::sanitize::sanitize_parser(&content_html, Some(&url))),
        body_text,
        image_url: note.cover.clone(),
        published_at: note.time_ms.and_then(ms_to_rfc3339),
        enclosures: detail
            .and_then(|d| d.video_urls.first().cloned())
            .map(|video| {
                vec![crate::models::Enclosure {
                    url: video,
                    mime_type: Some("video/mp4".into()),
                    length: None,
                }]
            })
            .unwrap_or_default(),
    }
}

/* ── network transport ───────────────────────────────────────────────── */

/// GET an SSR document with the browser header group, mapping the upstream
/// status and landing page onto the error taxonomy. Cookies travel only in
/// the explicit header — never a jar — so they cannot leak even if the edge
/// redirected.
async fn get_document(
    client: &Client,
    url: &str,
    cookie: Option<&str>,
) -> XhsResult<String> {
    let mut req = client.get(url);
    for (name, value) in browser_document_headers(WEB_HOST, cookie) {
        req = req.header(name, value);
    }
    let resp = req.send().await?;
    if !is_host_or_subdomain(resp.url(), REGISTRABLE_DOMAIN) {
        return Err(XhsError::Transport(format!(
            "redirect to non-xiaohongshu host blocked: {}",
            resp.url()
        )));
    }
    // The parser client does not follow redirects (cookies must never ride a
    // redirect). The edge's only redirect here is the risk-control login
    // bounce — anonymous profile access is throttled per IP/visitor, so a
    // cold visitor gets the SSR page and a hot one a 302 onto /login,
    // exactly what the real browser sees. Surface that gate (and a genuinely
    // gated profile) as `Blocked`; 401 with a pasted cookie means the session
    // is rejected; only an explicit 404/410 means gone.
    if resp.status().is_redirection() {
        return Err(XhsError::Blocked);
    }
    match resp.status() {
        StatusCode::UNAUTHORIZED => return Err(XhsError::Auth),
        StatusCode::FORBIDDEN => return Err(XhsError::Blocked),
        s if s.as_u16() == 461 => return Err(XhsError::Blocked),
        s if s == StatusCode::NOT_FOUND || s == StatusCode::GONE => {
            return Err(XhsError::Unavailable(format!("HTTP {}", s.as_u16())))
        }
        s if !s.is_success() => return Err(XhsError::Upstream(s.as_u16())),
        _ => {}
    }
    let bytes = crate::ingestion::fetch::read_capped(resp)
        .await
        .map_err(|e| XhsError::Transport(e.to_string()))?;
    let html = String::from_utf8(bytes)
        .map_err(|_| XhsError::Parse("response is not UTF-8".into()))?;
    if html.contains("fe-verify-box") {
        return Err(XhsError::Blocked);
    }
    Ok(html)
}

/// Fetch one profile page and parse it into metadata + note cards.
///
/// `cookie` upgrades the response to include note ids / tokens (cookie
/// mode); `None` runs the anonymous mode that needs no configuration.
pub async fn fetch_user_notes(
    client: &Client,
    profile_id: &str,
    cookie: Option<&str>,
) -> XhsResult<XhsUserPage> {
    let url = format!("https://{WEB_HOST}/user/profile/{profile_id}");
    let html = get_document(client, &url, cookie).await?;
    let json = extract_initial_state(&html)
        .ok_or_else(|| XhsError::Parse("no __INITIAL_STATE__ on profile page".into()))?;
    parse_profile_state(&json)
}

/// Fetch one note detail page (cookie mode only) and parse the full body.
pub async fn fetch_note_detail(
    client: &Client,
    note_id: &str,
    xsec_token: &str,
    cookie: &str,
) -> XhsResult<XhsNoteDetailParsed> {
    let url = format!(
        "https://{WEB_HOST}/explore/{note_id}?xsec_token={xsec_token}&xsec_source=pc_user"
    );
    let html = get_document(client, &url, Some(cookie)).await?;
    let json = extract_initial_state(&html)
        .ok_or_else(|| XhsError::Parse("no __INITIAL_STATE__ on note page".into()))?;
    parse_note_detail_state(&json, note_id)
}

/* ── pending-detail state ────────────────────────────────────────────── */

/// One note whose card was ingested but whose detail page has not been
/// fetched yet (cookie mode): retried on later refreshes, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct XhsPendingNote {
    pub note_id: String,
    pub xsec_token: Option<String>,
    /// The card-derived guid this note was first ingested under — the retry
    /// must rewrite that exact row, not insert a duplicate.
    pub guid: String,
}

/// Parser runtime state persisted across refreshes under
/// `parser.xiaohongshu.state`: per-feed queues of notes awaiting a detail
/// fetch. Accelerator only — a lost queue costs full-text on *new* notes,
/// never data.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct XhsState {
    /// feed_id → notes still awaiting their detail page.
    pub pending: BTreeMap<i64, Vec<XhsPendingNote>>,
}

impl XhsState {
    const SETTING_KEY: &'static str = "parser.xiaohongshu.state";

    /// Load persisted state. A missing or malformed blob degrades to a fresh
    /// state — the queue is an accelerator, never correctness-critical.
    pub fn load(conn: &Connection) -> XhsState {
        db::get_setting(conn, Self::SETTING_KEY)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Persist the state back. Failures are swallowed: a lost queue costs
    /// one extra detail fetch attempt on the next refresh, never data.
    pub fn save(&self, conn: &Connection) {
        if let Ok(json) = serde_json::to_string(self) {
            let _ = db::set_setting(conn, Self::SETTING_KEY, &json);
        }
    }

    /// Queue a note for detail fetch, dropping the oldest entries when the
    /// per-feed backlog outgrows [`MAX_PENDING_PER_FEED`].
    pub fn enqueue(&mut self, feed_id: i64, note: XhsPendingNote) {
        let queue = self.pending.entry(feed_id).or_default();
        if queue.iter().any(|p| p.note_id == note.note_id) {
            return;
        }
        queue.push(note);
        while queue.len() > MAX_PENDING_PER_FEED {
            queue.remove(0);
        }
    }

    /// Pop up to `limit` notes for one feed.
    pub fn take(&mut self, feed_id: i64, limit: usize) -> Vec<XhsPendingNote> {
        let Some(queue) = self.pending.get_mut(&feed_id) else {
            return Vec::new();
        };
        let take = queue.len().min(limit);
        queue.drain(..take).collect()
    }
}

/// Per-feed backlog bound so a profile whose details never resolve cannot
/// grow the setting blob forever.
pub const MAX_PENDING_PER_FEED: usize = 30;

/// Sleep between two detail fetches, with jitter.
pub async fn pace_details() {
    pace(jittered_delay_ms(DETAIL_THROTTLE_MS)).await;
}

/// True when the process should fetch note details at all: cookie mode with
/// a real note id and token.
pub fn detail_fetchable(note: &XhsNote, cookie: Option<&str>) -> bool {
    cookie.is_some() && note.note_id.is_some() && note.xsec_token.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ── URL normalisation ─────────────────────────────────────────── */

    fn parse(url: &str) -> Url {
        Url::parse(&crate::ingestion::discovery::normalize_query_url(url)).unwrap()
    }

    #[test]
    fn normalizes_plain_profile_urls() {
        let url = parse("https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895");
        assert_eq!(
            normalize_xiaohongshu_url(&url).as_deref(),
            Some("https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895")
        );
    }

    #[test]
    fn normalizes_tokenized_and_attributed_urls_to_canonical() {
        let url = parse(
            "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895/?xsec_token=ABhu_Cqz8_LewlXka4tu0shciSGEjMub3BwfnAS75ovs0=&xsec_source=pc_feed",
        );
        assert_eq!(
            normalize_xiaohongshu_url(&url).as_deref(),
            Some("https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895")
        );
        let bare = parse("xiaohongshu.com/user/profile/5e47b1ed0000000001006895");
        assert!(normalize_xiaohongshu_url(&bare).is_some());
    }

    #[test]
    fn rejects_other_pages_and_bad_ids() {
        for url in [
            "https://www.xiaohongshu.com/explore/abc",
            "https://www.xiaohongshu.com/user/profile/short",
            "https://www.xiaohongshu.com/user/profile/5e47b1ed000000000100zzzz",
            "https://evil.com/user/profile/5e47b1ed0000000001006895",
            "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895/extra",
        ] {
            assert!(normalize_xiaohongshu_url(&parse(url)).is_none(), "{url}");
        }
        // Uppercase ids normalize (lowercased) rather than reject.
        let upper = parse("https://www.xiaohongshu.com/user/profile/5E47B1ED0000000001006895");
        assert_eq!(
            normalize_xiaohongshu_url(&upper).as_deref(),
            Some("https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895")
        );
    }

    #[test]
    fn profile_id_round_trip() {
        let feed_url = "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895";
        assert_eq!(
            profile_id_from_feed_url(feed_url).as_deref(),
            Some("5e47b1ed0000000001006895")
        );
        // A tokenized URL is never stored, so the refresh-side parser must
        // reject it rather than silently accept a divergent feed row.
        assert!(profile_id_from_feed_url(
            "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895?xsec_token=x"
        )
        .is_none());
        assert!(profile_id_from_feed_url("https://www.xiaohongshu.com/explore/x").is_none());
    }

    #[test]
    fn sanitize_accepts_bare_id_and_url() {
        assert_eq!(
            sanitize_profile_input("5E47B1ED0000000001006895"),
            Some("5e47b1ed0000000001006895".to_string())
        );
        assert_eq!(
            sanitize_profile_input(
                "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895?xsec_token=AB&xsec_source=pc_feed"
            ),
            Some("5e47b1ed0000000001006895".to_string())
        );
        assert_eq!(sanitize_profile_input("not an id"), None);
    }

    /* ── INITIAL_STATE extraction ──────────────────────────────────── */

    #[test]
    fn extracts_and_normalizes_state_literal() {
        let html = r#"<html><script>window.__INITIAL_STATE__={"a":1,"b":undefined,"c":new Map([]),"d":{"e":"has } brace and \"quote\"","f":undefined}};(function(){var x="};"});()</script></html>"#;
        let json = extract_initial_state(html).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["a"], 1);
        assert!(v["b"].is_null());
        assert!(v["c"].is_null());
        // The string containing `}` and escapes must survive untouched.
        assert_eq!(v["d"]["e"], "has } brace and \"quote\"");
        assert!(v["d"]["f"].is_null());
        // The trailing IIFE must not leak into the literal.
        assert!(v.get("x").is_none());
    }

    #[test]
    fn undefined_inside_strings_survives() {
        let html = r#"<script>window.__INITIAL_STATE__={"t":"the word undefined stays","u":undefined}</script>"#;
        let v: Value = serde_json::from_str(&extract_initial_state(html).unwrap()).unwrap();
        assert_eq!(v["t"], "the word undefined stays");
        assert!(v["u"].is_null());
    }

    #[test]
    fn missing_state_returns_none() {
        assert!(extract_initial_state("<html>nothing here</html>").is_none());
    }

    #[test]
    fn cjk_text_survives_the_normalizer() {
        // Real SSR pages are full of Chinese text; the scanner must walk by
        // `char`, not by byte (a +1 slice inside a CJK code point panics).
        let html = "<script>window.__INITIAL_STATE__={\"a\":\"今天去一家新店\",\"b\":undefined,\"c\":\"undefined未定义\"}</script>";
        let v: Value =
            serde_json::from_str(&extract_initial_state(html).unwrap()).unwrap();
        assert_eq!(v["a"], "今天去一家新店");
        assert!(v["b"].is_null());
        // `undefined` inside a CJK string stays a literal.
        assert_eq!(v["c"], "undefined未定义");
    }

    /* ── profile state parsing ─────────────────────────────────────── */

    fn profile_fixture() -> String {
        // Modelled on the real anonymous SSR of
        // https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895
        // (2026-11): Vue reactive wrappers, camelCase note fields, no ids.
        r#"{
          "user": {
            "userPageData": {
              "_rawValue": {
                "basicInfo": {
                  "nickname": "给我开饭",
                  "desc": "穿搭｜探店｜旅行",
                  "imageb": "https://sns-avatar-qc.xhscdn.com/avatar/1040g2jo31tq0?imageView2/2/w/540/format/webp",
                  "images": "https://sns-avatar-qc.xhscdn.com/avatar/1040g2jo31tq0?imageView2/2/w/360/format/webp"
                },
                "interactions": [{"name": "粉丝", "count": "1万+"}]
              }
            },
            "notes": {
              "_rawValue": [
                [
                  {
                    "id": "",
                    "xsecToken": "ABhu_Cqz8_LewlXka4tu0shciSGEjMub3BwfnAS75ovs0=",
                    "noteCard": {
                      "noteId": "",
                      "displayTitle": "这期偏韩系",
                      "type": "normal",
                      "time": 1794462771000,
                      "cover": {"urlDefault": "http://sns-webpic-qc.xhscdn.com/202611142016/abc/1040g2sg327f8mdbj70fg5ni7m7mg8q4l2nne4i8!nc_n_nwebp_mw_1"},
                      "user": {"nickname": "给我开饭", "nickName": "给我开饭"},
                      "interactInfo": {"liked": false, "likedCount": "199"}
                    }
                  },
                  {
                    "id": "",
                    "noteCard": {
                      "displayTitle": "视频笔记",
                      "type": "video",
                      "time": 1794364119000,
                      "cover": {"urlDefault": "http://sns-webpic-qc.xhscdn.com/cover2"},
                      "user": {"nickname": "给我开饭"},
                      "interactInfo": {"likedCount": "1千+"}
                    }
                  }
                ],
                [
                  {"id": "66f00a22000000001e01c0e4", "xsecToken": "ABz9token=", "noteCard": {
                    "noteId": "66f00a22000000001e01c0e4",
                    "displayTitle": "带 id 的卡片(cookie 模式)",
                    "type": "normal",
                    "time": 1794111111000
                  }}
                ]
              ]
            }
          }
        }"#.to_string()
    }

    #[test]
    fn parses_anonymous_profile_with_raw_value_wrappers() {
        let page = parse_profile_state(&profile_fixture()).unwrap();
        assert_eq!(page.profile.nickname, "给我开饭");
        assert_eq!(page.profile.desc.as_deref(), Some("穿搭｜探店｜旅行"));
        assert!(page.profile.avatar.unwrap().contains("w/540"));
        assert_eq!(page.notes.len(), 3);
        // The anonymous cards carry no ids — the guid must not need one.
        assert!(page.notes[0].note_id.is_none());
        assert_eq!(page.notes[0].title, "这期偏韩系");
        assert_eq!(page.notes[0].time_ms, Some(1794462771000));
        assert_eq!(page.notes[0].liked_count.as_deref(), Some("199"));
        // Token surfaces on the item even when the card omits it.
        assert!(page.notes[0].xsec_token.is_some());
        // Cookie-mode card with a real id still parses.
        assert_eq!(
            page.notes[2].note_id.as_deref(),
            Some("66f00a22000000001e01c0e4")
        );
    }

    #[test]
    fn missing_nickname_is_a_parse_error() {
        let json = r#"{"user":{"userPageData":{"basicInfo":{}},"notes":[]}}"#;
        assert!(parse_profile_state(json).is_err());
        let json = r#"{}"#;
        assert!(parse_profile_state(json).is_err());
    }

    /* ── detail state parsing ──────────────────────────────────────── */

    fn detail_fixture() -> String {
        r#"{
          "note": {
            "firstNoteId": "66f00a22000000001e01c0e4",
            "noteDetailMap": {
              "66f00a22000000001e01c0e4": {
                "note": {
                  "title": "秋天的第一顿火锅",
                  "desc": "今天去了一家新店\n锅底很香\n推荐毛肚",
                  "type": "video",
                  "time": 1794111111000,
                  "lastUpdateTime": 1794112222000,
                  "imageList": [{"urlDefault": "https://sns-webpic-qc.xhscdn.com/img1"}],
                  "video": {
                    "consumer": {"originVideoKey": "1000g00000abc"},
                    "media": {"stream": {"h264": [{"masterUrl": "https://sns-video-hw.xhscdn.com/stream/66.mp4"}]}}
                  }
                }
              }
            }
          }
        }"#.to_string()
    }

    #[test]
    fn parses_note_detail_with_video_and_images() {
        let d = parse_note_detail_state(&detail_fixture(), "66f00a22000000001e01c0e4").unwrap();
        assert_eq!(d.title.as_deref(), Some("秋天的第一顿火锅"));
        assert_eq!(d.kind.as_deref(), Some("video"));
        assert_eq!(d.images, vec!["https://sns-webpic-qc.xhscdn.com/img1"]);
        // h264 masterUrl wins over the consumer-key fallback.
        assert_eq!(d.video_urls, vec!["https://sns-video-hw.xhscdn.com/stream/66.mp4"]);
        assert_eq!(d.time_ms, Some(1794111111000));
    }

    #[test]
    fn video_falls_back_to_origin_key() {
        let json = r#"{"note":{"firstNoteId":"a1","noteDetailMap":{"a1":{"note":{"type":"video","video":{"consumer":{"originVideoKey":"key123"}}}}}}}"#;
        let d = parse_note_detail_state(json, "a1").unwrap();
        assert_eq!(d.video_urls, vec!["https://sns-video-al.xhscdn.com/key123"]);
    }

    #[test]
    fn detail_parse_tolerates_missing_entries() {
        let json = r#"{"note":{"firstNoteId":"zz","noteDetailMap":{"a1":{"note":{"title":"t"}}}}}"#;
        assert!(parse_note_detail_state(json, "zz").is_err());
    }

    /* ── guids & articles ──────────────────────────────────────────── */

    #[test]
    fn guids_are_stable_and_position_independent() {
        let note = XhsNote {
            note_id: None,
            xsec_token: None,
            title: "这期偏韩系".into(),
            kind: Some("normal".into()),
            time_ms: Some(1794462771000),
            cover: None,
            author: None,
            liked_count: None,
        };
        let g1 = note_guid("5e47b1ed0000000001006895", &note);
        let g2 = note_guid("5e47b1ed0000000001006895", &note);
        assert_eq!(g1, g2);
        assert!(g1.starts_with("xhs-5e47b1ed0000000001006895-1794462771000-"));
        // Same millisecond, different title → different guid.
        let mut other = note.clone();
        other.title = "另一条".into();
        assert_ne!(g1, note_guid("5e47b1ed0000000001006895", &other));
        // No time at all still yields a deterministic guid.
        other.time_ms = None;
        assert!(note_guid("5e47b1ed0000000001006895", &other).contains("xhs-5e47b1ed0000000001006895-"));
    }

    #[test]
    fn card_article_carries_cover_title_and_profile_link() {
        let note = XhsNote {
            note_id: None,
            xsec_token: Some("ABx".into()),
            title: "这期偏韩系".into(),
            kind: Some("normal".into()),
            time_ms: Some(1794462771000),
            cover: Some("http://sns-webpic-qc.xhscdn.com/cover".into()),
            author: Some("给我开饭".into()),
            liked_count: Some("199".into()),
        };
        let article = note_to_article(
            "5e47b1ed0000000001006895",
            "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895",
            &note,
            None,
            None,
        );
        assert!(article.title == "这期偏韩系");
        assert!(article.url.unwrap().contains("/user/profile/"));
        let html = article.content_html.unwrap();
        assert!(html.contains("sns-webpic-qc.xhscdn.com/cover"));
        assert!(html.contains("x-xhs-card"));
        assert!(html.contains("♥ 199"));
        assert_eq!(article.image_url.as_deref(), Some("http://sns-webpic-qc.xhscdn.com/cover"));
        assert!(article.published_at.is_some());
    }

    #[test]
    fn detail_article_upgrades_body_and_enclosure() {
        let note = XhsNote {
            note_id: Some("66f00a22000000001e01c0e4".into()),
            xsec_token: Some("ABz9token=".into()),
            title: "秋天的第一顿火锅".into(),
            kind: Some("video".into()),
            time_ms: Some(1794111111000),
            cover: Some("http://sns-webpic-qc.xhscdn.com/cover".into()),
            author: None,
            liked_count: None,
        };
        let detail = XhsNoteDetailParsed {
            title: Some("秋天的第一顿火锅".into()),
            desc: Some("今天去了一家新店\n锅底很香".into()),
            kind: Some("video".into()),
            time_ms: Some(1794111111000),
            images: vec!["https://sns-webpic-qc.xhscdn.com/img1".into()],
            video_urls: vec!["https://sns-video-hw.xhscdn.com/stream/66.mp4".into()],
        };
        let article = note_to_article(
            "5e47b1ed0000000001006895",
            "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895",
            &note,
            Some(&detail),
            None,
        );
        // Permalink with the token, per the detail-page contract.
        assert!(article
            .url
            .as_deref()
            .unwrap()
            .starts_with("https://www.xiaohongshu.com/explore/66f00a22000000001e01c0e4?xsec_token="));
        let html = article.content_html.unwrap();

        assert!(html.contains("<video"));
        assert!(html.contains("stream/66.mp4"));
        assert!(html.contains("今天去了一家新店<br>锅底很香"));
        assert_eq!(
            article.enclosures.first().map(|e| e.url.as_str()),
            Some("https://sns-video-hw.xhscdn.com/stream/66.mp4")
        );
    }

    #[test]
    fn untitled_notes_get_a_readable_fallback_title() {
        let note = XhsNote {
            note_id: None,
            xsec_token: None,
            title: String::new(),
            kind: Some("video".into()),
            time_ms: Some(1794462771000),
            cover: None,
            author: None,
            liked_count: None,
        };
        let article = note_to_article(
            "5e47b1ed0000000001006895",
            "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895",
            &note,
            None,
            None,
        );
        assert!(article.title.starts_with("笔记 20"));
    }

    /* ── pending state ─────────────────────────────────────────────── */

    #[test]
    fn pending_queue_round_trips_and_bounds() {
        let conn = crate::db::test_conn();
        let mut state = XhsState::load(&conn);
        for i in 0..40 {
            state.enqueue(
                1,
                XhsPendingNote {
                    note_id: format!("note{i:024}"),
                    xsec_token: None,
                    guid: format!("g{i}"),
                },
            );
        }
        assert_eq!(state.pending.get(&1).unwrap().len(), MAX_PENDING_PER_FEED);
        state.save(&conn);
        let mut loaded = XhsState::load(&conn);
        assert_eq!(loaded.pending.get(&1).unwrap().len(), MAX_PENDING_PER_FEED);
        let taken = loaded.take(1, 10);
        assert_eq!(taken.len(), 10);
        assert_eq!(taken[0].note_id, format!("note{:024}", 40 - MAX_PENDING_PER_FEED));
    }

    #[test]
    fn detail_fetchable_requires_cookie_and_ids() {
        let with_id = XhsNote {
            note_id: Some("a".into()),
            xsec_token: Some("t".into()),
            title: "t".into(),
            kind: None,
            time_ms: None,
            cover: None,
            author: None,
            liked_count: None,
        };
        assert!(detail_fetchable(&with_id, Some("cookie")));
        assert!(!detail_fetchable(&with_id, None));
        let no_id = XhsNote {
            note_id: None,
            ..with_id.clone()
        };
        assert!(!detail_fetchable(&no_id, Some("cookie")));
    }
}
