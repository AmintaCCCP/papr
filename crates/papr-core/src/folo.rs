//! Folo shared-list import — bring the feeds of a Folo「列表」into papr.
//!
//! A Folo list is a curated set of feeds shareable at
//! `https://app.folo.is/share/lists/<id>`. Behind that page sits a public,
//! unauthenticated endpoint — `GET https://api.folo.is/lists?listId=<id>` —
//! whose response embeds the list metadata and its feeds, each carrying its
//! original subscription URL in Folo's internal notation (`rsshub://<route>`
//! short links or plain `http(s)://` URLs).
//!
//! Two upstream facts shape this module:
//! - the anonymous response is capped by the backend to the first ~5 feeds
//!   (the share page itself comments `// Backend limit` and links "follow to
//!   view all"); a `feedCount` greater than the embedded array means
//!   truncation. An authenticated request (`Authorization: Bearer <token>`,
//!   the token from a logged-in Folo session) lifts the cap.
//! - `noExtras=true` drops the response's heavy `entries` preview while
//!   keeping `feeds` and `feedCount`.
//!
//! Import mirrors the OPML path: convert each feed URL into papr's own
//! notation, insert in one transaction, and let the refresh machinery pick
//! the right leg per `source_type`. Twitter profile routes may optionally be
//! rewritten to `https://x.com/<handle>` so the native X parser serves them
//! instead of the (credential-hungry) RSSHub Twitter routes.

use crate::db;
use crate::error::{AppError, AppResult};
use crate::ingestion::fetch;
use crate::ingestion::parsers::x;
use crate::ingestion::parse;
use crate::ingestion::rsshub;
use crate::opml::ImportedFeed;
use reqwest::Client;
use reqwest::header::ACCEPT;
use reqwest::header::AUTHORIZATION;
use reqwest::StatusCode;
use serde::Deserialize;
use url::Url;

const API_BASE: &str = "https://api.folo.is";

/// Hosts that serve Folo list pages — the current domain plus the legacy
/// `follow.is` one still printed by older share links.
const LIST_HOSTS: &[&str] = &["app.folo.is", "folo.is", "app.follow.is", "follow.is"];

// ─────────────────────────── list reference ───────────────────────────

/// Extract a numeric list id from any shape a user might paste:
///
/// - a share link (`https://app.folo.is/share/lists/<id>?view=1`) or the
///   legacy `follow.is` equivalent,
/// - the web app's timeline deep path (`…/timeline/view-1/list-<id>/pending`),
/// - the `follow://add?id=<id>&type=list` deeplink the share page advertises
///   to the mobile app,
/// - a bare numeric id.
pub fn parse_list_ref(input: &str) -> Option<String> {
    let raw = input.trim();
    // A bare snowflake id: digits only, long enough to not be a stray number.
    if raw.len() >= 6 && raw.chars().all(|c| c.is_ascii_digit()) {
        return Some(raw.to_string());
    }
    let parsed = Url::parse(raw).ok()?;
    match parsed.scheme() {
        "http" | "https" => {
            let host = parsed.host_str()?.to_lowercase();
            if !LIST_HOSTS.contains(&host.as_str()) {
                return None;
            }
            // Segment names are matched case-insensitively (ids are digits,
            // so lowercasing cannot corrupt them); real links are lowercase
            // but a hand-typed one need not be.
            let segments: Vec<String> = parsed
                .path()
                .split('/')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_ascii_lowercase())
                .collect();
            id_from_path(&segments.iter().map(String::as_str).collect::<Vec<_>>())
        }
        "follow" => parsed
            .query()?
            .split('&')
            .find_map(|kv| kv.strip_prefix("id="))
            .map(str::to_string)
            .filter(|id| is_list_id(id)),
        _ => None,
    }
}

