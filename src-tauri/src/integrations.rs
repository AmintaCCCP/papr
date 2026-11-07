//! Article integrations — send the open article to a configured "save it
//! somewhere" service (Cubox, Readwise, Instapaper, Outline, Readeck, Zotero,
//! Obsidian) or to a user-defined webhook / URL scheme.
//!
//! The frontend owns the configuration (one JSON blob in the settings table)
//! and posts the resolved spec per invocation; this module loads the article,
//! builds the payload and performs the call from Rust, so the webview never
//! faces cross-origin restrictions and the shared proxy/timeout client applies.
//! Error text never echoes request URLs or header values — both can carry
//! user credentials a webhook config embeds.

use crate::error::{AppError, AppResult};
use crate::models::ArticleDetail;
use crate::state::AppState;
use base64::Engine as _;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

// ─────────────────────────── configuration ───────────────────────────

/// The fields one invocation needs. Built-ins use disjoint subsets (e.g. Cubox
/// only `api_url`); custom integrations use `method` / `url` / `headers` /
/// `body` or `scheme`. Unused fields are simply absent — the frontend sends
/// the config object of the integration being run.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationConfig {
    /// Cubox: the personal API URL copied from Cubox's extension settings.
    pub api_url: Option<String>,
    /// Readwise / Outline / Readeck / Zotero access token.
    pub token: Option<String>,
    /// Instapaper credentials (HTTP Basic).
    pub username: Option<String>,
    pub password: Option<String>,
    /// Outline / Readeck server root (e.g. `https://wiki.example.com/api`).
    pub endpoint: Option<String>,
    /// Outline collection — a UUID or a collection name resolved on the fly.
    pub collection: Option<String>,
    /// Zotero user id (API URL path segment).
    pub user_id: Option<String>,
    /// Obsidian: absolute path of the target vault directory.
    pub vault_path: Option<String>,
    /// Custom webhook: method, URL template, headers, body template.
    pub method: Option<String>,
    pub url: Option<String>,
    pub headers: Option<std::collections::HashMap<String, String>>,
    pub body: Option<String>,
    /// Custom URL-scheme integration (e.g. `obsidian://new?…`).
    pub scheme: Option<String>,
}

/// What a successful run reports back: a "view it" link when the service
/// returned one (Readwise, Instapaper, Outline, Readeck), else `None` — the
/// frontend surfaces it in the completion toast.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunOutcome {
    pub url: Option<String>,
}

#[tauri::command]
pub async fn run_integration(
    app: AppHandle,
    state: State<'_, AppState>,
    kind: String,
    use_ai_summary: bool,
    config: IntegrationConfig,
    article_id: i64,
) -> AppResult<RunOutcome> {
    // Load the article (plus its feed's URL for the Obsidian frontmatter)
    // behind a short-lived read lock; the network work below needs none.
    let (mut article, feed_url) = {
        let conn = state.read().await;
        let article = crate::db::get_article(&conn, article_id)?;
        let feed_url = match kind == "obsidian" {
            true => crate::db::list_feeds(&conn).ok().and_then(|feeds| {
                feeds
                    .into_iter()
                    .find(|f| f.id == article.feed_id)
                    .map(|f| f.feed_url)
            }),
            false => None,
        };
        (article, feed_url)
    };
    // "AI summary as description" (Folo's `generateSummary` on-demand flow):
    // with the preference on, an article that has no summary yet gets one
    // generated right here, before the send — so the description the service
    // receives is a real summary instead of the plain-text excerpt fallback.
    // A failure fails the whole send loudly rather than silently saving a
    // non-summary description.
    if use_ai_summary && article.ai_summary.as_deref().map_or(true, |s| s.trim().is_empty()) {
        ensure_ai_summary(&state, &mut article).await?;
    }
    let ctx = ArticleContext::build(article, feed_url).await?;

    let client = state.http();
    let url = dispatch(&app, &client, &kind, use_ai_summary, &config, &ctx).await?;
    Ok(RunOutcome { url })
}

/// Generate, persist, and store on `article` the AI summary used as the
/// description when the "AI summary as description" preference is on and the
/// article has none yet. Shares the reader's summary prompt
/// (`commands::summary_system_prompt`), so a custom prompt set in Settings
/// shapes both paths identically, and the persisted result is cached the same
/// way — the next reader open (or send) shows it without spending another
/// API call. Runs off the connection lock: the completion is network work.
async fn ensure_ai_summary(
    state: &State<'_, AppState>,
    article: &mut ArticleDetail,
) -> AppResult<()> {
    let (cfg, system, user) = {
        let conn = state.read().await;
        let (title, body) = crate::db::article_text(&conn, article.id)?;
        let cfg = crate::commands::load_ai_config(&conn)?;
        // A title-only item gives the model nothing to summarize and the
        // reader's summarize bails with the same error — mirror it so the
        // failure is explicit instead of a fabricated summary from the title.
        if body.trim().is_empty() {
            return Err(AppError::code("noArticleBody"));
        }
        (
            cfg,
            crate::commands::summary_system_prompt(&conn),
            crate::commands::summary_user_turn(&title, &body),
        )
    };
    let http = state.http();
    let text = crate::ai::complete_chat(
        &http,
        &cfg,
        &system,
        &user,
        crate::ai::MAX_TOKENS,
    )
    .await?;
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::code("aiEmptyResponse"));
    }
    {
        let conn = state.db.lock().await;
        crate::db::set_ai_summary(&conn, article.id, text)?;
    }
    article.ai_summary = Some(text.to_string());
    Ok(())
}

