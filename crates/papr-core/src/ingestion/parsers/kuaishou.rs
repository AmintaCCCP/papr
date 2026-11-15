//! Built-in Kuaishou parser: turns a creator profile URL into feed data from
//! live.kuaishou.com's plain JSON web APIs.
//!
//! Upstream reality (verified 2026-11 against the live site):
//! - www.kuaishou.com's profile page is a client-rendered SPA shell, and the
//!   unsigned GraphQL route is gone for good: the `Photo` type became an
//!   interface (`Cannot query field "id" on type "Photo"` — HTTP 400, the
//!   fields moved behind `... on PhotoEntity` inline fragments), and a schema
//!   fix only reaches the next gate — `visionProfilePhotoList` then answers
//!   `result: 50` to any unsigned call, because the web client signs its own
//!   requests with `__NS_hxfalcon`, a VM-obfuscated signer no plain HTTP
//!   client reproduces faithfully.
//! - live.kuaishou.com still serves two plain, unsigned JSON endpoints that
//!   the profile SPA itself calls: `/live_api/baseuser/userinfo/byid`
//!   (name / avatar / bio / ban status) and `/live_api/profile/public` (the
//!   newest ~20 works: id, cover, video, gallery, like counts). Both are
//!   login-gated for anonymous traffic (`result: 2`, or the `400002` captcha
//!   shape), so the parser requires the pasted logged-in `Cookie`
//!   (Settings → Parsers), mirroring the X parser's credential model. For a
//!   cookie-free setup, the RSSHub route marketplace's browser-flavor engine
//!   can run the signed flow headlessly — the error copy says so.
//! - The works payload carries no captions and no publish timestamps (the
//!   profile grid shows neither either): articles are media cards titled by
//!   work type and deduped by the stable work id — the same limitation the
//!   RSSHub route lives with.
//!
//! Anti-abuse posture: read-only, first page only, strictly serial per feed
//! leg, jittered pacing between feeds, cookies only in an explicit header
//! (never a jar), and every response's final URL must still belong to
//! kuaishou.com.

use crate::error::AppError;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use url::Url;

use super::transport::{browser_api_headers, is_host_or_subdomain};

/// The site every kuaishou request must stay on.
const REGISTRABLE_DOMAIN: &str = "kuaishou.com";
/// The host the JSON web APIs live on.
const LIVE_HOST: &str = "live.kuaishou.com";

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
    /// `result: 400002`, or an anonymous `result: 2` — the anti-crawl gate.
    /// Anonymous traffic hits this by design; a stale cookie can too, so the
    /// message covers both.
    #[error("captcha")]
    Captcha,
    /// `result: 2/50` with a cookie — rate-limited, risk-controlled or
    /// signature-required.
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
/// letters/digits. Covers both shapes the live profile URLs use — the
/// `3x…`-style web ids and 10-char app short names — both of which the
/// `principalId=` endpoints resolve.
pub fn is_valid_profile_id(id: &str) -> bool {
    (10..=24).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
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
    Some(format!("https://www.kuaishou.com/profile/{id}"))
}

/// Recognize an app share short link (`https://v.kuaishou.com/<token>`).
/// `true` only for the exact host and a single path segment — resolution
/// (redirect follow) happens in `add_feed`, never here.
pub fn is_kuaishou_short_url(url: &Url) -> bool {
    url.host_str()
        .map(|h| h.to_lowercase() == "v.kuaishou.com")
        .unwrap_or(false)
        && url.path().split('/').filter(|s| !s.is_empty()).count() == 1
}

/// Recover the profile id from a stored canonical feed URL. The inverse of
/// [`normalize_kuaishou_url`] restricted to canonical URLs.
pub fn profile_id_from_feed_url(feed_url: &str) -> Option<String> {
    let url = Url::parse(feed_url).ok()?;
    if url.host_str()?.to_lowercase() != "www.kuaishou.com" {
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

/* ── parsed shapes ───────────────────────────────────────────────────── */

/// The creator resolved via `/live_api/baseuser/userinfo/byid`.
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
    /// Work id — the stable dedup key (`articles.guid`).
    pub id: String,
    /// As shipped by the payload: `"video"`, `"multiple"` (image set), …
    pub work_type: Option<String>,
    /// Best cover (`poster`).
    pub cover: Option<String>,
    /// The mp4 URL, when the work is a video.
    pub video: Option<String>,
    /// Gallery images for image-set works.
    pub images: Vec<String>,
    /// Like counter, as shipped (`counts.like`).
    pub like_count: Option<String>,
}

/// One creator's page: metadata plus the newest works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KsUserPage {
    pub profile: KsProfile,
    pub photos: Vec<KsPhoto>,
}