/// Match the id out of a list page's path segments.
fn id_from_path(segments: &[&str]) -> Option<String> {
    match segments {
        ["share", "lists", id] => is_list_id(id).then(|| id.to_string()),
        ["list", id] => is_list_id(id).then(|| id.to_string()),
        // The web app nests lists inside a timeline view path.
        _ => segments
            .iter()
            .find_map(|s| s.strip_prefix("list-"))
            .filter(|id| is_list_id(id))
            .map(str::to_string),
    }
}

/// A syntactically plausible list id — numeric, at least a few digits.
fn is_list_id(id: &str) -> bool {
    id.len() >= 6 && id.chars().all(|c| c.is_ascii_digit())
}

// ─────────────────────────── API fetch ───────────────────────────

/// One feed embedded in the list response.
#[derive(Debug, Clone)]
pub struct FoloFeed {
    /// The original subscription URL in Folo's notation (`rsshub://…` or
    /// `http(s)://…`).
    pub url: String,
    pub title: String,
    pub description: Option<String>,
    pub site_url: Option<String>,
}

/// A Folo list as the API returned it.
#[derive(Debug, Clone)]
pub struct FoloList {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub image: Option<String>,
    pub owner_name: Option<String>,
    pub subscription_count: u64,
    /// Total feeds in the list per the API's counter.
    pub feed_count: usize,
    /// The feeds embedded in this response — anonymously capped to the first
    /// few, so this can be shorter than [`FoloList::feed_count`].
    pub feeds: Vec<FoloFeed>,
}

/// `true` when the response embedded fewer feeds than the list holds — the
/// anonymous backend cap. Callers surface this so the user knows an import
/// is partial.
impl FoloList {
    pub fn truncated(&self) -> bool {
        self.feed_count > self.feeds.len()
    }
}

// Wire shapes — the API answers `{"code":0,"data":{…}}` with camelCase keys.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListsResponse {
    #[serde(default)]
    data: Option<ListData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListData {
    list: Option<ListPayload>,
    #[serde(default)]
    subscription_count: u64,
    #[serde(default)]
    feed_count: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPayload {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    feed_ids: Vec<String>,
    #[serde(default)]
    feeds: Vec<FeedPayload>,
    #[serde(default)]
    owner: Option<OwnerPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedPayload {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    site_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnerPayload {
    #[serde(default)]
    name: Option<String>,
}

/// Fetch a Folo list by any reference [`parse_list_ref`] accepts. `token` is
/// an optional Folo access token — with it the API returns the list's full
/// feed membership, without it only the first few feeds.
///
/// `noExtras=true` keeps the heavy `entries` preview out of the response;
/// `feeds` and the counters still come through.
pub async fn fetch_list(client: &Client, input: &str, token: Option<&str>) -> AppResult<FoloList> {
    // The id is digits, so the query string is injection-proof as built.
    let id = parse_list_ref(input).ok_or_else(|| AppError::code("foloInvalidRef"))?;
    let url = format!("{API_BASE}/lists?listId={id}&noExtras=true");
    let mut req = client.get(&url).header(ACCEPT, "application/json");
    let mut has_token = false;
    if let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) {
        req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        has_token = true;
    }
    let resp = req.send().await?;
    match resp.status() {
        // A 401/403 only blames the token when one was actually sent — the
        // endpoint is public, so an unauthenticated rejection means something
        // else entirely (rate limit, Cloudflare, …).
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if has_token => {
            return Err(AppError::code("foloTokenRejected"));
        }
        StatusCode::NOT_FOUND => return Err(AppError::code("foloListNotFound")),
        s if !s.is_success() => {
            return Err(AppError::other(format!("folo api error: HTTP {}", s.as_u16())));
        }
        _ => {}
    }
    let bytes = fetch::read_capped(resp).await?;
    let parsed: ListsResponse = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::other(format!("invalid folo response: {e}")))?;
    let data = parsed.data.ok_or_else(|| AppError::code("foloListNotFound"))?;
    let list = data.list.ok_or_else(|| AppError::code("foloListNotFound"))?;
    let feeds: Vec<FoloFeed> = list
        .feeds
        .into_iter()
        .filter(|f| !f.url.trim().is_empty())
        .map(|f| FoloFeed {
            url: f.url.trim().to_string(),
            title: f.title.unwrap_or_default(),
            description: f.description,
            site_url: f.site_url,
        })
        .collect();
    Ok(FoloList {
        // A malformed payload without an id still imported nothing, so the
        // echo back is cosmetic — fall back to the parsed reference.
        id: if list.id.is_empty() { id } else { list.id },
        title: list.title.unwrap_or_default(),
        description: list.description,
        image: list.image,
        owner_name: list.owner.and_then(|o| o.name),
        subscription_count: data.subscription_count,
        // The counter is authoritative; only fall back to what arrived when
        // the API omitted it entirely.
        feed_count: if data.feed_count > 0 {
            data.feed_count as usize
        } else {
            list.feed_ids.len().max(feeds.len())
        },
        feeds,
    })
}

// ─────────────────────────── URL conversion ───────────────────────────

/// What one Folo feed URL becomes on papr's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedConversion {
    /// A `rsshub://twitter/user/*` route rewritten to the native X parser's
    /// `https://x.com/<handle>` form.
    Mapped(String),
    /// Imported unchanged — plain `http(s)://` URLs and every other
    /// `rsshub://` route (served by the local RSSHub engine).
    Keep(String),
    /// No papr equivalent — skipped by imports, reported by previews.
    Unsupported,
}

