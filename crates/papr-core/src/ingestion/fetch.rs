//! HTTP fetching with conditional GET (ETag / If-Modified-Since) so unchanged
//! feeds cost a single 304 round-trip.

use crate::db;
use crate::error::{AppError, AppResult};
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::{Client, StatusCode};
use rusqlite::Connection;
use std::time::Duration;

/// The feed client's User-Agent. A plain browser string, deliberately:
/// several feed hosts gate automated clients on the UA alone — Tengine CDN
/// edge rules blacklist product-name UAs (`Papr/0.1 …` was denied outright by
/// feed.xyzfm.space's `denied by UA ACL = blacklist`) and Cloudflare serves
/// "Just a moment…" 403s to them (`macked.app`). A recognizable browser UA
/// passes both without touching their challenge flows. Mirrors what the
/// built-in X parser already sends (see `parsers::x::X_USER_AGENT`).
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

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
        // rustls, not the platform TLS stack (native-tls → Security.framework
        // on macOS): several feed hosts drop or reject Security.framework's
        // handshake while every other client connects — sindresorhus.com
        // (Vercel) fails with a native-tls `bad protocol version` and
        // www.torrentmac.net times out mid-handshake. rustls negotiates
        // cleanly with both (verified live 2026-11). `rustls-tls-native-roots`
        // keeps system-trusted CAs — a local MITM proxy's root (e.g. Surge's
        // generated CA, installed into the macOS keychain) still verifies.
        .use_rustls_tls()
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
/// - No `Accept-Encoding` is advertised: reqwest's automatic `gzip, br` lands
///   the request inside xiaohongshu's bot heuristics — the same request
///   without the header is served (verified live 2026-11; responses then
///   arrive uncompressed, so nothing needs decoding either).
pub fn build_parser_client(conn: &Connection) -> Client {
    let timeout = db::setting_parsed::<u64>(conn, "net_timeout_sec", 30);
    let proxy = db::get_setting(conn, "net_proxy")
        .ok()
        .flatten()
        .unwrap_or_else(|| "system".to_string());
    let mut builder = Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .gzip(false)
        .brotli(false)
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

/// A detected anti-bot challenge and what a browser would do about it: the
/// URL to fetch next, plus any cookies the page's script computes before
/// navigating (`document.cookie="name="+<computed value>`).
#[derive(Debug, PartialEq)]
pub struct ChallengeHop {
    /// Where the browser would navigate. For the self-reload shape
    /// (`location.replace(location.href)`) this is the original request URL.
    pub target: String,
    /// `name → value` cookies the page's script computes before navigating,
    /// in assignment order — ready for a `Cookie` header on the follow-up.
    pub cookies: Vec<(String, String)>,
}

/// Detect a JavaScript-redirect anti-bot challenge in a fetched body and
/// return what a browser would do next.
///
/// Some hosts (openresty-style WAFs) answer automated clients with a tiny HTML
/// page whose only content is `location.replace(…)` — a browser follows it
/// and gets the real document. A plain HTTP client stops there and reports
/// "could not parse the feed" on what is actually a block page.
///
/// Two challenge shapes are recognised:
///
/// * A **token redirect**: `location.replace("/path?token=…")` — the target
///   is the argument, resolved against the document URL. The real page is
///   served to a client that simply follows it.
/// * A **cookie reload** (freedidi.com's openresty WAF, 2026-11): the script
///   computes a pass cookie from string pieces (`var p=[…];
///   v=p[1].split("").reverse().join("")+…; document.cookie="hg_pass="+v;`)
///   and then navigates to `location.replace(location.href)` — the *same*
///   URL, now expected to carry the computed cookie. The assignments are
///   evaluated (see [`cookie_assignments`]) and returned so the follow-up
///   request can present them.
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
/// A self-reload with no computable cookie is rejected too: following it
/// would just fetch the same challenge page again, forever.
///
/// `None` for anything else.
pub fn js_redirect_target(bytes: &[u8], base_url: &str) -> Option<ChallengeHop> {
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
        || head
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case("<html"));
    if !looks_like_html {
        return None;
    }
    let dom = scraper::Html::parse_fragment(&text);
    let mut script_call: Option<String> = None;
    let mut scripts: Vec<&str> = Vec::new();
    let mut visible_chars = 0usize;
    for node in dom.tree.nodes() {
        let scraper::Node::Text(t) = node.value() else {
            continue;
        };
        let in_script = node
            .ancestors()
            .any(|a| a.value().as_element().is_some_and(|e| e.name() == "script"));
        if in_script {
            scripts.push(t);
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
    let base = reqwest::Url::parse(base_url).ok()?;
    if rest.starts_with('"') || rest.starts_with('\'') {
        // Token redirect: the argument is a quoted string; take up to the
        // matching quote (the pages we target never nest quotes inside). The
        // path comes back in a JS string literal, so `/` is escaped as `\/`.
        let arg = &rest[1..];
        let end = arg.find(rest[..1].chars().next()?)?;
        let raw = &arg[..end];
        if raw.is_empty() {
            return None;
        }
        let target = base.join(&raw.replace("\\/", "/")).ok()?;
        // WAF pass-throughs hand back the real document from the same site:
        // bound the hop to the challenge's own origin so a hostile page
        // cannot point the follow-up fetch anywhere else.
        let same_origin = target.scheme() == base.scheme()
            && target.host_str() == base.host_str()
            && target.port_or_known_default() == base.port_or_known_default();
        if !same_origin {
            return None;
        }
        return Some(ChallengeHop {
            target: target.into(),
            cookies: Vec::new(),
        });
    }
    if rest.starts_with("location.href") {
        // The identifier must end here: `location.hrefs` or
        // `location.href.split('?')[0]` is a different expression the
        // evaluator does not model.
        let tail = &rest["location.href".len()..];
        let delimited = tail
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '.' && c != '[');
        if delimited {
            // Cookie reload: the browser re-fetches the same URL, now
            // carrying the cookie the script computed. Without a computable
            // cookie the reload is a loop — reject.
            let https = base.scheme() == "https";
            let cookies = cookie_assignments(&scripts, https);
            if cookies.is_empty() {
                return None;
            }
            return Some(ChallengeHop {
                target: base_url.to_string(),
                cookies,
            });
        }
    }
    None
}

/// One JS value the challenge evaluator understands: a string (a literal or
/// a computed `var`) or a string array (the `var p=["…","…"]` pieces).
#[derive(Clone)]
enum JsVal {
    Str(String),
    Arr(Vec<String>),
}

/// Evaluate the `document.cookie=…` assignments (and the `var` declarations
/// they read) in the challenge page's scripts — outside JS strings and
/// comments — and return the `(name, value)` cookies they set.
///
/// Each assignment's RHS is a cookie *string* — `name=value; attributes` —
/// built by concatenating string literals, pieces looked up from `var`
/// declarations (with the `.split("").reverse().join("")` character-reversal
/// chain the WAF pages use to obfuscate), and the
/// `(location.protocol==="https:"?"A":"B")` scheme ternary. See
/// [`eval_cookie_expr`] for the exact grammar; anything the evaluator cannot
/// read makes that assignment (and with it, for the reload shape, the whole
/// challenge) fail safe.
fn cookie_assignments(scripts: &[&str], scheme_https: bool) -> Vec<(String, String)> {
    let mut vars: std::collections::HashMap<String, JsVal> = std::collections::HashMap::new();
    let mut cookies = Vec::new();
    for script in scripts {
        // Declarations and assignments are evaluated in document order, so
        // `var v=p[1].…` sees `p`'s elements and the cookie assignment sees
        // `v`.
        let mut statements: Vec<(usize, bool)> = Vec::new(); // (offset, is_cookie)
        for keyword in ["var ", "let ", "const "] {
            let mut from = 0;
            while let Some(rel) = script[from..].find(keyword) {
                let idx = from + rel;
                from = idx + keyword.len();
                statements.push((idx, false));
            }
        }
        let mut from = 0;
        while let Some(rel) = script[from..].find("document.cookie") {
            let idx = from + rel;
            from = idx + "document.cookie".len();
            statements.push((idx, true));
        }
        statements.sort_unstable();
        for (idx, is_cookie) in statements {
            // A mention inside a string literal or comment is sample content,
            // not a run statement.
            if inside_js_string_or_comment(script, idx) {
                continue;
            }
            if is_cookie {
                let rest = script[idx + "document.cookie".len()..].trim_start();
                let Some(assign) = rest.strip_prefix('=') else {
                    continue;
                };
                let Some(cookie_string) = eval_cookie_expr(assign, &vars, scheme_https) else {
                    continue;
                };
                // `name=value; attributes` — the pair ends at the first `;`
                // (a page may set bare `name=value` with no attributes), the
                // `=` inside it names it.
                let pair = cookie_string.split(';').next().unwrap_or(&cookie_string);
                let Some((name, value)) = pair.split_once('=') else {
                    continue;
                };
                let (name, value) = (name.trim(), value.trim());
                // Cookie values travel in a header: only printable ASCII is
                // sendable — a decoded `\n`/`\t` escape would be rejected by
                // reqwest at send time and fail the whole fetch.
                let printable =
                    |s: &str| s.chars().all(|c| (' '..='~').contains(&c)) && !s.is_empty();
                if !printable(name) || !printable(value) {
                    continue;
                }
                // A browser keeps the last value set for a name; the follow-up
                // header must too, so a re-set name replaces its earlier pair.
                match cookies.iter_mut().find(|(n, _)| n == name) {
                    Some(slot) => slot.1 = value.to_string(),
                    None => cookies.push((name.to_string(), value.to_string())),
                }
                continue;
            }
            // A `var/let/const NAME = …` declaration. The array shape holds
            // the string pieces; a string expression holds a computed value.
            let rest = &script[idx..];
            let keyword_len = rest.find(char::is_whitespace).unwrap_or(0);
            let after_kw = rest[keyword_len..].trim_start();
            let name: String = after_kw
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                continue;
            }
            let Some(after) = after_kw[name.len()..].trim_start().strip_prefix('=') else {
                continue;
            };
            if let Some(close) = after.trim_start().strip_prefix('[').and_then(|b| b.find(']')) {
                // `NAME=["str","str",…]` — comma-separated string literals
                // only; one non-literal fails the whole array.
                let body = &after.trim_start()[1..1 + close];
                let mut elements = Vec::new();
                let mut ok = true;
                for part in body.split(',') {
                    match js_string_literal(part.trim()) {
                        Some(s) => elements.push(s),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok && !elements.is_empty() {
                    vars.insert(name, JsVal::Arr(elements));
                }
                continue;
            }
            // A computed string: evaluate and remember, so a later
            // `document.cookie="x="+NAME` reads the final value.
            if let Some(value) = eval_cookie_expr(after, &vars, scheme_https) {
                vars.insert(name, JsVal::Str(value));
            }
        }
    }
    cookies
}

/// Evaluate a JS string-valued *concatenation expression* — the RHS of a
/// `document.cookie=` assignment or a computed `var` declaration, starting
/// right after its `=`. Terms:
///
/// * a string literal (`"…"`, `'…'`, `\`-escapes),
/// * `NAME[n]` — an element of a `var NAME=[…]` string array, optionally
///   followed by `.split("")` / `.reverse()` / `.join("")` postfixes in the
///   challenge pages' order (split into characters, reverse, re-join),
/// * a bare `NAME` naming a previously computed string `var`,
/// * `(location.protocol==="https:"?"A":"B")` — the scheme ternary, honoured
///   only in that exact shape; the branch is chosen by the fetch's scheme.
///
/// Terms are joined with `+`. The expression ends at a top-level `;` (a `;`
/// inside a string literal is part of the value, not a terminator) or at the
/// end of the scanned text. Returns the concatenated value; `None` for
/// anything outside the grammar — the statement is then skipped, which is
/// the pre-existing fail-safe behaviour for unreadable pages.
fn eval_cookie_expr(
    expr: &str,
    vars: &std::collections::HashMap<String, JsVal>,
    scheme_https: bool,
) -> Option<String> {
    let mut out = String::new();
    let mut rest = expr.trim_start();
    loop {
        let b = rest.as_bytes();
        let (value, len) = match b.first() {
            Some(b'"' | b'\'') => {
                let (v, l) = js_string_literal_span(rest)?;
                (v, l)
            }
            Some(b'(') => {
                let (v, l) = scheme_ternary(rest, scheme_https)?;
                (v, l)
            }
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
                let (v, l) = var_term(rest, vars)?;
                (v, l)
            }
            _ => return None,
        };
        out.push_str(&value);
        rest = rest[len..].trim_start();
        match rest.chars().next() {
            // `;` closes the statement; `}` closes the enclosing IIFE when a
            // page omits the final semicolon.
            Some(';') | Some('}') | None => return Some(out),
            Some('+') => rest = rest[1..].trim_start(),
            _ => return None,
        }
    }
}

/// One `NAME[n]` term with its optional `.split("")/.reverse()/.join("")`
/// postfix chain. Returns the term's value and the bytes consumed.
fn var_term(
    expr: &str,
    vars: &std::collections::HashMap<String, JsVal>,
) -> Option<(String, usize)> {
    let name: String = expr
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let mut rest = &expr[name.len()..];
    // `NAME[n]` indexes a string array; a bare `NAME` reads a computed string.
    let mut value = match vars.get(&name)? {
        JsVal::Str(s) => {
            if rest.trim_start().starts_with('[') {
                return None;
            }
            s.clone()
        }
        JsVal::Arr(elements) => {
            let after = rest.trim_start().strip_prefix('[')?;
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return None;
            }
            let closing = after[digits.len()..].trim_start().strip_prefix(']')?;
            rest = closing;
            elements.get(digits.parse::<usize>().ok()?)?.clone()
        }
    };
    // Postfix chain, in the order the challenge pages write it. `.split("")`
    // and `.join("")` are identity operations once the value is a `String`
    // whose characters are what `split` would produce; only `.reverse()`
    // changes the value.
    loop {
        if take_postfix(&mut rest, ".split(\"\")") || take_postfix(&mut rest, ".split('')") {
            continue;
        }
        if take_postfix(&mut rest, ".reverse()") {
            value = value.chars().rev().collect();
            continue;
        }
        if take_postfix(&mut rest, ".join(\"\")") || take_postfix(&mut rest, ".join('')") {
            continue;
        }
        break;
    }
    Some((value, expr.len() - rest.len()))
}