/// Map a live-api `result` code onto the error taxonomy. Anonymous traffic is
/// refused with `2` or the `400002` captcha shape — both read as "configure a
/// cookie" when no cookie was sent; the same codes with a cookie sent are
/// risk control, not a missing credential.
fn result_to_error(result: i64, cookie_sent: bool) -> KsError {
    match result {
        400002 => KsError::Captcha,
        2 | 50 if cookie_sent => KsError::Blocked,
        2 | 50 => KsError::Captcha,
        _ => KsError::Unavailable(format!("result {result}")),
    }
}

/// Walk a live-api envelope: `{"data": {"result": 1, …}}`. Returns the `data`
/// object after checking `result` (absent result counts as success — the
/// payloads ship it only on failure).
fn envelope_data(response: &Value, cookie_sent: bool) -> KsResult<&Value> {
    let data = response
        .get("data")
        .filter(|d| d.is_object())
        .ok_or_else(|| KsError::Parse("live api: no `data` object".into()))?;
    if let Some(result) = data.get("result").and_then(|r| r.as_i64()) {
        if result != 1 {
            return Err(result_to_error(result, cookie_sent));
        }
    }
    Ok(data)
}

/// Parse the `/live_api/baseuser/userinfo/byid` response. `cookie_sent`
/// distinguishes "the credential was refused" (`Blocked`) from "there was no
/// credential to refuse" (`Captcha`) for the gate codes.
pub fn parse_userinfo_response(
    response: &str,
    principal: &str,
    cookie_sent: bool,
) -> KsResult<KsProfile> {
    let value: Value = serde_json::from_str(response)
        .map_err(|e| KsError::Parse(format!("userinfo json: {e}")))?;
    let data = envelope_data(&value, cookie_sent)?;
    let user = data
        .get("userInfo")
        .filter(|u| u.is_object())
        .ok_or_else(|| KsError::Unavailable("userinfo returned no userInfo".into()))?;
    let banned = user
        .pointer("/bannedStatus/banned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let social_banned = user
        .pointer("/bannedStatus/socialBanned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let isolate = user
        .pointer("/bannedStatus/isolate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if banned || social_banned || isolate {
        return Err(KsError::Unavailable("account banned or suspended".into()));
    }
    let name = user
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(KsError::Unavailable("userinfo has no nickname".into()));
    }
    Ok(KsProfile {
        user_id: user
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(principal)
            .to_string(),
        name,
        avatar: user
            .get("avatar")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        bio: user
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from),
        fan: None,
    })
}