/// Send the article through the integration `kind` names. Returns a
/// "view it" link when the target service reported one.
async fn dispatch(
    app: &AppHandle,
    client: &Client,
    kind: &str,
    use_ai_summary: bool,
    config: &IntegrationConfig,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    match kind {
        "cubox" => cubox(client, config, use_ai_summary, ctx).await,
        "readwise" => readwise(client, config, use_ai_summary, ctx).await,
        "instapaper" => instapaper(client, config, ctx).await,
        "outline" => outline(client, config, ctx).await,
        "readeck" => readeck(client, config, ctx).await,
        "zotero" => zotero(client, config, use_ai_summary, ctx).await,
        "obsidian" => obsidian(config, use_ai_summary, ctx).await,
        "webhook" => webhook(client, config, use_ai_summary, ctx).await,
        "urlScheme" => url_scheme(app, config, use_ai_summary, ctx),
        _ => Err(AppError::code("integrationUnknown")),
    }
}

// ─────────────────────────── article context ───────────────────────────

/// Everything a sender needs about the article, pre-derived once.
struct ArticleContext {
    title: String,
    author: String,
    url: String,
    /// The best body HTML available (full-text extraction, else feed content).
    html: String,
    /// The body as Markdown (Outline document text, Obsidian note body,
    /// the webhook `[content_markdown]` token).
    markdown: String,
    /// Feed-style description: the body's plain text, truncated. The
    /// "AI summary as description" preference overrides this per sender.
    description: String,
    ai_summary: Option<String>,
    published_at: Option<String>,
    feed_title: String,
    /// Only resolved for the Obsidian frontmatter (`run_integration` fills it).
    feed_url: Option<String>,
}

impl ArticleContext {
    /// Build the context. The Markdown conversion and plain-text derivation
    /// parse the whole body HTML — CPU work done on the blocking pool, the
    /// same treatment `extract_fulltext` gives Readability.
    async fn build(article: ArticleDetail, feed_url: Option<String>) -> AppResult<Self> {
        let url = article
            .url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| AppError::code("noArticleUrl"))?;
        let html = article
            .extracted_html
            .clone()
            .or_else(|| article.content_html.clone())
            .unwrap_or_default();
        let (markdown, description) =
            tauri::async_runtime::spawn_blocking(move || derive_body(&html))
                .await
                .map_err(|e| AppError::other(format!("article context task: {e}")))?;
        Ok(Self {
            title: non_empty(&article.title).unwrap_or_else(|| url.clone()),
            author: article.author.clone().unwrap_or_default(),
            url,
            html: article
                .extracted_html
                .or(article.content_html)
                .unwrap_or_default(),
            markdown,
            description,
            ai_summary: article.ai_summary.filter(|s| !s.trim().is_empty()),
            published_at: article.published_at,
            feed_title: article.feed_title,
            feed_url,
        })
    }

    /// The description a sender should carry: the AI summary when the
    /// "AI summary as description" preference is on and one exists, else the
    /// feed-style description derived from the body.
    fn description_with(&self, use_ai_summary: bool) -> String {
        match use_ai_summary {
            true => self
                .ai_summary
                .clone()
                .unwrap_or_else(|| self.description.clone()),
            false => self.description.clone(),
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Derive the body's Markdown and plain-text description from its HTML —
/// the CPU-heavy part of building an [`ArticleContext`], split out so the
/// command can run it on the blocking pool and tests can exercise it directly.
fn derive_body(html: &str) -> (String, String) {
    let markdown = if html.is_empty() {
        String::new()
    } else {
        htmd::convert(html).unwrap_or_default()
    };
    let description = truncate_chars(plain_text(html).trim(), 300);
    (markdown, description)
}

/// Entity-decoded text of an HTML fragment — text nodes joined with spaces.
/// (Feed HTML arrives sanitized, so script/style nodes never contribute.)
fn plain_text(html: &str) -> String {
    let doc = scraper::Html::parse_fragment(html);
    let mut out = String::new();
    for node in doc.tree.nodes() {
        if let scraper::node::Node::Text(t) = node.value() {
            let s = t.trim();
            if !s.is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(s);
            }
        }
    }
    out
}

/// Truncate to at most `max` characters without splitting a UTF-8 boundary.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// RFC-3339 timestamp reduced to whole seconds with a `Z` suffix — the shape
/// Zotero's `accessDate` and the Obsidian frontmatter expect.
fn naive_iso(ts: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

// ─────────────────────────── placeholder engine ───────────────────────────

/// Context a custom template interpolates from.
pub(crate) struct PlaceholderContext<'a> {
    pub title: &'a str,
    pub url: &'a str,
    pub content_html: &'a str,
    pub content_markdown: &'a str,
    /// AI summary when that preference is on, else the feed-style description.
    pub summary: &'a str,
    pub description: &'a str,
    pub author: &'a str,
    pub published_at: &'a str,
}

/// How a token's value is escaped when substituted.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum TokenMode {
    /// URL position — fully percent-encoded (`encodeURIComponent`).
    Url,
    /// Header position — verbatim.
    Raw,
    /// Body position — JSON-escaped when the body declares `application/json`.
    Json { json_body: bool },
}

