//! HTTP fetching with conditional GET (ETag / If-Modified-Since) so unchanged
//! feeds cost a single 304 round-trip.

use crate::db;
use crate::error::{AppError, AppResult};
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::{Client, StatusCode};
use rusqlite::Connection;
use std::time::Duration;

pub const USER_AGENT: &str = "Papr/0.1 (+https://github.com/papr-reader)";

/// Hard cap on a fetched body. Feeds and article pages are text — a few
/// hundred KB at most — so 16 MiB is generous while still stopping a
/// hostile or misconfigured server from exhausting memory.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Read a response body, aborting if it exceeds `MAX_BODY_BYTES`. Streams in
/// chunks so an unbounded (or lying-`Content-Length`) response can't first be
/// buffered whole.
pub(crate) async fn read_capped(mut resp: reqwest::Response) -> AppResult<Vec<u8>> {
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err(AppError::code("responseTooLarge"));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(AppError::code("responseTooLarge"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Build the shared HTTP client (connection pooling, gzip/brotli, redirects).
///
/// `timeout_secs` bounds the whole request. `proxy` is one of `"system"`
/// (honour `HTTP(S)_PROXY` env vars), `"none"` (bypass all proxies), or an
/// explicit proxy URL.
pub fn build_client(timeout_secs: u64, proxy: &str) -> Client {
    build_client_inner(timeout_secs.clamp(5, 300), proxy)
}

fn build_client_inner(timeout_secs: u64, proxy: &str) -> Client {
    let mut builder = Client::builder()
        .user_agent(USER_AGENT)
        .cookie_store(true)
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(10));

    match proxy {
        "system" | "" => {}
        "none" => builder = builder.no_proxy(),
        custom => {
            if let Ok(p) = reqwest::Proxy::all(custom) {
                builder = builder.proxy(p);
            }
        }
    }
    builder.build().expect("failed to build reqwest client")
}

/// Build the HTTP client from the persisted network settings.
pub fn build_client_from_settings(conn: &Connection) -> Client {
    let timeout = db::setting_parsed::<u64>(conn, "net_timeout_sec", 30);
    let proxy = db::get_setting(conn, "net_proxy")
        .ok()
        .flatten()
        .unwrap_or_else(|| "system".to_string());
    build_client(timeout, &proxy)
}

/// Build the client for large engine downloads: the user's proxy settings
/// (a release archive may well be behind the same network that needs the
/// proxy), but a far longer whole-request budget than the feed client's
/// default 30 s — a full engine archive is >100 MB and the download would
/// otherwise die mid-transfer. The transfer is still bounded by the
/// extractor's archive-size cap.
pub fn build_download_client(conn: &Connection) -> Client {
    let proxy = db::get_setting(conn, "net_proxy")
        .ok()
        .flatten()
        .unwrap_or_else(|| "system".to_string());
    build_client_inner(30 * 60, &proxy)
}

/// Build the client the browser-flavor built-in parsers (xiaohongshu,
/// kuaishou) talk through: the user's proxy settings, but **no cookie jar and
/// no redirect following**. Both properties are load-bearing:
/// - reqwest's jar would `insert` a `Cookie` header over the parser's
///   explicitly-built one whenever the jar holds cookies for the target host
///   (upstream responses set `acw_tc`-style cookies on every hit), silently
///   replacing the configured session — the jar must be off.
/// - Redirects are inspected instead of followed: the xiaohongshu edge
///   answers throttled visitors with a 302 onto its login page, which is a
///   coded error, not a document to read; and a redirect must never be
///   followed off-site with session cookies attached.
pub fn build_parser_client(conn: &Connection) -> Client {
    let timeout = db::setting_parsed::<u64>(conn, "net_timeout_sec", 30);
    let proxy = db::get_setting(conn, "net_proxy")
        .ok()
        .flatten()
        .unwrap_or_else(|| "system".to_string());
    let mut builder = Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(timeout.clamp(5, 300)))
        .connect_timeout(Duration::from_secs(10));
    match proxy.as_str() {
        "system" | "" => {}
        "none" => builder = builder.no_proxy(),
        custom => {
            if let Ok(p) = reqwest::Proxy::all(custom) {
                builder = builder.proxy(p);
            }
        }
    }
    builder.build().expect("failed to build reqwest client")
}

/// Result of a conditional GET against a feed URL.
pub enum Fetched {
    /// Server returned 304 — the stored copy is still current.
    NotModified,
    /// Fresh content, along with revalidation headers to store.
    Body {
        bytes: Vec<u8>,
        /// The URL the document actually came from — after HTTP redirects and
        /// an anti-bot challenge hop this can differ from the request URL,
        /// and it (not the request URL) is the base relative links inside the
        /// document resolve against.
        doc_url: String,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// Detect a JavaScript-redirect anti-bot challenge in a fetched body and
/// return the URL it points at.
///
/// Some hosts (openresty-style WAFs) answer automated clients with a tiny HTML
/// page whose only content is `location.replace("/path?token=…")` — a browser
/// follows it (with the cookie the response set) and gets the real document.
/// A plain HTTP client stops there and reports "could not parse the feed" on
/// what is actually a block page.
///
/// The recognisable shape is deliberately narrow, on two axes: the document
/// must be small (challenges are a few hundred bytes; a real feed or article
/// is kilobytes), and the `location.replace` call must live inside a
/// `<script>` element on a page that carries no visible story beyond it
/// (empty body, at most a short title). The DOM-level checks keep page
/// content from impersonating a challenge — a feed-discovery page that quotes
/// the call in a `<pre>` code sample keeps its prose visible and is rejected,
/// so following it never fetches the quoted example URL.
///
/// Returns the `location.replace` argument with JSON escapes (`\/`) decoded,
/// resolved against `base_url` when it is relative. `None` for anything else.
pub fn js_redirect_target(bytes: &[u8], base_url: &str) -> Option<String> {
    // A challenge page is minimal HTML — anything large is real content.
    if bytes.len() > 2048 {
        return None;
    }
    let text = String::from_utf8_lossy(bytes);
    // Byte-sliced prefix checks (`get`, so a multibyte lead can't panic): a
    // doctype or `<html>` opener, any case.
    let head = text.trim_start();
    let looks_like_html = head
        .get(..9)
        .is_some_and(|s| s.eq_ignore_ascii_case("<!doctype"))
        || head.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("<html"));
    if !looks_like_html {
        return None;
    }
    let dom = scraper::Html::parse_fragment(&text);
    let mut script_call: Option<String> = None;
    let mut visible_chars = 0usize;
    for node in dom.tree.nodes() {
        let scraper::Node::Text(t) = node.value() else {
            continue;
        };
        let in_script = node
            .ancestors()
            .any(|a| a.value().as_element().is_some_and(|e| e.name() == "script"));
        if in_script {
            if script_call.is_none() {
                // Only a call the browser would actually run counts: a
                // `location.replace` mentioned inside a JS string literal or
                // comment is sample content, not a redirect. Scan forward
                // through the candidates, skipping the ones the tiny lexer
                // below puts inside a string or comment.
                let mut from = 0;
                while let Some(rel) = t[from..].find("location.replace(") {
                    let idx = from + rel;
                    if !inside_js_string_or_comment(t, idx) {
                        script_call = Some(t[idx..].to_string());
                        break;
                    }
                    from = idx + "location.replace(".len();
                }
            }
        } else {
            visible_chars += t.trim().chars().count();
        }
    }
    // Visible text beyond the script means page content, not a challenge.
    if visible_chars > 32 {
        return None;
    }
    let call = script_call?;
    let rest = call["location.replace(".len()..].trim_start();
    // The argument is a single- or double-quoted string; take up to the
    // matching quote (the pages we target never nest quotes inside).
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let arg = &rest[1..];
    let end = arg.find(quote)?;
    let raw = &arg[..end];
    if raw.is_empty() {
        return None;
    }
    // The challenge pages embed the path in a JS string literal, so `/` comes
    // back escaped as `\/`.
    let path = raw.replace("\\/", "/");
    let base = reqwest::Url::parse(base_url).ok()?;
    base.join(&path).ok().map(|u| u.into())
}

/// Is `idx` — the byte offset of a `location.replace(` candidate inside the
/// script source `script` — inside a JS string literal or comment? A tiny
/// lexer tracks line/block comments and the three quote styles (with `\`
/// escapes, backticks included) from the start of the script: a call
/// mentioned inside `"…"` or `// …` is sample content, not a redirect the
/// browser would run. JS regex literals are not modelled — at worst a regex
/// containing `//` hides a real call, which fails safe (the parse errors as
/// it did before the challenge handling existed). An unterminated string or
/// comment also fails safe the same way.
fn inside_js_string_or_comment(script: &str, idx: usize) -> bool {
    #[derive(PartialEq)]
    enum Js {
        Code,
        LineComment,
        BlockComment,
        Str(u8),
    }
    let bytes = script.as_bytes();
    let mut state = Js::Code;
    let mut i = 0;
    while i < idx && i < bytes.len() {
        let c = bytes[i];
        match state {
            Js::Code => {
                if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    state = Js::LineComment;
                    i += 2;
                    continue;
                }
                if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    state = Js::BlockComment;
                    i += 2;
                    continue;
                }
                if c == b'"' || c == b'\'' || c == b'`' {
                    state = Js::Str(c);
                }
                i += 1;
            }
            Js::LineComment => {
                if c == b'\n' {
                    state = Js::Code;
                }
                i += 1;
            }
            Js::BlockComment => {
                if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    state = Js::Code;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            Js::Str(q) => {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    state = Js::Code;
                }
                i += 1;
            }
        }
    }
    state != Js::Code
}