/// If `rest` starts with `postfix`, consume it (plus surrounding whitespace
/// before it) and return `true`.
fn take_postfix(rest: &mut &str, postfix: &str) -> bool {
    match rest.trim_start().strip_prefix(postfix) {
        Some(after) => {
            *rest = after;
            true
        }
        None => false,
    }
}

/// Parse `(location.protocol==="https:"?"A":"B")` starting at the `(`, in
/// the exact shape these challenge pages write. Returns the branch chosen by
/// `scheme_https` and the bytes consumed through the closing `)`.
fn scheme_ternary(expr: &str, scheme_https: bool) -> Option<(String, usize)> {
    let rest = expr
        .trim_start()
        .strip_prefix('(')?
        .trim_start()
        .strip_prefix("location.protocol")?
        .trim_start()
        .strip_prefix("===")?
        .trim_start()
        .strip_prefix("\"https:\"")?
        .trim_start()
        .strip_prefix('?')?
        .trim_start();
    let (when_https, used) = js_string_literal_span(rest)?;
    let rest = rest[used..].trim_start().strip_prefix(':')?.trim_start();
    let (other, used) = js_string_literal_span(rest)?;
    let rest = rest[used..].trim_start().strip_prefix(')')?;
    Some((
        if scheme_https { when_https } else { other },
        expr.len() - rest.len(),
    ))
}