/// The eight tokens a custom template may interpolate, in display order.
pub(crate) const PLACEHOLDER_TOKENS: [&str; 8] = [
    "[title]",
    "[url]",
    "[content_html]",
    "[content_markdown]",
    "[summary]",
    "[description]",
    "[author]",
    "[published_at]",
];

fn token_value<'a>(ctx: &'a PlaceholderContext<'_>, token: &str) -> &'a str {
    match token {
        "[title]" => ctx.title,
        "[url]" => ctx.url,
        "[content_html]" => ctx.content_html,
        "[content_markdown]" => ctx.content_markdown,
        "[summary]" => ctx.summary,
        "[description]" => ctx.description,
        "[author]" => ctx.author,
        "[published_at]" => ctx.published_at,
        _ => "",
    }
}

/// Percent-encode everything outside the unreserved set — a stricter
/// `encodeURIComponent` (safe in both query strings and path segments).
fn url_encode(value: &str) -> String {
    const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    utf8_percent_encode(value, UNRESERVED).to_string()
}

/// JSON-escape a string for splicing inside a JSON body's quoted value
/// (the escaped content `JSON.stringify` would produce, without the quotes).
/// Strips exactly one leading/trailing quote — a `trim_matches` would also
/// eat a trailing *escaped* quote inside the value.
fn json_escape(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap_or_default();
    let inner = quoted.strip_prefix('"').unwrap_or(&quoted);
    inner.strip_suffix('"').unwrap_or(inner).to_string()
}

pub(crate) fn replace_placeholders(
    template: &str,
    ctx: &PlaceholderContext<'_>,
    mode: TokenMode,
) -> String {
    let escape = |value: &str| match mode {
        TokenMode::Url => url_encode(value),
        TokenMode::Raw => value.to_string(),
        TokenMode::Json { json_body } => {
            if json_body {
                json_escape(value)
            } else {
                value.to_string()
            }
        }
    };
    // Single left-to-right scan: each token is replaced once and its escaped
    // value is appended without being rescanned, so article content that
    // itself contains a literal token (a title like "use [summary] here")
    // can never be re-expanded into another field's slot.
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    'scan: while !rest.is_empty() {
        for token in PLACEHOLDER_TOKENS {
            if let Some(after) = rest.strip_prefix(token) {
                out.push_str(&escape(token_value(ctx, token)));
                rest = after;
                continue 'scan;
            }
        }
        // No token starts here — copy one character and move on. Tokens are
        // ASCII, so the scan only ever matches at char boundaries.
        let ch = rest.chars().next().expect("non-empty checked above");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Normalize a webhook's headers: trim + lowercase keys, substitute tokens
/// verbatim, drop empty keys, and default `Content-Type` to
/// `application/json` for the body-carrying methods that ship a body without
/// declaring one (mirrors what every JSON API the webhook form targets
/// expects).
pub(crate) fn effective_headers(
    method: &str,
    headers: &std::collections::HashMap<String, String>,
    ctx: &PlaceholderContext<'_>,
    has_body: bool,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut has_content_type = false;
    for (key, value) in headers {
        let key = key.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        let value = replace_placeholders(value, ctx, TokenMode::Raw);
        if key == "content-type" && !value.trim().is_empty() {
            has_content_type = true;
        }
        out.push((key, value));
    }
    let body_method = matches!(method, "POST" | "PUT" | "PATCH");
    if has_body && body_method && !has_content_type {
        out.push(("content-type".to_string(), "application/json".to_string()));
    }
    out
}

// ─────────────────────────── HTTP plumbing ───────────────────────────

/// API responses are small JSON — 1 MiB stops a misbehaving endpoint from
/// buffering unbounded output while leaving real responses untouched.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Read a response body, aborting past `MAX_RESPONSE_BYTES`.
async fn read_body_capped(mut resp: reqwest::Response) -> AppResult<String> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| AppError::other(describe_reqwest_error(&e)))?
    {
        if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(AppError::other("response too large"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// A reqwest error message that never quotes the request URL — webhook URLs
/// routinely embed API keys as query parameters.
fn describe_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "request timed out".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else {
        match e.status() {
            Some(s) => format!("HTTP {s}"),
            None => "request failed".to_string(),
        }
    }
}

/// First 200 characters of a response body, for the error toast. Control
/// characters are dropped so the toast stays one line.
fn response_snippet(body: &str) -> String {
    let one_line: String = body.chars().filter(|c| !c.is_control()).take(200).collect();
    truncate_chars(one_line.trim(), 200)
}

/// Send a request, cap the read, and turn a non-2xx into an error carrying
/// the status and a short server-message snippet.
async fn finish(req: reqwest::RequestBuilder) -> AppResult<String> {
    let resp = req
        .send()
        .await
        .map_err(|e| AppError::other(describe_reqwest_error(&e)))?;
    let status = resp.status();
    let body = read_body_capped(resp).await?;
    if !status.is_success() {
        return Err(AppError::other(format!(
            "HTTP {status}: {}",
            response_snippet(&body)
        )));
    }
    Ok(body)
}

/// Parse a response body as JSON, tolerating non-JSON (some services answer
/// with an empty body on success).
fn parse_json(body: &str) -> Option<Value> {
    serde_json::from_str(body).ok()
}

/// Read `key` from a response, accepting both the bare object and Outline's
/// `{ "data": … }` wrapper.
fn json_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Pick the Outline collection whose name matches (case-insensitive) from a
/// `collections.list` response — the items live under `data`, but the bare
/// array shape is accepted too.
fn outline_collection_id(v: &Value, name: &str) -> Option<String> {
    let items = v.get("data").unwrap_or(v).as_array()?;
    items
        .iter()
        .filter_map(|c| {
            let id = c.get("id").and_then(Value::as_str)?;
            let cname = c.get("name").and_then(Value::as_str)?;
            Some((cname, id))
        })
        .find(|(cname, _)| cname.eq_ignore_ascii_case(name))
        .map(|(_, id)| id.to_string())
        .filter(|id| !id.is_empty())
}

/// A missing credential — the settings UI marks which fields are required,
/// so a generic localized message is enough here.
fn require<'a>(value: &'a Option<String>, _what: &str) -> AppResult<&'a str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::code("integrationNotConfigured"))
}

