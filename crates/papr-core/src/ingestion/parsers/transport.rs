//! Shared transport helpers for the browser-flavor built-in parsers
//! (xiaohongshu / kuaishou).
//!
//! These parsers talk to sites that fingerprint traffic. The X parser gets
//! away with a minimal header set because x.com's GraphQL API only checks the
//! UA; xiaohongshu's SSR edge is stricter, so requests there carry the full
//! browser header group a Chrome document navigation would send — same names,
//! same order, self-consistent version numbers.
//!
//! Two invariants are non-negotiable and shared by every parser:
//! - Cookies travel only inside an explicit `Cookie` header, never a jar, so
//!   they cannot leak to a redirected host.
//! - Response reads are capped ([`read_capped`]) — an unbounded SSR page is
//!   an OOM vector.

use chrono::Utc;
use rand::Rng;

/// One bundled browser profile: UA string plus the matching sec-ch-ua
/// versions. The versions must agree or the mismatch itself is a fingerprint.
struct BrowserProfile {
    /// `sec-ch-ua` member, e.g. `"Chromium";v="152"`.
    brand_version: &'static str,
    /// The full desktop UA string.
    ua: &'static str,
}

/// A small rotating pool (one entry today) — the header builder picks per
/// call so a future Chrome bump is a one-line change here.
static PROFILES: &[BrowserProfile] = &[BrowserProfile {
    brand_version: "152",
    ua: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36",
}];

/// The complete Chrome document-navigation header group for `host`, in
/// browser order. `cookie` rides as the last header exactly as the DevTools
/// "copy as cURL" rendering shows it.
pub fn browser_document_headers(host: &str, cookie: Option<&str>) -> Vec<(String, String)> {
    let profile = &PROFILES[rand::thread_rng().gen_range(0..PROFILES.len())];
    let v = profile.brand_version;
    let mut headers: Vec<(String, String)> = vec![
        ("User-Agent".into(), profile.ua.into()),
        (
            "Accept".into(),
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7".into(),
        ),
        ("Accept-Language".into(), "zh-CN,zh;q=0.9,en;q=0.8".into()),
        ("sec-ch-ua".into(), format!(r#""Chromium";v="{v}", "Not(A:Brand";v="24", "Google Chrome";v="{v}""#)),
        ("sec-ch-ua-mobile".into(), "?0".into()),
        ("sec-ch-ua-platform".into(), r#""macOS""#.into()),
        ("Sec-Fetch-Dest".into(), "document".into()),
        ("Sec-Fetch-Mode".into(), "navigate".into()),
        ("Sec-Fetch-Site".into(), "none".into()),
        ("Sec-Fetch-User".into(), "?1".into()),
        ("Upgrade-Insecure-Requests".into(), "1".into()),
        (
            "Referer".into(),
            format!("https://{host}/"),
        ),
    ];
    if let Some(cookie) = cookie {
        headers.push(("Cookie".into(), cookie.to_string()));
    }
    headers
}

/// The Chrome XHR/fetch header group for JSON API calls (kuaishou's GraphQL
/// endpoint). Same UA family, but `sec-fetch-*` describe a same-origin CORS
/// request rather than a navigation.
pub fn browser_api_headers(host: &str, referer: &str, cookie: Option<&str>) -> Vec<(String, String)> {
    let profile = &PROFILES[0];
    let v = profile.brand_version;
    let mut headers: Vec<(String, String)> = vec![
        ("User-Agent".into(), profile.ua.into()),
        ("Accept".into(), "application/json, text/plain, */*".into()),
        ("Accept-Language".into(), "zh-CN,zh;q=0.9,en;q=0.8".into()),
        ("Content-Type".into(), "application/json".into()),
        ("sec-ch-ua".into(), format!(r#""Chromium";v="{v}", "Not(A:Brand";v="24", "Google Chrome";v="{v}""#)),
        ("sec-ch-ua-mobile".into(), "?0".into()),
        ("sec-ch-ua-platform".into(), r#""macOS""#.into()),
        ("Sec-Fetch-Dest".into(), "empty".into()),
        ("Sec-Fetch-Mode".into(), "cors".into()),
        ("Sec-Fetch-Site".into(), "same-origin".into()),
        ("Referer".into(), referer.to_string()),
        ("Origin".into(), format!("https://{host}")),
    ];
    if let Some(cookie) = cookie {
        headers.push(("Cookie".into(), cookie.to_string()));
    }
    headers
}

/// `true` when `url`'s host is `host` or a subdomain of it. The per-parser
/// final-URL checks build on this so a redirect can never carry session
/// cookies to a foreign host.
pub fn is_host_or_subdomain(url: &url::Url, registrable: &str) -> bool {
    url.host_str()
        .map(|h| {
            let h = h.to_lowercase();
            h == registrable || h.ends_with(&format!(".{registrable}"))
        })
        .unwrap_or(false)
}

/// Milliseconds to sleep between two requests to the same site: the base
/// pacing plus a ±50% jitter so a wall of feeds never produces a perfectly
/// regular (i.e. bot-shaped) request stream.
pub fn jittered_delay_ms(base_ms: u64) -> u64 {
    let half = (base_ms / 2).max(1);
    rand::thread_rng().gen_range(base_ms.saturating_sub(half)..=(base_ms + half))
}

/// Sleep `delay_ms` milliseconds.
pub async fn pace(delay_ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
}

/// The current unix time in whole milliseconds — the `ets`/`loadts`-style
/// values some sites echo back in cookies or forms.
pub fn unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn document_headers_carry_cookie_last_and_consistent_versions() {
        let headers = browser_document_headers("www.xiaohongshu.com", Some("a=1"));
        let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        // Browser order: UA first, Cookie last.
        assert_eq!(names[0], "User-Agent");
        assert_eq!(*names.last().unwrap(), "Cookie");
        // sec-ch-ua and UA must claim the same major version.
        let ua = &headers[0].1;
        let sec = headers.iter().find(|(n, _)| n == "sec-ch-ua").unwrap();
        let ua_major = ua.split("Chrome/").nth(1).unwrap().split('.').next().unwrap();
        assert!(sec.1.contains(&format!("v=\"{ua_major}\"")));
    }

    #[test]
    fn api_headers_set_cors_shape() {
        let headers = browser_api_headers(
            "www.kuaishou.com",
            "https://www.kuaishou.com/profile/x",
            None,
        );
        let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        assert_eq!(get("Sec-Fetch-Mode").as_deref(), Some("cors"));
        assert_eq!(get("Origin").as_deref(), Some("https://www.kuaishou.com"));
        assert!(get("Cookie").is_none());
    }

    #[test]
    fn host_check_matches_subdomains_only() {
        let ok = Url::parse("https://www.xiaohongshu.com/user/profile/x").unwrap();
        let sub = Url::parse("https://edith.xiaohongshu.com/api/x").unwrap();
        let bad = Url::parse("https://xhs.example.com/user").unwrap();
        let evil = Url::parse("https://xiaohongshu.com.evil.io/x").unwrap();
        assert!(is_host_or_subdomain(&ok, "xiaohongshu.com"));
        assert!(is_host_or_subdomain(&sub, "xiaohongshu.com"));
        assert!(!is_host_or_subdomain(&bad, "xiaohongshu.com"));
        assert!(!is_host_or_subdomain(&evil, "xiaohongshu.com"));
    }

    #[test]
    fn jitter_stays_around_base() {
        for _ in 0..50 {
            let d = jittered_delay_ms(1000);
            assert!((500..=1500).contains(&d), "jitter out of band: {d}");
        }
    }
}