/// A whole JS string literal (single or double quoted, `\`-escapes) at the
/// start of `s`: returns its decoded value and the bytes it occupies
/// (quotes included).
fn js_string_literal_span(s: &str) -> Option<(String, usize)> {
    let quote = s.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut out = String::new();
    let mut end = 1;
    let bytes = s.as_bytes();
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => {
                let esc = *bytes.get(end + 1)?;
                match esc {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'/' => out.push('/'),
                    b'\\' => out.push('\\'),
                    e if e == quote as u8 => out.push(e as char),
                    // The challenge pages never use other escapes; bail.
                    _ => return None,
                }
                end += 2;
            }
            b if b == quote as u8 => {
                return Some((out, end + 1));
            }
            _ => {
                let ch = s[end..].chars().next()?;
                out.push(ch);
                end += ch.len_utf8();
            }
        }
    }
    None
}

/// A whole JS string literal occupying all of `s` (used for array elements).
fn js_string_literal(s: &str) -> Option<String> {
    let (value, end) = js_string_literal_span(s)?;
    s[end..].trim().is_empty().then_some(value)
}

/// Is `idx` — the byte offset of a `location.replace(` / `document.cookie` /
/// `var` candidate inside the script source `script` — inside a JS string
/// literal or comment? A tiny lexer tracks line/block comments and the three
/// quote styles (with `\` escapes, backticks included) from the start of the
/// script: a call mentioned inside `"…"` or `// …` is sample content, not a
/// redirect the browser would run. JS regex literals are not modelled — at
/// worst a regex containing `//` hides a real call, which fails safe (the
/// page is simply not followed, as before the challenge handling existed).
/// An unterminated string or comment fails safe the same way.
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
/// and any cookies the challenge's script computes, and return the second
/// response untouched. One hop only — if the target is itself a challenge
/// page it is returned as-is rather than looping.
///
/// The computed cookies travel as an explicit `Cookie` header — reqwest's
/// jar only fills the header in when the request doesn't already carry one,
/// so the explicit values win.
async fn follow_challenge(
    client: &Client,
    target: &str,
    cookies: &[(String, String)],
) -> AppResult<reqwest::Response> {
    let mut req = client.get(target);
    if !cookies.is_empty() {
        let header = cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        req = req.header(reqwest::header::COOKIE, header);
    }
    let resp = req.send().await?;
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
    // redirect once (presenting any cookie the challenge's script computes)
    // and serve whatever comes back (with the second response's revalidation
    // headers).
    if let Some(hop) = js_redirect_target(&bytes, &final_url) {
        let resp = follow_challenge(client, &hop.target, &hop.cookies).await?;
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
    // once (with the challenge's computed cookies, if any) and surface the
    // second response's body, type and final URL.
    if let Some(hop) = js_redirect_target(&bytes, &final_url) {
        let resp = follow_challenge(client, &hop.target, &hop.cookies).await?;
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
    use super::{
        charset_from_content_type, charset_from_html, decode_html, js_redirect_target,
        ChallengeHop,
    };

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
            js_redirect_target(page, "https://www.freedidi.com/feed"),
            Some(ChallengeHop {
                target: "https://www.freedidi.com/feed?_hgk=j9eff67319cf33ad0.1794633424.GVaSebtYlv"
                    .into(),
                cookies: vec![],
            })
        );
    }

    #[test]
    fn challenge_redirect_absolute_target_and_single_quotes() {
        let page =
            b"<html><script>location.replace('https://example.com/feed?x=1')</script></html>";
        assert_eq!(
            js_redirect_target(page, "https://example.com/"),
            Some(ChallengeHop {
                target: "https://example.com/feed?x=1".into(),
                cookies: vec![],
            })
        );
    }

    #[test]
    fn challenge_redirect_cross_origin_target_is_rejected() {
        // A pass-through hop is a same-site round trip; a page pointing the
        // follow-up fetch elsewhere (or at a loopback admin port) is not a
        // challenge to follow.
        let cross = b"<html><script>location.replace('https://evil.example/steal')</script></html>";
        assert_eq!(js_redirect_target(cross, "https://example.com/feed"), None);
        let loopback =
            b"<html><script>location.replace('http://127.0.0.1:9222/admin')</script></html>";
        assert_eq!(js_redirect_target(loopback, "https://example.com/feed"), None);
        let downgraded =
            b"<html><script>location.replace('http://example.com/feed')</script></html>";
        assert_eq!(js_redirect_target(downgraded, "https://example.com/feed"), None);
    }

    #[test]
    fn challenge_cookie_reload_evaluates_the_computed_cookie() {
        // The exact page freedidi.com's openresty WAF served 2026-11: compute
        // `hg_pass` from two reversed string pieces, set the cookie, then
        // reload the same URL — `location.replace(location.href)`.
        let page = b"<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"robots\" content=\"noindex,nofollow\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"></head>\
<body><noscript>Please enable JavaScript.</noscript><script>(function(){var p=[\"Gleshs2K42YUGjfns6RFDA.351\",\"5394971.f72a355dec38a9b4s\"];\
var v=p[1].split(\"\").reverse().join(\"\")+p[0].split(\"\").reverse().join(\"\");\
document.cookie=\"hg_pass=\"+v+\"; path=/; max-age=7200; SameSite=Lax\"+(location.protocol===\"https:\"?\"; Secure\":\"\");\
location.replace(location.href)})()</script></body></html>";
        let hop = js_redirect_target(page, "https://www.freedidi.com/feed").expect("challenge");
        assert_eq!(hop.target, "https://www.freedidi.com/feed");
        assert_eq!(
            hop.cookies,
            vec![(
                "hg_pass".to_string(),
                "s4b9a83ced553a27f.1794935153.ADFR6snfjGUY24K2shselG".to_string()
            )]
        );
    }

    #[test]
    fn cookie_reload_without_a_computable_cookie_is_rejected() {
        // A self-reload that sets no cookie would loop forever — not a
        // challenge to follow.
        let bare = b"<html><script>location.replace(location.href)</script></html>";
        assert_eq!(js_redirect_target(bare, "https://example.com/feed"), None);
        // An assignment the evaluator cannot read fails the same way.
        let opaque = b"<html><script>document.cookie=\"a=\"+crypto.randomUUID();\
location.replace(location.href)</script></html>";
        assert_eq!(js_redirect_target(opaque, "https://example.com/feed"), None);
    }

    #[test]
    fn location_href_must_end_at_the_identifier() {
        // `location.hrefs`, `location.href.split('?')[0]` and friends are
        // different expressions the evaluator does not model — navigating to
        // base_url on their behalf would guess wrong.
        let with_method = b"<html><script>var p=[\"x\"];\
document.cookie=\"a=\"+p[0]+\"; path=/\";\
location.replace(location.href.split('?')[0])</script></html>";
        assert_eq!(js_redirect_target(with_method, "https://example.com/feed"), None);
        let typo = b"<html><script>var p=[\"x\"];document.cookie=\"a=\"+p[0]+\"; path=/\";\
location.replace(location.hrefs)</script></html>";
        assert_eq!(js_redirect_target(typo, "https://example.com/feed"), None);
    }

    #[test]
    fn cookie_shapes_beyond_the_freedidi_page() {
        // Bare `name=value` with no `; attributes` is a valid assignment.
        let bare = b"<html><script>var p=[\"5394971.f72a355dec38a9b4s\"];\
document.cookie=\"hg_pass=\"+p[0].split(\"\").reverse().join(\"\");\
location.replace(location.href)</script></html>";
        let hop = js_redirect_target(bare, "https://example.com/feed").expect("challenge");
        assert_eq!(
            hop.cookies,
            vec![("hg_pass".to_string(), "s4b9a83ced553a27f.1794935".to_string())]
        );
        // A name re-set by a later assignment keeps the last value, in
        // first-seen order.
        let twice = b"<html><script>var p=[\"one\",\"two\"];\
document.cookie=\"a=\"+p[0]+\"; path=/\";\
document.cookie=\"b=\"+p[1]+\"; path=/\";\
document.cookie=\"a=\"+p[1]+\"; path=/\";\
location.replace(location.href)</script></html>";
        let hop = js_redirect_target(twice, "https://example.com/feed").expect("challenge");
        assert_eq!(
            hop.cookies,
            vec![
                ("a".to_string(), "two".to_string()),
                ("b".to_string(), "two".to_string()),
            ]
        );
        // A value with a control character (decoded \n) is not sendable in a
        // header — the assignment is skipped, and with nothing left to
        // present the reload is rejected.
        let control = b"<html><script>var p=[\"1\\n2\"];\
document.cookie=\"a=\"+p[0]+\"; path=/\";\
location.replace(location.href)</script></html>";
        assert_eq!(js_redirect_target(control, "https://example.com/feed"), None);
    }

    #[test]
    fn quoted_cookie_and_reload_samples_are_not_a_challenge() {
        // The whole reload shape quoted in visible page text (a <pre> sample)
        // or inside a script string stays content.
        let quoted = b"<html><body><pre>location.replace(location.href)</pre></body></html>";
        assert_eq!(js_redirect_target(quoted, "https://example.com/"), None);
        let in_string = b"<html><script>const s=\"document.cookie='a=b';\
location.replace(location.href)\"</script></html>";
        assert_eq!(js_redirect_target(in_string, "https://example.com/"), None);
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
        let line_comment = b"<html><script>// location.replace('/missing')\n</script></html>";
        assert_eq!(
            js_redirect_target(line_comment, "https://example.com/"),
            None
        );
        let block_comment = b"<html><script>/* location.replace('/missing') */</script></html>";
        assert_eq!(
            js_redirect_target(block_comment, "https://example.com/"),
            None
        );
        // A real call after a quoted sample in the same script still wins.
        let mixed = b"<html><script>const s = \"location.replace('/sample')\";\
location.replace('/real?a=1')</script></html>";
        assert_eq!(
            js_redirect_target(mixed, "https://example.com/x"),
            Some(ChallengeHop {
                target: "https://example.com/real?a=1".into(),
                cookies: vec![],
            })
        );
    }

    #[test]
    fn redirect_only_page_with_a_title_is_a_challenge() {
        // Empty body + script + a short title: the shape many challenge pages
        // carry. Visible text stays within the contentless budget.
        let titled = b"<html><head><title>Redirecting...</title></head><body>\
<script>location.replace('/next?a=1')</script></body></html>";
        assert_eq!(
            js_redirect_target(titled, "https://example.com/x"),
            Some(ChallengeHop {
                target: "https://example.com/next?a=1".into(),
                cookies: vec![],
            })
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