// ─────────────────────────── built-in senders ───────────────────────────

/// Cubox — POST to the user's personal API URL.
async fn cubox(
    client: &Client,
    config: &IntegrationConfig,
    use_ai_summary: bool,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let api_url = require(&config.api_url, "Cubox API URL")?;
    finish(
        client
            .post(api_url)
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({
                "type": "url",
                "content": ctx.url,
                "title": ctx.title,
                "description": ctx.description_with(use_ai_summary),
                "tags": [],
                "folder": "",
            })),
    )
    .await?;
    Ok(None)
}

/// Readwise Reader — Save API v3.
async fn readwise(
    client: &Client,
    config: &IntegrationConfig,
    use_ai_summary: bool,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let token = require(&config.token, "Readwise token")?;
    let mut body = serde_json::Map::new();
    body.insert("url".into(), json!(ctx.url));
    body.insert("title".into(), json!(ctx.title));
    if !ctx.html.is_empty() {
        body.insert("html".into(), json!(ctx.html));
    }
    if !ctx.author.is_empty() {
        body.insert("author".into(), json!(ctx.author));
    }
    body.insert(
        "summary".into(),
        json!(ctx.description_with(use_ai_summary)),
    );
    if let Some(published) = ctx.published_at.as_deref() {
        body.insert("published_date".into(), json!(published));
    }
    body.insert("saved_using".into(), json!("Papr"));
    let resp = finish(
        client
            .post("https://readwise.io/api/v3/save/")
            .header(CONTENT_TYPE, "application/json")
            .bearer_auth(token)
            .json(&Value::Object(body)),
    )
    .await?;
    Ok(parse_json(&resp)
        .as_ref()
        .and_then(|v| json_field(v, "url"))
        .map(String::from))
}