/// Convert one feed URL from Folo's notation to papr's.
pub fn convert_feed_url(folo_url: &str, map_twitter: bool) -> FeedConversion {
    let url = folo_url.trim();
    if url.is_empty() {
        return FeedConversion::Unsupported;
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return FeedConversion::Keep(url.to_string());
    }
    if let Some(route) = rsshub::route_from_url(url) {
        if map_twitter {
            if let Some(mapped) = map_twitter_route(&route) {
                return FeedConversion::Mapped(mapped);
            }
        }
        return FeedConversion::Keep(url.to_string());
    }
    // Everything else (xurl://, mail://, inbox://, …) has no papr equivalent.
    FeedConversion::Unsupported
}

/// Rewrite `twitter/user/<handle>` to the native X parser's profile form.
/// Only the profile route maps — keyword, list, media and sibling Twitter
/// routes have no parser equivalent and stay engine routes. A route carrying
/// a query string (an RSSHub filter like `exclude_replies` or `limit`) also
/// stays an engine route: the rewrite would silently drop the filter. The X
/// parser's own handle validation gates the rewrite, so a route that could
/// never be a real profile keeps its `rsshub://` form.
fn map_twitter_route(route: &str) -> Option<String> {
    if route.contains('?') {
        return None;
    }
    let segments: Vec<&str> = route.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 3
        || !segments[0].eq_ignore_ascii_case("twitter")
        || !segments[1].eq_ignore_ascii_case("user")
    {
        return None;
    }
    let profile = Url::parse(&format!("https://x.com/{}", segments[2])).ok()?;
    x::normalize_x_url(&profile)
}

// ─────────────────────────── import ───────────────────────────

/// The title a feed row displays — Folo leaves some feeds untitled, and an
/// empty name in the sidebar helps nobody, so the URL stands in. Shared by
/// the preview card and the import rows so the two never disagree.
pub fn display_title(feed: &FoloFeed) -> &str {
    let title = feed.title.trim();
    if title.is_empty() { feed.url.as_str() } else { title }
}