/// Parse the `/live_api/profile/public` response into typed works.
/// `cookie_sent` distinguishes the anonymous gate (`Captcha`) from a refused
/// credential (`Blocked`), as in [`parse_userinfo_response`].
pub fn parse_public_profile_response(response: &str, cookie_sent: bool) -> KsResult<Vec<KsPhoto>> {
    let value: Value = serde_json::from_str(response)
        .map_err(|e| KsError::Parse(format!("profile public json: {e}")))?;
    let data = envelope_data(&value, cookie_sent)?;
    let list = data
        .get("list")
        .and_then(|l| l.as_array())
        .ok_or_else(|| KsError::Parse("profile public: no `list`".into()))?;
    let mut photos = Vec::new();
    for item in list {
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let images: Vec<String> = item
            .get("imgUrls")
            .and_then(|v| v.as_array())
            .map(|urls| {
                urls.iter()
                    .filter_map(|u| u.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let like_count = item
            .pointer("/counts/like")
            .and_then(|v| v.as_i64())
            .filter(|n| *n >= 0)
            .map(|n| n.to_string());
        photos.push(KsPhoto {
            id,
            work_type: item
                .get("workType")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            cover: item
                .get("poster")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            video: item
                .get("playUrl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            images,
            like_count,
        });
    }
    Ok(photos)
}

/* ── article builders (pure) ─────────────────────────────────────────── */

fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The fallback title for a caption-less work. The payload carries no
/// captions, so the work type is what the card can say.
fn work_title(photo: &KsPhoto) -> &'static str {
    match photo.work_type.as_deref() {
        Some("video") => "视频作品",
        Some("multiple") => "图集作品",
        _ => "作品",
    }
}

/// Map one work to the ingestion shape: video player (with poster) when the
/// payload carries an mp4, gallery images otherwise (falling back to the
/// cover), meta line below.
pub fn photo_to_article(photo: &KsPhoto, _profile_url: &str) -> crate::db::NewArticle {
    let permalink = format!("https://www.kuaishou.com/short-video/{}", photo.id);
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
        _ => {}
    }
    let gallery: Vec<&String> = if photo.images.is_empty() {
        photo.cover.iter().collect()
    } else {
        photo.images.iter().collect()
    };
    if photo.video.is_none() {
        for image in gallery {
            html.push_str(&format!("<p><img src=\"{}\"></p>", escape_html_text(image)));
        }
    }
    let mut meta = Vec::new();
    if let Some(likes) = &photo.like_count {
        meta.push(format!("♥ {likes}"));
    }
    if !meta.is_empty() {
        html.push_str(&format!("<p>{}</p>", escape_html_text(&meta.join(" · "))));
    }
    crate::db::NewArticle {
        guid: photo.id.clone(),
        url: Some(permalink),
        title: work_title(photo).to_string(),
        author: None,
        summary: Some(work_title(photo).to_string()),
        content_html: Some(crate::sanitize::sanitize_parser(
            &html,
            Some(&sanitize_base),
        )),
        body_text: String::new(),
        image_url: photo.cover.clone(),
        published_at: None,
        enclosures: photo
            .video
            .as_ref()
            .map(|video| {
                vec![crate::models::Enclosure {
                    url: video.clone(),
                    mime_type: Some("video/mp4".into()),
                    length: None,
                }]
            })
            .unwrap_or_default(),
    }
}

/* ── network transport ───────────────────────────────────────────────── */

/// GET one live-api JSON endpoint with the browser XHR header group. The
/// shared client honours the app's proxy settings and does not follow
/// redirects (cookies must never ride one); cookies ride only in the explicit
/// header. The final URL must still belong to kuaishou.com.
async fn get_json(client: &Client, url: &str, cookie: Option<&str>) -> KsResult<String> {
    let mut req = client.get(url);
    for (name, value) in browser_api_headers(LIVE_HOST, cookie) {
        req = req.header(name, value);
    }
    let resp = req.send().await?;
    if !is_host_or_subdomain(resp.url(), REGISTRABLE_DOMAIN) {
        return Err(KsError::Transport(format!(
            "redirect to non-kuaishou host blocked: {}",
            resp.url()
        )));
    }
    if resp.status().is_redirection() {
        // The parser client does not follow redirects; the edge's redirect
        // here is the login bounce, same as the other parsers' gates.
        return Err(KsError::Blocked);
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

async fn get_live_api(client: &Client, path_query: &str, cookie: Option<&str>) -> KsResult<String> {
    get_json(client, &format!("https://{LIVE_HOST}{path_query}"), cookie).await
}

/// Fetch one creator's metadata + newest works page.
///
/// `cookie` is the pasted logged-in Cookie header; without it the live-api
/// answers the captcha/`result: 2` gate — that is the coded, localisable
/// signal to configure one.
pub async fn fetch_creator_page(
    client: &Client,
    user_id: &str,
    cookie: Option<&str>,
) -> KsResult<KsUserPage> {
    // The id is validated `[a-z0-9]{10,24}` upstream — no escaping needed.
    let userinfo_q = format!("/live_api/baseuser/userinfo/byid?principalId={user_id}");
    let userinfo = get_live_api(client, &userinfo_q, cookie)
        .await
        .and_then(|body| parse_userinfo_response(&body, user_id, cookie.is_some()));
    let public = get_live_api(
        client,
        &format!("/live_api/profile/public?principalId={user_id}"),
        cookie,
    )
    .await?;
    let photos = parse_public_profile_response(&public, cookie.is_some())?;
    // The works list already identifies the creator per row; the userinfo
    // call only pins down the nickname/avatar for the feed row itself. Its
    // failure is tolerable only while the list actually yielded works to fall
    // back on — an empty list plus a metadata failure must propagate, or
    // subscribing would "succeed" with a nameless feed.
    let profile = match userinfo {
        Ok(profile) => profile,
        Err(e) => {
            if photos.is_empty() {
                return Err(e);
            }
            fallback_profile(user_id)
        }
    };
    Ok(KsUserPage { profile, photos })
}

fn fallback_profile(user_id: &str) -> KsProfile {
    KsProfile {
        user_id: user_id.to_string(),
        name: user_id.to_string(),
        avatar: None,
        bio: None,
        fan: None,
    }
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
            "https://www.kuaishou.com/short-video/3xkuwfd24jzzf4",
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
            profile_id_from_feed_url("https://www.kuaishou.com/profile/3x4kuwfd24jzzf4").as_deref(),
            Some("3x4kuwfd24jzzf4")
        );
        assert!(
            profile_id_from_feed_url("https://www.kuaishou.com/profile/3x4kuwfd24jzzf4?fid=1")
                .is_none()
        );
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
        assert_eq!(
            sanitize_profile_input("https://v.kuaishou.com/KoHZ7psH"),
            None
        );
    }

    /* ── response parsing ──────────────────────────────────────────── */

    fn userinfo_response() -> String {
        r#"{"data":{"result":1,"userInfo":{"id":"youbao5266","name":"彩色角落","description":"每周更新","avatar":"https://p5-pro.a.yximgs.com/uhead/abc.jpg","sex":"F","originUserId":1317646681,"bannedStatus":{"banned":false,"socialBanned":false,"isolate":false,"defriend":false},"counts":{}}}}"#.to_string()
    }

    #[test]
    fn parses_userinfo_response() {
        let p = parse_userinfo_response(&userinfo_response(), "youbao5266", false).unwrap();
        assert_eq!(p.name, "彩色角落");
        assert_eq!(p.user_id, "youbao5266");
        assert_eq!(
            p.avatar.as_deref(),
            Some("https://p5-pro.a.yximgs.com/uhead/abc.jpg")
        );
        assert_eq!(p.bio.as_deref(), Some("每周更新"));
    }

    #[test]
    fn userinfo_error_codes_map_to_taxonomy() {
        let captcha = r#"{"data":{"result":400002,"url":"https://captcha.zt.kuaishou.com/…","jsSdkUrl":"//static.yximgs.com/x.js"}}"#;
        assert!(matches!(
            parse_userinfo_response(captcha, "x", false),
            Err(KsError::Captcha)
        ));
        let gone = r#"{"data":{"result":21,"userInfo":{}}}"#;
        assert!(matches!(
            parse_userinfo_response(gone, "x", true),
            Err(KsError::Unavailable(_))
        ));
        let banned =
            r#"{"data":{"result":1,"userInfo":{"name":"x","bannedStatus":{"banned":true}}}}"#;
        assert!(matches!(
            parse_userinfo_response(banned, "x", true),
            Err(KsError::Unavailable(_))
        ));
        let no_user = r#"{"data":{"result":1}}"#;
        assert!(matches!(
            parse_userinfo_response(no_user, "x", true),
            Err(KsError::Unavailable(_))
        ));
        // The gate codes read as a refused credential once a cookie was sent.
        let gate = r#"{"data":{"result":2,"error_msg":null}}"#;
        assert!(matches!(
            parse_userinfo_response(gate, "x", true),
            Err(KsError::Blocked)
        ));
        assert!(matches!(
            parse_userinfo_response(gate, "x", false),
            Err(KsError::Captcha)
        ));
    }

    fn public_profile_response() -> String {
        r#"{"data":{"result":1,"list":[
            {"type":"work","workType":"video","id":"3xks5c62378xie2",
             "poster":"https://p23.a.yximgs.com/cover1.jpg",
             "playUrl":"https://txmov2.a.kwimgs.com/video1.mp4",
             "width":720,"height":1280,"imgUrls":[],"imgSizes":[],
             "counts":{"like":31,"displayView":"999","displayLike":"31"}},
            {"type":"work","workType":"multiple","id":"3xjrscpkm6dsj5k",
             "poster":"https://p23.a.yximgs.com/cover2.jpg",
             "imgUrls":["https://p23.a.yximgs.com/g1.jpg","https://p23.a.yximgs.com/g2.jpg"],
             "counts":{"like":-1}},
            {"type":"work","id":"","poster":"https://x"}]}}"#
            .to_string()
    }

    #[test]
    fn parses_public_profile_page() {
        let photos = parse_public_profile_response(&public_profile_response(), true).unwrap();
        assert_eq!(photos.len(), 2);
        assert_eq!(photos[0].id, "3xks5c62378xie2");
        assert_eq!(photos[0].work_type.as_deref(), Some("video"));
        assert_eq!(
            photos[0].video.as_deref(),
            Some("https://txmov2.a.kwimgs.com/video1.mp4")
        );
        assert_eq!(
            photos[0].cover.as_deref(),
            Some("https://p23.a.yximgs.com/cover1.jpg")
        );
        assert_eq!(photos[0].like_count.as_deref(), Some("31"));
        // Image-set work: gallery carried, hidden like count dropped.
        assert_eq!(photos[1].images.len(), 2);
        assert!(photos[1].video.is_none());
        assert!(photos[1].like_count.is_none());
    }

    #[test]
    fn public_profile_error_codes_map_to_taxonomy() {
        let gate = r#"{"data":{"result":2,"error_msg":null}}"#;
        assert!(matches!(
            parse_public_profile_response(gate, false),
            Err(KsError::Captcha)
        ));
        assert!(matches!(
            parse_public_profile_response(gate, true),
            Err(KsError::Blocked)
        ));
    }

    /* ── articles ──────────────────────────────────────────────────── */

    #[test]
    fn video_article_carries_player_poster_and_enclosure() {
        let photo = KsPhoto {
            id: "3xks5c62378xie2".into(),
            work_type: Some("video".into()),
            cover: Some("https://p23.a.yximgs.com/cover1.jpg".into()),
            video: Some("https://txmov2.a.kwimgs.com/video1.mp4".into()),
            images: vec![],
            like_count: Some("31".into()),
        };
        let article = photo_to_article(&photo, "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4");
        assert_eq!(article.guid, "3xks5c62378xie2");
        assert_eq!(article.title, "视频作品");
        assert!(article
            .url
            .as_deref()
            .unwrap()
            .starts_with("https://www.kuaishou.com/short-video/3xks5c62378xie2"));
        let html = article.content_html.unwrap();
        assert!(html.contains("<video"));
        assert!(html.contains("poster=\"https://p23.a.yximgs.com/cover1.jpg\""));
        assert!(html.contains("♥ 31"));
        assert_eq!(
            article.enclosures.first().map(|e| e.mime_type.as_deref()),
            Some(Some("video/mp4"))
        );
    }

    #[test]
    fn gallery_article_renders_the_image_set() {
        let photo = KsPhoto {
            id: "3xjrscpkm6dsj5k".into(),
            work_type: Some("multiple".into()),
            cover: Some("https://p23.a.yximgs.com/cover2.jpg".into()),
            video: None,
            images: vec![
                "https://p23.a.yximgs.com/g1.jpg".into(),
                "https://p23.a.yximgs.com/g2.jpg".into(),
            ],
            like_count: None,
        };
        let article = photo_to_article(&photo, "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4");
        assert_eq!(article.title, "图集作品");
        let html = article.content_html.unwrap();
        assert!(html.contains("g1.jpg"));
        assert!(html.contains("g2.jpg"));
        assert!(!html.contains("<video"));
        assert_eq!(
            article.image_url.as_deref(),
            Some("https://p23.a.yximgs.com/cover2.jpg")
        );
        assert!(article.enclosures.is_empty());
    }

    #[test]
    fn unknown_work_type_falls_back_to_a_generic_title() {
        let photo = KsPhoto {
            id: "3xabc".into(),
            work_type: None,
            cover: Some("https://p23.a.yximgs.com/cover3.jpg".into()),
            video: None,
            images: vec![],
            like_count: None,
        };
        let article = photo_to_article(&photo, "");
        assert_eq!(article.title, "作品");
        // No gallery: the cover renders as the single image.
        let html = article.content_html.unwrap();
        assert!(html.contains("cover3.jpg"));
    }
}