/// Instapaper — simple add API with HTTP Basic credentials.
async fn instapaper(
    client: &Client,
    config: &IntegrationConfig,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let username = require(&config.username, "Instapaper username")?;
    let password = require(&config.password, "Instapaper password")?;
    let credentials =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    let url = format!(
        "https://www.instapaper.com/api/add?url={}&title={}",
        url_encode(&ctx.url),
        url_encode(&ctx.title)
    );
    let body = finish(
        client
            .post(url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("Authorization", format!("Basic {credentials}")),
    )
    .await?;
    // Success answers `{"bookmark_id": 123, …}`; the link follows the reader
    // URL shape. A non-JSON answer still counts as saved.
    let id = parse_json(&body).and_then(|v| v.get("bookmark_id").and_then(Value::as_i64));
    Ok(id.map(|i| format!("https://www.instapaper.com/read/{i}")))
}

/// Outline — resolve the collection (name → id) then create the document.
async fn outline(
    client: &Client,
    config: &IntegrationConfig,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let endpoint = require(&config.endpoint, "Outline endpoint")?
        .trim_end_matches('/')
        .to_string();
    let token = require(&config.token, "Outline token")?;
    let collection = require(&config.collection, "Outline collection")?;

    let collection_id = match is_uuid_like(collection) {
        true => collection.to_string(),
        false => {
            // `collections.info` only accepts a UUID id, so a collection
            // *name* is resolved through `collections.list`, which filters by
            // a free-form `query`; the exact match (case-insensitive) then
            // happens client-side.
            let body = finish(
                client
                    .post(format!("{endpoint}/collections.list"))
                    .header(CONTENT_TYPE, "application/json")
                    .bearer_auth(token)
                    .json(&json!({ "query": collection })),
            )
            .await?;
            parse_json(&body)
                .and_then(|v| outline_collection_id(&v, collection))
                .ok_or_else(|| AppError::other("could not resolve the Outline collection name"))?
        }
    };

    let body = finish(
        client
            .post(format!("{endpoint}/documents.create"))
            .header(CONTENT_TYPE, "application/json")
            .bearer_auth(token)
            .json(&json!({
                "title": ctx.title,
                "text": ctx.markdown,
                "collectionId": collection_id,
                "publish": true,
            })),
    )
    .await?;
    Ok(parse_json(&body)
        .as_ref()
        .and_then(|v| json_field(v, "url"))
        .map(String::from))
}

/// `36-8-4-4-4-12` hex UUID shape (any version — the check only decides
/// whether an Outline collection id needs a name→id round trip).
fn is_uuid_like(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        let dash = matches!(i, 8 | 13 | 18 | 23);
        match dash {
            true if *b != b'-' => return false,
            false if !b.is_ascii_hexdigit() => return false,
            _ => {}
        }
    }
    true
}

/// Readeck — bookmark API, multipart form, link from `Location`.
async fn readeck(
    client: &Client,
    config: &IntegrationConfig,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let endpoint = require(&config.endpoint, "Readeck endpoint")?
        .trim_end_matches('/')
        .to_string();
    let token = require(&config.token, "Readeck token")?;
    let form = reqwest::multipart::Form::new()
        .text("url", ctx.url.clone())
        .text("title", ctx.title.clone());
    let resp = client
        .post(format!("{endpoint}/api/bookmarks"))
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| AppError::other(describe_reqwest_error(&e)))?;
    let location = resp
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let status = resp.status();
    let body = read_body_capped(resp).await?;
    if !status.is_success() {
        return Err(AppError::other(format!(
            "HTTP {status}: {}",
            response_snippet(&body)
        )));
    }
    Ok(location)
}

/// Zotero — one `webpage` item posted to the user library.
async fn zotero(
    client: &Client,
    config: &IntegrationConfig,
    use_ai_summary: bool,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let user_id = require(&config.user_id, "Zotero user id")?;
    let token = require(&config.token, "Zotero API key")?;
    let creators = if ctx.author.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "creatorType": "author",
            "firstName": ctx.author,
            "lastName": "",
        })]
    };
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let date = ctx
        .published_at
        .as_deref()
        .and_then(naive_iso)
        .unwrap_or_default();
    let body = finish(
        client
            .post(format!("https://api.zotero.org/users/{user_id}/items"))
            .header(CONTENT_TYPE, "application/json")
            .header("Zotero-API-Key", token)
            .json(&json!([{
                "itemType": "webpage",
                "title": ctx.title,
                "creators": creators,
                "abstractNote": ctx.description_with(use_ai_summary),
                "websiteTitle": ctx.title,
                "websiteType": "",
                "date": date,
                "url": ctx.url,
                "accessDate": now,
                "language": "",
                "shortTitle": "",
                "rights": "",
                "extra": "",
                "tags": [],
                "collections": [],
                "relations": {},
            }])),
    )
    .await?;
    // Zotero answers 200 even when individual items fail — the write
    // endpoint reports them in the response's `failed` object. Anything but
    // a parseable response counts as a failed send too.
    let value =
        parse_json(&body).ok_or_else(|| AppError::other("Zotero returned a non-JSON response"))?;
    if let Some(message) = zotero_failure(&value) {
        return Err(AppError::other(format!(
            "Zotero rejected the item: {message}"
        )));
    }
    Ok(None)
}