/// Convert a fetched list into OPML-shaped rows ready for
/// [`import_feeds`]. Returns the importable rows plus the count of feeds
/// whose URL had no papr equivalent.
///
/// `folder` is applied to every row (the list's natural grouping); pass
/// `None` to leave them ungrouped.
pub fn to_imported(
    list: &FoloList,
    map_twitter: bool,
    folder: Option<String>,
) -> (Vec<ImportedFeed>, usize) {
    let mut imported = Vec::new();
    let mut unsupported = 0;
    for feed in &list.feeds {
        match convert_feed_url(&feed.url, map_twitter) {
            FeedConversion::Mapped(url) | FeedConversion::Keep(url) => {
                imported.push(ImportedFeed {
                    feed_url: url,
                    title: display_title(feed).to_string(),
                    folder: folder.clone(),
                });
            }
            FeedConversion::Unsupported => unsupported += 1,
        }
    }
    (imported, unsupported)
}

/// Bulk-insert import rows — the OPML import loop, shared by the desktop
/// command and the CLI. One transaction for the whole import (a mid-list
/// failure rolls back rather than leaving feeds and auto-created folders
/// partly imported); already-subscribed URLs are skipped.
///
/// `folder_id` overrides every row's folder (the folder picker selects by
/// id); when `None`, each row's folder *name* is resolved — creating it —
/// or the row lands ungrouped.
///
/// Returns `(added, skipped)`.
pub fn import_feeds(
    conn: &rusqlite::Connection,
    feeds: &[ImportedFeed],
    folder_id: Option<i64>,
) -> AppResult<(usize, usize)> {
    let tx = conn.unchecked_transaction()?;
    let mut added = 0;
    let mut skipped = 0;
    for feed in feeds {
        if db::find_feed_by_url(&tx, &feed.feed_url)?.is_some() {
            skipped += 1;
            continue;
        }
        let folder_id = match folder_id {
            Some(id) => Some(id),
            None => match &feed.folder {
                Some(name) => Some(db::folder_id_by_name(&tx, name)?),
                None => None,
            },
        };
        let source_type = parse::detect_source_type(&feed.feed_url);
        db::insert_feed(
            &tx,
            &feed.feed_url,
            None,
            &feed.title,
            None,
            source_type,
            folder_id,
        )?;
        added += 1;
    }
    tx.commit()?;
    Ok((added, skipped))
}

#[cfg(test)]
mod tests {
    use super::{convert_feed_url, parse_list_ref, to_imported, FeedConversion, FoloFeed, FoloList};

    #[test]
    fn parses_share_links() {
        assert_eq!(
            parse_list_ref("https://app.folo.is/share/lists/69107368310609920?view=1"),
            Some("69107368310609920".to_string())
        );
        assert_eq!(
            parse_list_ref("https://app.follow.is/share/lists/69107368310609920"),
            Some("69107368310609920".to_string())
        );
        // Case-insensitive scheme/host, trailing slash, plain /list/<id>.
        assert_eq!(
            parse_list_ref("HTTPS://APP.FOLO.IS/SHARE/LISTS/69107368310609920/"),
            Some("69107368310609920".to_string())
        );
        assert_eq!(
            parse_list_ref("https://app.folo.is/list/69107368310609920"),
            Some("69107368310609920".to_string())
        );
    }

    #[test]
    fn parses_timeline_deep_path() {
        assert_eq!(
            parse_list_ref("https://app.folo.is/timeline/view-1/list-69107368310609920/pending"),
            Some("69107368310609920".to_string())
        );
    }

    #[test]
    fn parses_deeplink_and_bare_id() {
        assert_eq!(
            parse_list_ref("follow://add?id=69107368310609920&type=list"),
            Some("69107368310609920".to_string())
        );
        assert_eq!(
            parse_list_ref(" 69107368310609920 "),
            Some("69107368310609920".to_string())
        );
    }

    #[test]
    fn rejects_non_list_input() {
        assert_eq!(parse_list_ref(""), None);
        assert_eq!(parse_list_ref("https://github.com/RSSNext/Folo"), None);
        assert_eq!(parse_list_ref("https://app.folo.is/share/lists/abc123"), None);
        assert_eq!(parse_list_ref("follow://add?type=list"), None);
        assert_eq!(parse_list_ref("xurl://twitter/user/a"), None);
        assert_eq!(parse_list_ref("12345"), None);
    }

