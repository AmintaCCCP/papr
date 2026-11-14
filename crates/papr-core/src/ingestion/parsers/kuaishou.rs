//! Built-in Kuaishou parser: turns a creator profile URL into feed data from
//! the www.kuaishou.com GraphQL web API.
//!
//! Upstream reality (verified 2026-11 against the live site):
//! - The profile page itself is a client-rendered SPA shell — the SSR
//!   document carries no feed data.
//! - The SPA reads the feed from `POST /graphql`
//!   (`visionProfile` / `visionProfilePhotoList`), the same operations the
//!   web client sends.
//! - Anonymous traffic to those operations is captcha-gated (`result: 400002`,
//!   `bizName: ANTICRAWL_DEFAULT`) — the SPA survives it because the web
//!   client's own signed REST endpoints (`/rest/v/profile/feed?__NS_hxfalcon=…`)
//!   run inside the page's JS signing realm, a VM-obfuscated signer no plain
//!   HTTP client can reproduce faithfully.
//!
//! So this parser runs one request shape: the unsigned GraphQL operations
//! with the full browser XHR header group. Anonymous mode tries it first and
//! surfaces the captcha as a distinct coded error; pasting a logged-in
//! `Cookie` (Settings → Parsers) is the supported path, mirroring the X
//! parser's credential model. For a cookie-free setup, the RSSHub route
//! marketplace's browser-flavor engine can run the signed flow headlessly —
//! the error copy says so.

use crate::error::AppError;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use url::Url;

use super::transport::{browser_api_headers, is_host_or_subdomain};

/// The site every kuaishou request must stay on.
const REGISTRABLE_DOMAIN: &str = "kuaishou.com";
/// The host the GraphQL API lives on.
const WEB_HOST: &str = "www.kuaishou.com";

/// Pause between per-feed fetches (the refresh leg adds jitter).
pub const FEED_THROTTLE_MS: u64 = 900;

/// Coded errors the frontend localises under `error.<code>`.
pub const CODE_CAPTCHA: &str = "kuaishouCaptcha";
pub const CODE_ACCOUNT_UNAVAILABLE: &str = "kuaishouAccountUnavailable";
pub const CODE_BLOCKED: &str = "kuaishouBlocked";
pub const CODE_HANDLE_INVALID: &str = "kuaishouHandleInvalid";

/* ── errors ──────────────────────────────────────────────────────────── */