/// The message of the first per-item failure in a Zotero write response.
fn zotero_failure(v: &Value) -> Option<String> {
    let failed = v.get("failed")?.as_object()?;
    (!failed.is_empty()).then(|| {
        failed
            .values()
            .find_map(|e| {
                e.get("message")
                    .or_else(|| e.get("error"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_else(|| "unknown error".to_string())
    })
}

/// Obsidian — write the note into the vault directory (desktop-local file
/// write; the vault path is chosen in Settings and validated on save).
async fn obsidian(
    config: &IntegrationConfig,
    use_ai_summary: bool,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let vault = require(&config.vault_path, "Obsidian vault path")?;
    let base = obsidian_note_stem(&ctx.title, ctx.published_at.as_deref().unwrap_or(""));
    let path = std::path::Path::new(vault).join(format!("{base}.md"));
    let content = obsidian_note(ctx, use_ai_summary);
    tauri::async_runtime::spawn_blocking(move || {
        // Atomic create: `create_new` refuses an existing file outright, so
        // two processes saving the same-named note concurrently can never
        // overwrite each other (an `exists()` pre-check would race).
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, content.as_bytes()))
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => AppError::code("obsidianNoteExists"),
                _ => AppError::other(format!("could not write the note: {e}")),
            })
    })
    .await
    .map_err(|e| AppError::other(format!("note task: {e}")))??;
    Ok(None)
}

/// Note file stem: keep letters (including CJK), digits and a few safe
/// punctuation marks, replace the rest with `_`, cap at 80 chars. A stem
/// without any letter or digit (symbol-only title) falls back to the
/// published date so the note name stays meaningful.
fn obsidian_note_stem(title: &str, fallback: &str) -> String {
    let mapped = |s: &str| {
        let replaced: String = s
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        truncate_chars(replaced.trim(), 80)
    };
    let title_stem = non_empty(title).map(|t| mapped(&t));
    let fallback_stem = non_empty(fallback).map(|f| mapped(&f));
    title_stem
        .filter(|s| s.chars().any(|c| c.is_alphanumeric()))
        .or_else(|| fallback_stem.filter(|s| s.chars().any(|c| c.is_alphanumeric())))
        .unwrap_or_else(|| "note".to_string())
}

/// YAML frontmatter + Markdown body, mirroring the note layout the built-in
/// integrations write: metadata first, then the article as a heading and body.
fn obsidian_note(ctx: &ArticleContext, use_ai_summary: bool) -> String {
    let mut fm = String::from("---\n");
    fm.push_str(&format!("url: {}\n", ctx.url));
    if !ctx.author.is_empty() {
        fm.push_str(&format!("author: {}\n", json_quote(&ctx.author)));
    }
    if let Some(published) = ctx.published_at.as_deref().and_then(naive_iso) {
        fm.push_str(&format!("publishedAt: {published}\n"));
    }
    let description = ctx.description_with(use_ai_summary);
    if !description.is_empty() {
        fm.push_str(&yaml_block("description", &description));
    }
    fm.push_str("tags:\n  - papr\n");
    if !ctx.feed_title.is_empty() {
        fm.push_str(&format!("feedTitle: {}\n", json_quote(&ctx.feed_title)));
    }
    if let Some(feed_url) = ctx.feed_url.as_deref().filter(|u| !u.is_empty()) {
        fm.push_str(&format!("feedUrl: {feed_url}\n"));
    }
    fm.push_str("---\n");
    format!("{fm}\n# {}\n\n{}", ctx.title, ctx.markdown)
}

/// A JSON-quoted scalar — YAML accepts JSON string syntax, so this is both a
/// valid YAML quoted string and escapes any embedded quote or newline.
fn json_quote(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// A multi-line scalar as a YAML `|-` block, each line indented two spaces.
fn yaml_block(key: &str, value: &str) -> String {
    let mut out = format!("{key}: |-\n");
    for line in value.lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out
}

// ─────────────────────────── custom integrations ───────────────────────────

/// The body-carrying methods a webhook may choose.
const WEBHOOK_METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// URL schemes a template may never open, whatever the user configured —
/// each can execute content or reach local resources by design.
const BLOCKED_SCHEMES: [&str; 10] = [
    "data",
    "file",
    "blob",
    "javascript",
    "vbscript",
    "jar",
    "view-source",
    "ms-msdt",
    "search-ms",
    "smb",
];

/// Validate a substituted scheme URL: a real scheme, a registered-looking
/// name, and not one of the blocklisted families.
fn validate_scheme_url(url: &str) -> AppResult<()> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(AppError::code("integrationSchemeInvalid"));
    };
    let scheme_ok = !scheme.is_empty()
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !scheme_ok || rest.is_empty() || BLOCKED_SCHEMES.contains(&scheme.to_lowercase().as_str()) {
        return Err(AppError::code("integrationSchemeInvalid"));
    }
    Ok(())
}