    #[test]
    fn converts_twitter_user_routes_to_native_x() {
        assert_eq!(
            convert_feed_url("rsshub://twitter/user/shao__meng", true),
            FeedConversion::Mapped("https://x.com/shao__meng".to_string())
        );
        // Case-insensitive route prefix; handle lowercased by the X parser.
        assert_eq!(
            convert_feed_url("rsshub://Twitter/User/SomeBody", true),
            FeedConversion::Mapped("https://x.com/somebody".to_string())
        );
        // Mapping off keeps the engine route.
        assert_eq!(
            convert_feed_url("rsshub://twitter/user/shao__meng", false),
            FeedConversion::Keep("rsshub://twitter/user/shao__meng".to_string())
        );
    }

    #[test]
    fn keeps_routes_without_a_parser_equivalent() {
        // Non-profile Twitter routes have no native parser form.
        assert_eq!(
            convert_feed_url("rsshub://twitter/keyword/rust", true),
            FeedConversion::Keep("rsshub://twitter/keyword/rust".to_string())
        );
        // Reserved X paths would never be a real profile either.
        assert_eq!(
            convert_feed_url("rsshub://twitter/user/home", true),
            FeedConversion::Keep("rsshub://twitter/user/home".to_string())
        );
        // Any other RSSHub route flows to the local engine unchanged.
        assert_eq!(
            convert_feed_url("rsshub://bilibili/user/video/2267573", true),
            FeedConversion::Keep("rsshub://bilibili/user/video/2267573".to_string())
        );
    }

    #[test]
    fn keeps_filtered_twitter_routes_whole() {
        // A query is the user's RSSHub filter (exclude_replies, limit, …) —
        // rewriting to x.com would silently drop it, so the route stays an
        // engine route, query included.
        assert_eq!(
            convert_feed_url("rsshub://twitter/user/shao__meng?exclude_replies=1", true),
            FeedConversion::Keep(
                "rsshub://twitter/user/shao__meng?exclude_replies=1".to_string()
            )
        );
    }

    #[test]
    fn classifies_plain_and_foreign_urls() {
        assert_eq!(
            convert_feed_url("https://example.com/feed.xml", true),
            FeedConversion::Keep("https://example.com/feed.xml".to_string())
        );
        assert_eq!(
            convert_feed_url("xurl://example.com/feed", true),
            FeedConversion::Unsupported
        );
        assert_eq!(convert_feed_url("", true), FeedConversion::Unsupported);
    }

    #[test]
    fn to_imported_maps_and_counts_unsupported() {
        let list = FoloList {
            id: "1".to_string(),
            title: "AI资讯".to_string(),
            description: None,
            image: None,
            owner_name: None,
            subscription_count: 2206,
            feed_count: 4,
            feeds: vec![
                FoloFeed {
                    url: "rsshub://twitter/user/shao__meng".to_string(),
                    title: "Twitter @meng shao".to_string(),
                    description: None,
                    site_url: None,
                },
                FoloFeed {
                    url: "https://example.com/feed.xml".to_string(),
                    title: "  ".to_string(), // title falls back to the URL
                    description: None,
                    site_url: None,
                },
                FoloFeed {
                    url: "xurl://nope".to_string(),
                    title: "Mystery".to_string(),
                    description: None,
                    site_url: None,
                },
            ],
        };
        let (imported, unsupported) = to_imported(&list, true, Some("Folo".to_string()));
        assert_eq!(unsupported, 1);
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[0].feed_url, "https://x.com/shao__meng");
        assert_eq!(imported[0].title, "Twitter @meng shao");
        assert_eq!(imported[0].folder.as_deref(), Some("Folo"));
        assert_eq!(imported[1].feed_url, "https://example.com/feed.xml");
        assert_eq!(imported[1].title, "https://example.com/feed.xml");
    }
}