/// Follow one anti-bot challenge redirect: re-GET the target URL with the
/// same client (its cookie jar carries whatever the challenge response set)
/// and return the second response untouched. One hop only — if the target is
/// itself a challenge page it is returned as-is rather than looping.
async fn follow_challenge(
    client: &Client,
    target: &str,
) -> AppResult<reqwest::Response> {
    let resp = client.get(target).send().await?;
    Ok(resp.error_for_status()?)
}

/// Conditional GET. Sends `If-None-Match`/`If-Modified-Since` when we have them.
pub async fn conditional_get(
    client: &Client,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> AppResult<Fetched> {
    let mut req = client.get(url);
    if let Some(e) = etag {
        req = req.header(IF_NONE_MATCH, e);
    }
    if let Some(lm) = last_modified {
        req = req.header(IF_MODIFIED_SINCE, lm);
    }

    let resp = req.send().await?;
    if resp.status() == StatusCode::NOT_MODIFIED {
        return Ok(Fetched::NotModified);
    }
    let resp = resp.error_for_status()?;
    // Resolve the challenge target against the *final* URL, not the request
    // URL — the feed host may have HTTP-redirected along the way, and a
    // relative `location.replace('?token=…')` only means what a browser
    // thinks it means at the document's own address.
    let final_url = resp.url().to_string();
    let header = |name: reqwest::header::HeaderName| {
        resp.headers()
            .get(&name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    let etag = header(ETAG);
    let last_modified = header(LAST_MODIFIED);
    let bytes = read_capped(resp).await?;
    // An anti-bot challenge in place of the feed document: follow its JS
    // redirect once and serve whatever comes back (with the second response's
    // revalidation headers).
    if let Some(target) = js_redirect_target(&bytes, &final_url) {
        let resp = follow_challenge(client, &target).await?;
        let doc_url = resp.url().to_string();
        let read_header = |name: reqwest::header::HeaderName| {
            resp.headers()
                .get(&name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let etag = read_header(ETAG);
        let last_modified = read_header(LAST_MODIFIED);
        return Ok(Fetched::Body {
            bytes: read_capped(resp).await?,
            doc_url,
            etag,
            last_modified,
        });
    }
    Ok(Fetched::Body {
        bytes,
        doc_url: final_url,
        etag,
        last_modified,
    })
}

/// Pull the `charset` parameter out of a `Content-Type` header value, e.g.
/// `text/html; charset=Shift_JIS` → `Some("shift_jis")`. Case-insensitive,
/// tolerant of surrounding quotes and whitespace.
fn charset_from_content_type(content_type: &str) -> Option<String> {
    content_type
        .split(';')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("charset"))
        .map(|(_, v)| v.trim().trim_matches('"').trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

/// Read a charset label that begins at `tail` — the text right after a
/// `charset` keyword. Skips an optional `=` and surrounding quotes, then takes
/// the run of label characters.
fn parse_charset_value(tail: &str) -> Option<String> {
    let tail = tail.trim_start().strip_prefix('=').unwrap_or(tail);
    let value: String = tail
        .trim_start()
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!value.is_empty()).then_some(value)
}

/// Sniff a charset from an HTML document's own `<meta>` declaration —
/// `<meta charset=...>` or `<meta http-equiv="Content-Type" content="...">`.
/// Only the first 2 KiB are scanned (the `<head>` is required to declare the
/// charset early); the bytes are read as ASCII, which is safe because every
/// supported encoding is ASCII-compatible for the markup itself.
///
/// The keyword is only honoured when it sits *inside* a `<meta …>` tag. A bare
/// substring search would be derailed by an earlier, unrelated `charset`: a
/// CSS `@charset "utf-8";` rule in an inline `<style>`, the word in an early
/// `<title>` or comment, or `charset` in a `<script>` blob — any of which can
/// precede the real `<meta charset>` and would otherwise be picked instead,
/// mis-decoding the page.
fn charset_from_html(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(2048)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut search = 0;
    // Walk each `<meta …>` tag and look for `charset` within its bounds only.
    while let Some(rel) = text[search..].find("<meta") {
        let tag_start = search + rel;
        // The tag runs to the next '>' (or, in truncated HTML, the buffer end).
        let tag_end = text[tag_start..]
            .find('>')
            .map(|i| tag_start + i)
            .unwrap_or(text.len());
        let tag = &text[tag_start..tag_end];
        if let Some(i) = tag.find("charset") {
            if let Some(value) = parse_charset_value(&tag[i + "charset".len()..]) {
                return Some(value);
            }
        }
        search = tag_end;
    }
    None
}

/// Decode fetched HTML/text bytes into a `String` using the page's declared
/// character encoding rather than blindly assuming UTF-8. Many non-English
/// sites still serve Shift-JIS, GBK, EUC-KR or ISO-8859-1; decoding those as
/// UTF-8 produces mojibake that breaks full-text extraction, feed discovery
/// and YouTube channel-id resolution.
///
/// Encoding is resolved in priority order: the HTTP `Content-Type` charset,
/// then the document's own `<meta charset>`, then UTF-8 as the default.
/// Unknown labels fall back to UTF-8. Mirrors the WHATWG resource-decoding
/// order browsers use.
pub fn decode_html(bytes: &[u8], content_type: Option<&str>) -> String {
    let label = content_type
        .and_then(charset_from_content_type)
        .or_else(|| charset_from_html(bytes));
    let encoding = label
        .as_deref()
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

/// Plain GET returning `(body, content_type, final_url)` — used for feed
/// auto-discovery and full-text article extraction.
pub async fn get(client: &Client, url: &str) -> AppResult<(Vec<u8>, Option<String>, String)> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = read_capped(resp).await?;
    // An anti-bot challenge in place of the document: follow its JS redirect
    // once and surface the second response's body, type and final URL.
    if let Some(target) = js_redirect_target(&bytes, &final_url) {
        let resp = follow_challenge(client, &target).await?;
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        return Ok((read_capped(resp).await?, content_type, final_url));
    }
    Ok((bytes, content_type, final_url))
}

#[cfg(test)]
mod tests {
    use super::{charset_from_content_type, charset_from_html, decode_html, js_redirect_target};

    #[test]
    fn challenge_redirect_target_extracted_from_js_page() {
        // The exact shape the freedidi.com WAF serves: JSON-escaped slashes,
        // no <html> wrapper beyond the minimal document.
        let page = b"<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"robots\" content=\"noindex,nofollow\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<script>location.replace(\"\\/feed?_hgk=j9eff67319cf33ad0.1794633424.GVaSebtYlv\")\
</script></head><body></body></html>";
        assert_eq!(
            js_redirect_target(page, "https://www.freedidi.com/feed").as_deref(),
            Some("https://www.freedidi.com/feed?_hgk=j9eff67319cf33ad0.1794633424.GVaSebtYlv")
        );
    }

    #[test]
    fn challenge_redirect_absolute_target_and_single_quotes() {
        let page = b"<html><script>location.replace('https://example.com/feed?x=1')</script></html>";
        assert_eq!(
            js_redirect_target(page, "https://example.com/").as_deref(),
            Some("https://example.com/feed?x=1")
        );
    }

    #[test]
    fn regular_content_is_never_mistaken_for_a_challenge() {
        // A real RSS document: large, not HTML.
        let feed = b"<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>t</title>\
</channel></rss>";
        assert_eq!(js_redirect_target(feed, "https://example.com/feed"), None);
        // A real article page: HTML but far over the challenge size.
        let mut article = b"<!doctype html><html><body>".to_vec();
        article.extend(vec![b'x'; 4096]);
        article.extend_from_slice(b"location.replace('/x')</body></html>");
        assert_eq!(js_redirect_target(&article, "https://example.com/"), None);
        // HTML with a redirect but not via location.replace.
        let meta = b"<!doctype html><html><head><meta http-equiv=\"refresh\" content=\"0;url=/a\">\
</head></html>";
        assert_eq!(js_redirect_target(meta, "https://example.com/"), None);
        // No redirect at all.
        let plain = b"<!doctype html><html><body>hello</body></html>";
        assert_eq!(js_redirect_target(plain, "https://example.com/"), None);
    }

    #[test]
    fn quoted_call_in_page_content_is_not_a_challenge() {
        // A feed-discovery page quoting the call in a `<pre>` sample: the
        // redirect lives in visible page text, not in a script, so following
        // it would fetch the quoted example URL instead of a feed.
        let quoted = b"<!doctype html><html><head><title>Site - subscribe</title>\
<link rel=\"alternate\" type=\"application/rss+xml\" href=\"/feed\"></head>\
<body><pre>location.replace('/missing')</pre></body></html>";
        assert_eq!(js_redirect_target(quoted, "https://example.com/"), None);
        // A commented-out call is content too.
        let commented = b"<html><body><!-- location.replace('/nope') --></body></html>";
        assert_eq!(js_redirect_target(commented, "https://example.com/"), None);
    }

    #[test]
    fn quoted_call_in_script_string_or_comment_is_not_a_challenge() {
        // The call assigned to a variable inside a script never runs — it is
        // a string literal, and must not redirect the fetch.
        let in_string = b"<html><script>const example = \"location.replace('/missing')\";\
</script></html>";
        assert_eq!(js_redirect_target(in_string, "https://example.com/"), None);
        // Same for a commented-out call (line and block forms).
        let line_comment =
            b"<html><script>// location.replace('/missing')\n</script></html>";
        assert_eq!(js_redirect_target(line_comment, "https://example.com/"), None);
        let block_comment =
            b"<html><script>/* location.replace('/missing') */</script></html>";
        assert_eq!(js_redirect_target(block_comment, "https://example.com/"), None);
        // A real call after a quoted sample in the same script still wins.
        let mixed = b"<html><script>const s = \"location.replace('/sample')\";\
location.replace('/real?a=1')</script></html>";
        assert_eq!(
            js_redirect_target(mixed, "https://example.com/x").as_deref(),
            Some("https://example.com/real?a=1")
        );
    }

    #[test]
    fn redirect_only_page_with_a_title_is_a_challenge() {
        // Empty body + script + a short title: the shape many challenge pages
        // carry. Visible text stays within the contentless budget.
        let titled = b"<html><head><title>Redirecting...</title></head><body>\
<script>location.replace('/next?a=1')</script></body></html>";
        assert_eq!(
            js_redirect_target(titled, "https://example.com/x").as_deref(),
            Some("https://example.com/next?a=1")
        );
    }

    #[test]
    fn charset_parsed_from_content_type_header() {
        assert_eq!(
            charset_from_content_type("text/html; charset=UTF-8").as_deref(),
            Some("utf-8")
        );
        assert_eq!(
            charset_from_content_type("text/html;charset=\"Shift_JIS\"").as_deref(),
            Some("shift_jis")
        );
        assert_eq!(
            charset_from_content_type("text/html ; CHARSET = gbk ").as_deref(),
            Some("gbk")
        );
        assert_eq!(charset_from_content_type("text/html"), None);
        assert_eq!(charset_from_content_type("text/html; charset="), None);
    }

    #[test]
    fn charset_sniffed_from_html_meta() {
        assert_eq!(
            charset_from_html(b"<html><head><meta charset=\"euc-kr\">").as_deref(),
            Some("euc-kr")
        );
        assert_eq!(
            charset_from_html(
                b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=Shift_JIS\">"
            )
            .as_deref(),
            Some("shift_jis")
        );
        assert_eq!(charset_from_html(b"<html><head><title>x</title>"), None);
    }

    #[test]
    fn charset_ignores_keyword_outside_meta_tags() {
        // A CSS `@charset` rule in an inline <style> precedes the real
        // <meta charset>. The bare-substring scan would pick the CSS rule's
        // "utf-8" and never reach the genuine Shift_JIS declaration.
        let html = b"<head><style>@charset \"UTF-8\";</style>\
            <meta charset=\"Shift_JIS\"></head>";
        assert_eq!(charset_from_html(html).as_deref(), Some("shift_jis"));

        // The literal word "charset" in an early <title> must not be mistaken
        // for a declaration.
        let html = b"<head><title>How to set the charset</title>\
            <meta charset=\"euc-kr\"></head>";
        assert_eq!(charset_from_html(html).as_deref(), Some("euc-kr"));

        // "charset" only ever appearing outside a <meta> tag yields nothing.
        assert_eq!(
            charset_from_html(b"<head><title>charset basics</title></head>"),
            None
        );
    }

    #[test]
    fn decode_html_honours_header_charset() {
        // ISO-8859-1 byte 0xE9 is 'é'. Decoded as UTF-8 it would be replaced
        // with U+FFFD; with the declared charset it round-trips correctly.
        let bytes = b"caf\xe9";
        let decoded = decode_html(bytes, Some("text/html; charset=iso-8859-1"));
        assert_eq!(decoded, "café");
    }

    #[test]
    fn decode_html_falls_back_to_meta_then_utf8() {
        // No header charset — the <meta> declaration is used instead.
        let mut html: Vec<u8> = b"<meta charset=windows-1252><body>".to_vec();
        html.push(0x80); // windows-1252 0x80 == '€'
        let decoded = decode_html(&html, None);
        assert!(decoded.contains('€'), "decoded: {decoded:?}");

        // No charset anywhere — valid UTF-8 round-trips untouched.
        assert_eq!(decode_html("héllo".as_bytes(), Some("text/html")), "héllo");
    }

    #[test]
    fn decode_html_ignores_unknown_charset_label() {
        // A bogus charset label must not panic — it falls back to UTF-8.
        assert_eq!(
            decode_html("plain".as_bytes(), Some("text/html; charset=not-a-charset")),
            "plain"
        );
    }
}