async fn webhook(
    client: &Client,
    config: &IntegrationConfig,
    use_ai_summary: bool,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let raw_method = require(&config.method, "webhook method")?.to_ascii_uppercase();
    if !WEBHOOK_METHODS.contains(&raw_method.as_str()) {
        return Err(AppError::code("integrationMethodInvalid"));
    }
    let method = Method::from_bytes(raw_method.as_bytes())
        .map_err(|_| AppError::code("integrationMethodInvalid"))?;
    let raw_url = require(&config.url, "webhook URL")?;

    let headers = config.headers.clone().unwrap_or_default();
    let summary = ctx.description_with(use_ai_summary);
    let tmpl_ctx = PlaceholderContext {
        title: ctx.title.as_str(),
        url: ctx.url.as_str(),
        content_html: ctx.html.as_str(),
        content_markdown: ctx.markdown.as_str(),
        summary: summary.as_str(),
        description: ctx.description.as_str(),
        author: ctx.author.as_str(),
        published_at: ctx.published_at.as_deref().unwrap_or(""),
    };
    let url = replace_placeholders(raw_url, &tmpl_ctx, TokenMode::Url);
    let resolved_headers =
        effective_headers(&raw_method, &headers, &tmpl_ctx, config.body.is_some());
    let json_body = resolved_headers
        .iter()
        .any(|(k, v)| k == "content-type" && v.to_lowercase().contains("application/json"));

    let body = config
        .body
        .as_deref()
        .filter(|b| !b.is_empty())
        .map(|b| replace_placeholders(b, &tmpl_ctx, TokenMode::Json { json_body }));

    let mut req = client.request(method, &url);
    for (name, value) in &resolved_headers {
        req = match (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            (Ok(n), Ok(v)) => req.header(n, v),
            // Same policy as `header` above: report the header, not its value.
            _ => {
                return Err(AppError::other(format!(
                    "invalid header name or value: {name}"
                )))
            }
        };
    }
    match body {
        Some(b) => finish(req.body(b)).await?,
        None => finish(req).await?,
    };
    Ok(None)
}

/// Open a `scheme://…` integration through the OS handler.
fn url_scheme(
    app: &AppHandle,
    config: &IntegrationConfig,
    use_ai_summary: bool,
    ctx: &ArticleContext,
) -> AppResult<Option<String>> {
    let raw = require(&config.scheme, "URL scheme")?;
    let summary = ctx.description_with(use_ai_summary);
    let ctx = PlaceholderContext {
        title: ctx.title.as_str(),
        url: ctx.url.as_str(),
        content_html: ctx.html.as_str(),
        content_markdown: ctx.markdown.as_str(),
        summary: summary.as_str(),
        description: ctx.description.as_str(),
        author: ctx.author.as_str(),
        published_at: ctx.published_at.as_deref().unwrap_or(""),
    };
    let url = replace_placeholders(raw, &ctx, TokenMode::Url);
    validate_scheme_url(&url)?;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| AppError::other(format!("could not open the URL: {e}")))?;
    Ok(None)
}