/// Everything that can go wrong while fetching one profile.
#[derive(Debug, thiserror::Error)]
pub enum KsError {
    /// `result: 400002` — the anti-crawl captcha. Anonymous traffic hits
    /// this by design; a stale cookie can too, so the message covers both.
    #[error("captcha")]
    Captcha,
    /// `result: 2/50` — rate-limited, risk-controlled or signature-required.
    #[error("blocked")]
    Blocked,
    /// The creator is gone, banned, or the profile is private.
    #[error("account unavailable: {0}")]
    Unavailable(String),
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

impl KsError {
    /// The frontend i18n key for this failure, when one exists.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            KsError::Captcha => Some(CODE_CAPTCHA),
            KsError::Blocked => Some(CODE_BLOCKED),
            KsError::Unavailable(_) => Some(CODE_ACCOUNT_UNAVAILABLE),
            KsError::Upstream(_) | KsError::Transport(_) | KsError::Parse(_) => None,
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

impl From<reqwest::Error> for KsError {
    fn from(e: reqwest::Error) -> Self {
        KsError::Transport(e.to_string())
    }
}

pub type KsResult<T> = Result<T, KsError>;

/* ── URL / profile-id normalisation (pure) ───────────────────────────── */

/// True for a syntactically valid Kuaishou principal id: 10–24 lowercase
/// letters/digits (the web profile path's format, e.g. `3x4kuwfd24jzzf4`).
pub fn is_valid_profile_id(id: &str) -> bool {
    (10..=24).contains(&id.len())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Recognize a pasted profile URL — `https://www.kuaishou.com/profile/<id>`
/// or `https://live.kuaishou.com/profile/<id>` (both hosts serve the same
/// creator) — and rewrite it to the canonical feed URL
/// `https://www.kuaishou.com/profile/<id>`. Share attribution (`?fid=…`,
/// `shareToken`…) and the app's `v.kuaishou.com` short links are handled by
/// the caller: the short link needs a redirect follow, so it is classified
/// as [`super::super::sources::Normalized::NeedsKuaishouResolution`].
///
/// Pure and unit-tested.
pub fn normalize_kuaishou_url(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_lowercase();
    if host != "www.kuaishou.com" && host != "live.kuaishou.com" && host != "kuaishou.com" {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 2 || segments[0] != "profile" {
        return None;
    }
    let id = segments[1].to_lowercase();
    if !is_valid_profile_id(&id) {
        return None;
    }
    Some(format!("https://{WEB_HOST}/profile/{id}"))
}

/// Recognize an app share short link (`https://v.kuaishou.com/<token>`).
/// `true` only for the exact host and a single path segment — resolution
/// (redirect follow) happens in `add_feed`, never here.
pub fn is_kuaishou_short_url(url: &Url) -> bool {
    url.host_str()
        .map(|h| h.to_lowercase() == "v.kuaishou.com")
        .unwrap_or(false)
        && url
            .path()
            .split('/')
            .filter(|s| !s.is_empty())
            .count()
            == 1
}

/// Recover the profile id from a stored canonical feed URL. The inverse of
/// [`normalize_kuaishou_url`] restricted to canonical URLs.
pub fn profile_id_from_feed_url(feed_url: &str) -> Option<String> {
    let url = Url::parse(feed_url).ok()?;
    if url.host_str()?.to_lowercase() != WEB_HOST {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 2 || segments[0] != "profile" {
        return None;
    }
    let id = segments[1];
    (is_valid_profile_id(id) && url.query().is_none()).then(|| id.to_string())
}

/// Accept free-form input from the settings test field: a bare id (case
/// insensitive — ids are lowercased) or a profile URL (www or live host).
/// Short links are rejected — they need a redirect the pure function cannot
/// do.
pub fn sanitize_profile_input(input: &str) -> Option<String> {
    let trimmed = input.trim().to_lowercase();
    if is_valid_profile_id(&trimmed) {
        return Some(trimmed);
    }
    if trimmed.contains('.') || trimmed.contains('/') {
        let with_scheme = crate::ingestion::discovery::normalize_query_url(&trimmed);
        if let Ok(url) = Url::parse(&with_scheme) {
            return normalize_kuaishou_url(&url)
                .as_deref()
                .and_then(profile_id_from_feed_url);
        }
    }
    None
}

/* ── GraphQL request bodies (pure) ───────────────────────────────────── */

/// The `visionProfile` operation — creator metadata (nickname, avatar, bio).
pub fn build_vision_profile_body(user_id: &str) -> Value {
    json!({
        "operationName": "visionProfile",
        "variables": { "userId": user_id },
        "query": "query visionProfile($userId: String) { visionProfile(userId: $userId) { result hostName userProfile { ownerCount { fan photo follow photo_public } profile { gender user_name user_id headurl user_text user_profile_bg_url } isFollowing } } }",
    })
}

/// The `visionProfilePhotoList` operation, page 1. The periodic-refresh model
/// re-pulls the newest page each cycle and dedupes by photo id — the `pcursor`
/// is parsed but never followed.
pub fn build_photo_list_body(user_id: &str) -> Value {
    json!({
        "operationName": "visionProfilePhotoList",
        "variables": { "page": "profile", "pcursor": "", "userId": user_id },
        "query": PHOTO_LIST_QUERY,
    })
}

/// The feed-list query, verbatim the web client's own shape (ported from the
/// battle-tested open-source clients). Its selection set is the contract the
/// parser below walks; widening it risks schema errors that fail the whole
/// operation.
const PHOTO_LIST_QUERY: &str = "query visionProfilePhotoList($pcursor: String, $userId: String, $page: String, $webPageArea: String) { visionProfilePhotoList(pcursor: $pcursor, userId: $userId, page: $page, webPageArea: $webPageArea) { result llsid webPageArea feeds { type author { id name headerUrl } photo { id duration caption originCaption likeCount viewCount commentCount realLikeCount coverUrl photoUrl coverUrls { url } timestamp expTag animatedCoverUrl distance videoRatio liked stereoType profileUserTopPhoto musicBlocked } canAddComment llsid status currentPcursor tags { type name } } hostName pcursor } }";

/* ── parsed shapes ───────────────────────────────────────────────────── */

/// The creator resolved via `visionProfile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KsProfile {
    pub user_id: String,
    pub name: String,
    pub avatar: Option<String>,
    pub bio: Option<String>,
    pub fan: Option<String>,
}

/// One work mapped for ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KsPhoto {
    /// Photo id — the stable dedup key (`articles.guid`).
    pub id: String,
    pub caption: String,
    /// Author display name as shipped on the feed row.
    pub author: Option<String>,
    /// Best cover (`coverUrl`, else the first `coverUrls` entry).
    pub cover: Option<String>,
    /// The mp4 URL, when the work is a video.
    pub video: Option<String>,
    /// Publish time, epoch milliseconds.
    pub timestamp_ms: Option<i64>,
    /// Like counter, as shipped (raw number or `-1` when hidden).
    pub like_count: Option<String>,
}

/// One creator's page: metadata plus the newest works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KsUserPage {
    pub profile: KsProfile,
    pub photos: Vec<KsPhoto>,
}

/// Map a GraphQL `result` code onto the error taxonomy.
fn result_to_error(result: i64) -> KsError {
    match result {
        400002 => KsError::Captcha,
        2 | 50 => KsError::Blocked,
        _ => KsError::Unavailable(format!("result {result}")),
    }
}

/// Pull the operation payload out of a GraphQL response. Kuaishou wraps
/// success under `data.<operation>`; failures collapse to `data.result` (the
/// captcha shape) without the operation key. Top-level fallbacks keep the
/// parser total across both shapes.
fn operation_payload<'a>(response: &'a Value, operation: &str) -> Option<&'a Value> {
    if let Some(op) = response.pointer(&format!("/data/{operation}")) {
        return Some(op);
    }
    if let Some(op) = response.get(operation) {
        return Some(op);
    }
    response.pointer("/data").or(Some(response))
}

/// Parse the `visionProfile` response.
pub fn parse_profile_response(response: &str) -> KsResult<KsProfile> {
    let value: Value = serde_json::from_str(response)
        .map_err(|e| KsError::Parse(format!("profile json: {e}")))?;
    let payload = operation_payload(&value, "visionProfile");
    let result = payload
        .and_then(|p| p.get("result"))
        .and_then(|r| r.as_i64())
        .unwrap_or(0);
    if result != 1 {
        return Err(result_to_error(result));
    }
    let profile = payload
        .and_then(|p| p.pointer("/userProfile/profile"))
        .ok_or_else(|| KsError::Unavailable("visionProfile returned no userProfile".into()))?;
    let user_id = profile
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let name = profile
        .get("user_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(KsError::Unavailable("profile has no nickname".into()));
    }
    Ok(KsProfile {
        user_id,
        name,
        avatar: profile
            .get("headurl")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        bio: profile
            .get("user_text")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        fan: payload
            .and_then(|p| p.pointer("/userProfile/ownerCount/fan"))
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string()),
    })
}

/// Parse the `visionProfilePhotoList` response into typed photos.
pub fn parse_photo_list_response(response: &str) -> KsResult<(Vec<KsPhoto>, String)> {
    let value: Value = serde_json::from_str(response)
        .map_err(|e| KsError::Parse(format!("photo list json: {e}")))?;
    let payload = operation_payload(&value, "visionProfilePhotoList");
    let result = payload
        .and_then(|p| p.get("result"))
        .and_then(|r| r.as_i64())
        .unwrap_or(0);
    if result != 1 {
        return Err(result_to_error(result));
    }
    let pcursor = payload
        .and_then(|p| p.get("pcursor"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let mut photos = Vec::new();
    if let Some(feeds) = payload.and_then(|p| p.get("feeds")).and_then(|f| f.as_array()) {
        for feed in feeds {
            let photo = feed.get("photo");
            let id = photo
                .and_then(|p| p.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                continue;
            }
            let caption = photo
                .and_then(|p| p.get("caption").or_else(|| p.get("originCaption")))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let cover = photo
                .and_then(|p| p.get("coverUrl"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| {
                    photo
                        .and_then(|p| p.get("coverUrls"))
                        .and_then(|v| v.as_array())
                        .and_then(|list| list.first())
                        .and_then(|c| c.get("url"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                });
            photos.push(KsPhoto {
                id,
                caption,
                author: feed
                    .get("author")
                    .and_then(|a| a.get("name"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                cover,
                video: photo
                    .and_then(|p| p.get("photoUrl"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                timestamp_ms: photo
                    .and_then(|p| p.get("timestamp"))
                    .and_then(|v| v.as_i64()),
                like_count: photo
                    .and_then(|p| {
                        p.get("realLikeCount").or_else(|| p.get("likeCount"))
                    })
                    .and_then(|v| v.as_i64())
                    .filter(|n| *n >= 0)
                    .map(|n| n.to_string()),
            });
        }
    }
    Ok((photos, pcursor))
}

/* ── dates / article builders (pure) ─────────────────────────────────── */

/// Epoch milliseconds → RFC 3339.
pub fn ms_to_rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
}

fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Map one work to the ingestion shape: video player (with poster) when the
/// payload carries an mp4, cover image otherwise, caption below.
pub fn photo_to_article(photo: &KsPhoto, profile_url: &str) -> crate::db::NewArticle {
    let permalink = format!("https://{WEB_HOST}/short-video/{}", photo.id);
    let sanitize_base = permalink.clone();
    let mut html = String::new();
    match (&photo.video, &photo.cover) {
        (Some(video), Some(poster)) => {
            html.push_str(&format!(
                "<p><video controls preload=\"metadata\" poster=\"{}\"><source src=\"{}\" type=\"video/mp4\"></video></p>",
                escape_html_text(poster),
                escape_html_text(video)
            ));
        }
        (Some(video), None) => {
            html.push_str(&format!(
                "<p><video controls preload=\"metadata\"><source src=\"{}\" type=\"video/mp4\"></video></p>",
                escape_html_text(video)
            ));
        }
        (None, Some(cover)) => {
            html.push_str(&format!(
                "<p><img src=\"{}\" alt=\"{}\"></p>",
                escape_html_text(cover),
                escape_html_text(&photo.caption)
            ));
        }
        (None, None) => {}
    }
    if !photo.caption.is_empty() {
        html.push_str(&format!(
            "<p>{}</p>",
            escape_html_text(&photo.caption)
                .split('\n')
                .collect::<Vec<_>>()
                .join("<br>")
        ));
    }
    let _ = profile_url;
    crate::db::NewArticle {
        guid: photo.id.clone(),
        url: Some(permalink),
        title: if photo.caption.is_empty() {
            match photo.timestamp_ms.and_then(ms_to_rfc3339) {
                Some(ts) => format!("作品 {}", &ts[..10.min(ts.len())]),
                None => "无标题作品".into(),
            }
        } else {
            photo.caption.clone()
        },
        author: photo.author.clone(),
        summary: Some(photo.caption.clone()),
        content_html: Some(crate::sanitize::sanitize_parser(
            &html,
            Some(&sanitize_base),
        )),
        body_text: photo.caption.clone(),
        image_url: photo.cover.clone(),
        published_at: photo.timestamp_ms.and_then(ms_to_rfc3339),
        enclosures: photo.video.as_ref().map(|video| {
            vec![crate::models::Enclosure {
                url: video.clone(),
                mime_type: Some("video/mp4".into()),
                length: None,
            }]
        }).unwrap_or_default(),
    }
}

/* ── network transport ───────────────────────────────────────────────── */

/// POST one GraphQL operation with the browser XHR header group. The shared
/// client honours the app's proxy settings; cookies ride only in the explicit
/// header. The final URL must still belong to kuaishou.com.
async fn graphql(
    client: &Client,
    body: &Value,
    referer: &str,
    cookie: Option<&str>,
) -> KsResult<String> {
    let mut req = client.post(format!("https://{WEB_HOST}/graphql").as_str());
    for (name, value) in browser_api_headers(WEB_HOST, referer, cookie) {
        req = req.header(name, value);
    }
    let resp = req.json(body).send().await?;
    if !is_host_or_subdomain(resp.url(), REGISTRABLE_DOMAIN) {
        return Err(KsError::Transport(format!(
            "redirect to non-kuaishou host blocked: {}",
            resp.url()
        )));
    }
    match resp.status() {
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => return Err(KsError::Blocked),
        s if !s.is_success() => return Err(KsError::Upstream(s.as_u16())),
        _ => {}
    }
    let bytes = crate::ingestion::fetch::read_capped(resp)
        .await
        .map_err(|e| KsError::Transport(e.to_string()))?;
    String::from_utf8(bytes).map_err(|_| KsError::Parse("response is not UTF-8".into()))
}

/// Fetch one creator's metadata + newest works page.
///
/// `cookie` (optional) is the pasted logged-in Cookie header; without it the
/// captcha-gate typically answers `Captcha` — that is the coded, localisable
/// signal to configure one.
pub async fn fetch_creator_page(
    client: &Client,
    user_id: &str,
    cookie: Option<&str>,
) -> KsResult<KsUserPage> {
    let referer = format!("https://{WEB_HOST}/profile/{user_id}");
    let (photos, _) = {
        let response = graphql(client, &build_photo_list_body(user_id), &referer, cookie).await?;
        parse_photo_list_response(&response)?
    };
    // The photo list already carries per-row author names; `visionProfile`
    // only pins down the nickname/avatar for the feed row itself. The
    // metadata call's failure is tolerable only while the list actually
    // yielded works to fall back on — an empty list plus a captcha must
    // propagate, or subscribing would "succeed" with a nameless feed.
    let author_from_list = photos.first().and_then(|p| p.author.clone());
    let fallback_profile = || KsProfile {
        user_id: user_id.to_string(),
        name: author_from_list.clone().unwrap_or_else(|| user_id.to_string()),
        avatar: None,
        bio: None,
        fan: None,
    };
    let profile = match graphql(client, &build_vision_profile_body(user_id), &referer, cookie).await
    {
        Ok(response) => match parse_profile_response(&response) {
            Ok(profile) => profile,
            Err(e) => {
                if photos.is_empty() {
                    return Err(e);
                }
                fallback_profile()
            }
        },
        Err(e) => {
            if photos.is_empty() {
                return Err(e);
            }
            fallback_profile()
        }
    };
    Ok(KsUserPage { profile, photos })
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ── URL normalisation ─────────────────────────────────────────── */

    fn parse(url: &str) -> Url {
        Url::parse(&crate::ingestion::discovery::normalize_query_url(url)).unwrap()
    }

    #[test]
    fn normalizes_www_and_live_profile_urls() {
        for url in [
            "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4",
            "https://live.kuaishou.com/profile/3x4kuwfd24jzzf4",
            "kuaishou.com/profile/3x4kuwfd24jzzf4",
            "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4?fid=123&shareToken=X-1",
        ] {
            assert_eq!(
                normalize_kuaishou_url(&parse(url)).as_deref(),
                Some("https://www.kuaishou.com/profile/3x4kuwfd24jzzf4"),
                "{url}"
            );
        }
    }

    #[test]
    fn rejects_other_pages_and_bad_ids() {
        for url in [
            "https://www.kuaishou.com/short-video/3x4kuwfd24jzzf4",
            "https://www.kuaishou.com/profile/short",
            "https://www.kuaishou.com/profile/UPPERCASE!",
            "https://evil.com/profile/3x4kuwfd24jzzf4",
        ] {
            assert!(normalize_kuaishou_url(&parse(url)).is_none(), "{url}");
        }
    }

    #[test]
    fn short_links_are_classified_not_normalized() {
        let short = parse("https://v.kuaishou.com/KoHZ7psH");
        assert!(is_kuaishou_short_url(&short));
        assert!(normalize_kuaishou_url(&short).is_none());
        let nested = parse("https://v.kuaishou.com/a/b");
        assert!(!is_kuaishou_short_url(&nested));
    }

    #[test]
    fn profile_id_round_trip() {
        assert_eq!(
            profile_id_from_feed_url("https://www.kuaishou.com/profile/3x4kuwfd24jzzf4")
                .as_deref(),
            Some("3x4kuwfd24jzzf4")
        );
        assert!(profile_id_from_feed_url(
            "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4?fid=1"
        )
        .is_none());
        assert!(profile_id_from_feed_url("https://www.kuaishou.com/short-video/x").is_none());
    }

    #[test]
    fn sanitize_accepts_bare_id_and_both_hosts() {
        assert_eq!(
            sanitize_profile_input("3X4KUWFd24jzzf4"),
            Some("3x4kuwfd24jzzf4".to_string())
        );
        assert_eq!(
            sanitize_profile_input("https://live.kuaishou.com/profile/3x4kuwfd24jzzf4"),
            Some("3x4kuwfd24jzzf4".to_string())
        );
        assert_eq!(sanitize_profile_input("https://v.kuaishou.com/KoHZ7psH"), None);
    }

    /* ── request bodies ────────────────────────────────────────────── */

    #[test]
    fn operation_bodies_carry_the_expected_shapes() {
        let profile = build_vision_profile_body("abc");
        assert_eq!(profile["operationName"], "visionProfile");
        assert_eq!(profile["variables"]["userId"], "abc");
        let list = build_photo_list_body("abc");
        assert_eq!(list["operationName"], "visionProfilePhotoList");
        assert_eq!(list["variables"]["pcursor"], "");
        assert_eq!(list["variables"]["page"], "profile");
        assert!(list["query"]
            .as_str()
            .unwrap()
            .contains("visionProfilePhotoList"));
    }

    /* ── response parsing ──────────────────────────────────────────── */

    fn profile_response() -> String {
        r#"{"data":{"visionProfile":{"result":1,"hostName":"www.kuaishou.com","userProfile":{"ownerCount":{"fan":123456,"photo":321},"profile":{"gender":"F","user_name":"创作者昵称","user_id":"3x4kuwfd24jzzf4","headurl":"https://p2-pro.a.yximgs.com/uhead/abc.jpg","user_text":"作者简介"}}}}}"#.to_string()
    }

    #[test]
    fn parses_profile_response() {
        let p = parse_profile_response(&profile_response()).unwrap();
        assert_eq!(p.name, "创作者昵称");
        assert_eq!(p.user_id, "3x4kuwfd24jzzf4");
        assert_eq!(p.avatar.as_deref(), Some("https://p2-pro.a.yximgs.com/uhead/abc.jpg"));
        assert_eq!(p.bio.as_deref(), Some("作者简介"));
        assert_eq!(p.fan.as_deref(), Some("123456"));
    }

    #[test]
    fn profile_error_codes_map_to_taxonomy() {
        let captcha = r#"{"data":{"result":400002,"jsSdkUrl":"//static.yximgs.com/x.js","url":"https://captcha.zt.kuaishou.com/iframe/index.html?captchaSession=x"}}"#;
        assert!(matches!(
            parse_profile_response(captcha),
            Err(KsError::Captcha)
        ));
        let blocked = r#"{"data":{"result":2}}"#;
        assert!(matches!(parse_profile_response(blocked), Err(KsError::Blocked)));
        let gone = r#"{"data":{"result":99}}"#;
        assert!(matches!(
            parse_profile_response(gone),
            Err(KsError::Unavailable(_))
        ));
    }

    fn photo_list_response() -> String {
        r#"{"data":{"visionProfilePhotoList":{"result":1,"pcursor":"1.0","feeds":[
            {"type":"1","author":{"id":"3x4kuwfd24jzzf4","name":"创作者昵称","headerUrl":"https://p2-pro.a.yximgs.com/uhead/abc.jpg"},
             "photo":{"id":"3xphoto00001","duration":45000,"caption":"第一条作品","originCaption":"第一条作品",
                      "likeCount":100,"viewCount":2000,"commentCount":10,"realLikeCount":120,
                      "coverUrl":"https://p2-pro.a.yximgs.com/cover1.jpg","photoUrl":"https://p2-pro.a.yximgs.com/video1.mp4",
                      "coverUrls":[{"url":"https://p2-pro.a.yximgs.com/cover1.jpg"}],
                      "timestamp":1794462771000,"expTag":"1.1.1","animatedCoverUrl":"https://p2-pro.a.yximgs.com/anim.gif",
                      "distance":-1,"videoRatio":"1.7778","liked":false,"stereoType":0,"profileUserTopPhoto":false,"musicBlocked":false},
             "canAddComment":true,"llsid":"1","status":1,"currentPcursor":null,"tags":[]},
            {"type":"1","author":{"id":"3x4kuwfd24jzzf4","name":"创作者昵称"},
             "photo":{"id":"3xphoto00002","caption":"图集封面","coverUrls":[{"url":"https://p2-pro.a.yximgs.com/cover2.jpg"}],
                      "timestamp":1794370000000,"realLikeCount":-1}},
            {"type":"1","author":{"id":"x"},"photo":{"id":"","caption":"无 id 丢弃"}}]}}}"#.to_string()
    }

    #[test]
    fn parses_photo_list_page() {
        let (photos, pcursor) = parse_photo_list_response(&photo_list_response()).unwrap();
        assert_eq!(pcursor, "1.0");
        assert_eq!(photos.len(), 2);
        assert_eq!(photos[0].id, "3xphoto00001");
        assert_eq!(photos[0].caption, "第一条作品");
        assert_eq!(photos[0].video.as_deref(), Some("https://p2-pro.a.yximgs.com/video1.mp4"));
        assert_eq!(photos[0].timestamp_ms, Some(1794462771000));
        assert_eq!(photos[0].like_count.as_deref(), Some("120"));
        // Works without an mp4 fall back to the cover; hidden like counts drop.
        assert!(photos[1].video.is_none());
        assert_eq!(photos[1].cover.as_deref(), Some("https://p2-pro.a.yximgs.com/cover2.jpg"));
        assert!(photos[1].like_count.is_none());
    }

    #[test]
    fn photo_list_error_codes_map_to_taxonomy() {
        let captcha = r#"{"data":{"result":400002}}"#;
        assert!(matches!(
            parse_photo_list_response(captcha),
            Err(KsError::Captcha)
        ));
    }

    /* ── articles ──────────────────────────────────────────────────── */

    #[test]
    fn video_article_carries_player_poster_and_enclosure() {
        let photo = KsPhoto {
            id: "3xphoto00001".into(),
            caption: "第一条\n作品".into(),
            author: Some("创作者昵称".into()),
            cover: Some("https://p2-pro.a.yximgs.com/cover1.jpg".into()),
            video: Some("https://p2-pro.a.yximgs.com/video1.mp4".into()),
            timestamp_ms: Some(1794462771000),
            like_count: Some("120".into()),
        };
        let article = photo_to_article(
            &photo,
            "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4",
        );
        assert_eq!(article.guid, "3xphoto00001");
        assert!(article
            .url
            .as_deref()
            .unwrap()
            .starts_with("https://www.kuaishou.com/short-video/3xphoto00001"));
        let html = article.content_html.unwrap();
        assert!(html.contains("<video"));
        assert!(html.contains("poster=\"https://p2-pro.a.yximgs.com/cover1.jpg\""));
        assert!(html.contains("第一条<br>作品"));
        assert_eq!(
            article.enclosures.first().map(|e| e.mime_type.as_deref()),
            Some(Some("video/mp4"))
        );
        assert!(article.published_at.is_some());
    }

    #[test]
    fn photo_article_falls_back_to_cover_and_title() {
        let photo = KsPhoto {
            id: "3xphoto00002".into(),
            caption: String::new(),
            author: None,
            cover: Some("https://p2-pro.a.yximgs.com/cover2.jpg".into()),
            video: None,
            timestamp_ms: Some(1794370000000),
            like_count: None,
        };
        let article = photo_to_article(
            &photo,
            "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4",
        );
        assert!(article.title.starts_with("作品 20"));
        let html = article.content_html.unwrap();
        assert!(html.contains("<img src="));
        assert!(!html.contains("<video"));
        assert_eq!(
            article.image_url.as_deref(),
            Some("https://p2-pro.a.yximgs.com/cover2.jpg")
        );
    }
}