// ─────────────────────────── tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PlaceholderContext<'static> {
        PlaceholderContext {
            title: "Hello \"world\"",
            url: "https://example.com/a b?x=1",
            content_html: "<p>hi</p>",
            content_markdown: "hi",
            summary: "sum",
            description: "desc",
            author: "Ada",
            published_at: "2026-01-02T03:04:05Z",
        }
    }

    #[test]
    fn placeholders_url_mode_encodes() {
        let out = replace_placeholders(
            "https://hook.test/?u=[url]&t=[title]",
            &ctx(),
            TokenMode::Url,
        );
        assert_eq!(
            out,
            "https://hook.test/?u=https%3A%2F%2Fexample.com%2Fa%20b%3Fx%3D1&t=Hello%20%22world%22"
        );
    }

    #[test]
    fn placeholders_raw_mode_keeps_values() {
        let out = replace_placeholders("[author] — [summary]", &ctx(), TokenMode::Raw);
        assert_eq!(out, "Ada — sum");
    }

    #[test]
    fn placeholders_json_mode_escapes_body_values() {
        let out = replace_placeholders(
            "{\"title\":\"[title]\"}",
            &ctx(),
            TokenMode::Json { json_body: true },
        );
        assert_eq!(out, "{\"title\":\"Hello \\\"world\\\"\"}");
    }

    #[test]
    fn placeholders_json_mode_verbatim_without_json_content_type() {
        let out = replace_placeholders(
            "title=[title]",
            &ctx(),
            TokenMode::Json { json_body: false },
        );
        assert_eq!(out, "title=Hello \"world\"");
    }

    #[test]
    fn placeholders_not_reexpanded_from_values() {
        // A title that itself contains a literal token must survive verbatim
        // — the single-pass scan never rescans substituted content.
        let title = String::from("Read [summary] and [url] now");
        let c = PlaceholderContext {
            title: &title,
            ..ctx()
        };
        assert_eq!(replace_placeholders("[title]", &c, TokenMode::Raw), title);
        // Inside a JSON body the inserted brackets must not open another
        // field's slot either.
        let out = replace_placeholders(
            "{\"t\":\"[title]\"}",
            &c,
            TokenMode::Json { json_body: true },
        );
        assert_eq!(out, "{\"t\":\"Read [summary] and [url] now\"}");
    }

    #[test]
    fn webhook_headers_normalized_and_defaulted() {
        let mut headers = std::collections::HashMap::new();
        headers.insert(" X-Auth ".to_string(), " v-[author] ".to_string());
        let out = effective_headers("POST", &headers, &ctx(), true);
        assert!(out.contains(&("x-auth".to_string(), " v-Ada ".to_string())));
        assert!(out.contains(&("content-type".to_string(), "application/json".to_string())));
    }

    #[test]
    fn webhook_headers_declared_content_type_wins() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("Content-Type".to_string(), "text/plain".to_string());
        let out = effective_headers("POST", &headers, &ctx(), true);
        assert!(out.contains(&("content-type".to_string(), "text/plain".to_string())));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn webhook_headers_get_without_body_has_no_content_type() {
        let out = effective_headers("GET", &std::collections::HashMap::new(), &ctx(), true);
        assert!(!out.iter().any(|(k, _)| k == "content-type"));
    }

    #[test]
    fn uuid_detection() {
        assert!(is_uuid_like("a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d"));
        assert!(!is_uuid_like("knowledge-base"));
        assert!(!is_uuid_like("a1b2c3d4e5f6-4a7b-8c9d-0e1f2a3b4c5d"));
        assert!(!is_uuid_like("a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5"));
    }

    #[test]
    fn scheme_validation() {
        assert!(validate_scheme_url("obsidian://new?vault=V").is_ok());
        assert!(validate_scheme_url("bear://x-callback-url/create").is_ok());
        assert!(validate_scheme_url("no-scheme").is_err());
        assert!(validate_scheme_url("javascript://alert(1)").is_err());
        assert!(validate_scheme_url("file:///etc/passwd").is_err());
        assert!(validate_scheme_url("data://text/html,x").is_err());
        assert!(validate_scheme_url("1bad://x").is_err());
        assert!(validate_scheme_url("obsidian://").is_err());
    }

    #[test]
    fn obsidian_stem_sanitized_and_capped() {
        assert_eq!(
            obsidian_note_stem("Hello/World: *bad*", ""),
            "Hello_World_ _bad_"
        );
        let long = obsidian_note_stem(&"x".repeat(200), "");
        assert_eq!(long.chars().count(), 80);
        assert_eq!(
            obsidian_note_stem("  ", "2026-01-02T03:04:05Z"),
            "2026-01-02T03_04_05Z"
        );
        // A symbol-only title falls back to the published date.
        assert_eq!(
            obsidian_note_stem("///", "2026-01-02T03:04:05Z"),
            "2026-01-02T03_04_05Z"
        );
        assert_eq!(obsidian_note_stem("///", ""), "note");
    }

    #[test]
    fn derive_body_extracts_markdown_and_description() {
        let (md, desc) = derive_body("<h1>Head</h1><p>body <b>text</b></p>");
        assert!(md.contains("# Head"));
        assert!(md.contains("**text**") || md.contains("__text__"));
        assert_eq!(desc, "Head body text");
        assert_eq!(derive_body("").0, "");
    }

    #[test]
    fn obsidian_note_frontmatter() {
        let ctx = ArticleContext {
            title: "A note".into(),
            author: "Ada".into(),
            url: "https://example.com/a".into(),
            html: "<p>body text</p>".into(),
            markdown: "body text".into(),
            description: "body text".into(),
            ai_summary: Some("AI summary line".into()),
            published_at: Some("2026-01-02T03:04:05.123Z".into()),
            feed_title: "My Feed \"Q\"".into(),
            feed_url: Some("https://example.com/feed.xml".into()),
        };
        let note = obsidian_note(&ctx, true);
        assert!(note.starts_with("---\nurl: https://example.com/a\n"));
        assert!(note.contains("author: \"Ada\"\n"));
        assert!(note.contains("publishedAt: 2026-01-02T03:04:05Z\n"));
        // AI summary wins as the description when the preference is on.
        assert!(note.contains("description: |-\n  AI summary line\n"));
        assert!(note.contains("tags:\n  - papr\n"));
        assert!(note.contains("feedTitle: \"My Feed \\\"Q\\\"\"\n"));
        assert!(note.contains("feedUrl: https://example.com/feed.xml\n"));
        assert!(note.contains("\n# A note\n\n"));
        // Without the preference the body-derived description is used.
        let note2 = obsidian_note(&ctx, false);
        assert!(note2.contains("description: |-\n  body text\n"));
    }

    #[test]
    fn naive_iso_strips_fraction() {
        assert_eq!(
            naive_iso("2026-01-02T03:04:05.123Z").as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
        assert_eq!(naive_iso("not a date"), None);
    }

    #[test]
    fn response_snippet_one_line_and_capped() {
        let s = response_snippet(&format!("{}err", "x".repeat(300)));
        assert_eq!(s.chars().count(), 200);
        assert_eq!(response_snippet("line1\nline2\ttab"), "line1line2tab");
    }
}
