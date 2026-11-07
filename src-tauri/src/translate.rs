//! Full-text article translation. Splits an article body into batches of whole
//! top-level blocks, each translated by the cloud LLM (`ai::stream_chat`) with
//! the HTML structure preserved, then reassembled. The pure helpers here —
//! block chunking, prompt building, code-fence stripping — carry the logic worth
//! testing; the streaming/persistence glue lives in `commands::ai_translate`.

use crate::ai;
use crate::error::{AppError, AppResult};
use ego_tree::NodeId;
use reqwest::Client;
use scraper::node::Node;
use scraper::{ElementRef, Html};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Generic wrapper tags that carry no readable text of their own. A body that is
/// a single such container (the common "everything inside one `<div>`" feed
/// shape) is unwrapped so its children can be batched rather than translated as
/// one oversized block. Unwrapping is repeated for nested wrappers.
const UNWRAP_TAGS: &[&str] = &["div", "article", "section", "main"];

/// The human-readable name for a UI/target language code, as used in the
/// translation instruction. Anything unrecognised falls back to English.
pub fn language_name(code: &str) -> &'static str {
    match code {
        "zh" => "Simplified Chinese",
        "ja" => "Japanese",
        _ => "English",
    }
}

/// Build the system prompt instructing the model to translate one batch of HTML
/// into `target` while leaving the markup intact.
pub fn translate_system_prompt(target: &str) -> String {
    format!(
        "You are a professional translator. Translate the text content of the \
         HTML fragment into {target}.\n\n\
         Rules:\n\
         - Preserve every HTML tag, attribute, and the overall structure exactly.\n\
         - Translate only human-readable text; do not translate code or URLs.\n\
         - Keep images, links, and all other markup intact.\n\
         - Output only the translated HTML fragment: no preamble, no code fences."
    )
}

/// Split source body HTML into batches of whole top-level blocks, each at most
/// `budget` bytes where possible. Whole elements are never split across batches;
/// a single block larger than `budget` becomes its own batch. A body that is a
/// single generic wrapper (`div`/`article`/`section`/`main`, possibly nested) is
/// unwrapped so its children are batched. Concatenating the returned batches
/// reproduces the source's block sequence (minus any unwrapped wrappers).
pub fn chunk_blocks(html: &str, budget: usize) -> Vec<String> {
    let frag = Html::parse_fragment(html);
    let mut nodes: Vec<_> = frag.root_element().children().collect();

    // Unwrap nested single generic containers so their children can be batched.
    loop {
        let elems: Vec<_> = nodes
            .iter()
            .filter(|n| n.value().as_element().is_some())
            .copied()
            .collect();
        match elems.as_slice() {
            [only] => match only.value().as_element() {
                Some(el) if UNWRAP_TAGS.contains(&el.name()) => {
                    nodes = only.children().collect();
                }
                _ => break,
            },
            _ => break,
        }
    }

    // Serialize each top-level node, dropping whitespace-only text between blocks.
    let mut pieces: Vec<String> = Vec::new();
    for node in nodes {
        match node.value() {
            Node::Element(_) => {
                if let Some(el) = ElementRef::wrap(node) {
                    pieces.push(el.html());
                }
            }
            Node::Text(t) => {
                let text: &str = t;
                if !text.trim().is_empty() {
                    pieces.push(text.to_string());
                }
            }
            _ => {}
        }
    }

    // Greedily group whole pieces under the byte budget. A piece larger than the
    // budget sits alone rather than being split.
    let mut batches: Vec<String> = Vec::new();
    let mut cur = String::new();
    for piece in pieces {
        if !cur.is_empty() && cur.len() + piece.len() > budget {
            batches.push(std::mem::take(&mut cur));
        }
        cur.push_str(&piece);
    }
    if !cur.is_empty() {
        batches.push(cur);
    }
    batches
}

/// Strip a surrounding ```/```html markdown code fence a model may wrap its HTML
/// output in, returning the inner content trimmed. Unfenced input is returned
/// trimmed but otherwise unchanged.
pub fn strip_code_fence(s: &str) -> String {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        // Drop the opening fence's optional language tag line (`html`, ``, …).
        let inner = match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        return inner.trim().to_string();
    }
    trimmed.to_string()
}

// ─────────────────────────── translation engines ───────────────────────────
//
// papr translates a full article body, batch by batch, with the HTML structure
// preserved (see `commands::ai_translate`). The original engine is a cloud LLM
// that is handed an HTML fragment and returns the translated fragment. To let a
// user pick a non-LLM engine instead — Google (free), DeepL, Bing — without
// losing that structure, the traditional machine-translation engines take a
// different path: the fragment's text nodes are extracted, translated as plain
// strings, and written back into the same DOM. Every tag, attribute, link and
// image is therefore preserved exactly, and the reassembled fragment flows back
// through the same sanitizer / DB-cache / streaming path as the LLM output.

/// Per-request timeout for a machine-translation call. Each call carries at most
/// one batch (a few KB of text), so a generous but bounded timeout is plenty —
/// unlike the LLM path it does not stream for minutes.
const MT_TIMEOUT: Duration = Duration::from_secs(30);

/// The per-batch input budget (bytes of source HTML) for `chunk_blocks`,
/// deciding when a long article is split. A short article falls under the budget
/// and translates in a single request — only longer ones are chunked — and the
/// size is tuned to each engine's limits: the LLM is bounded by its per-batch
/// output-token cap; Google, DeepL and Bing all take large POST bodies, so a
/// bigger batch means fewer round-trips on a long article.
pub fn chunk_budget(engine: &str) -> usize {
    match engine {
        "google" | "deepl" | "bing" => 8000,
        _ => ai::TRANSLATE_CHUNK_BUDGET,
    }
}

/// Text nodes inside these elements are code/markup, not prose, so they are left
/// untranslated (the LLM prompt makes the same exclusion for its path).
const SKIP_TEXT_TAGS: &[&str] = &["script", "style", "code", "pre", "kbd", "samp"];

/// The chosen engine plus the credentials/handles it needs at request time. Built
/// by [`ready`] from a [`Selection`]; every machine-translation engine here is
/// keyless, so no variant carries credentials.
pub enum Backend {
    /// Cloud LLM — handed HTML, returns HTML (the original path).
    Llm(ai::AiConfig),
    /// Google's free endpoint (keyless).
    Google,
    /// DeepL's free web endpoint (keyless, the same one the web client uses).
    Deepl,
    /// Bing/Edge translator (keyless).
    Bing,
}

/// The engine choice resolved from settings, before any network work.
pub enum Selection {
    Llm(ai::AiConfig),
    Google,
    Deepl,
    Bing,
}

/// Turn a [`Selection`] into a request-ready [`Backend`]. No one-off network
/// setup is needed — every engine is keyless — so this is a plain conversion.
pub fn ready(sel: Selection) -> AppResult<Backend> {
    Ok(match sel {
        Selection::Llm(cfg) => Backend::Llm(cfg),
        Selection::Google => Backend::Google,
        Selection::Deepl => Backend::Deepl,
        Selection::Bing => Backend::Bing,
    })
}

impl Backend {
    /// Translate one batch of body HTML into `target`, returning the translated
    /// HTML fragment (unsanitized — the caller sanitizes uniformly). `system` is
    /// the LLM instruction prompt, ignored by the machine-translation engines.
    pub async fn translate_batch(
        &self,
        client: &Client,
        system: &str,
        batch: &str,
        target: &str,
    ) -> AppResult<String> {
        match self {
            Backend::Llm(cfg) => {
                let text =
                    ai::complete_chat(client, cfg, system, batch, ai::TRANSLATE_MAX_TOKENS).await?;
                Ok(strip_code_fence(&text))
            }
            _ => translate_fragment(client, self, batch, target).await,
        }
    }
}

/// One translatable text node: its tree id plus the surrounding whitespace to
/// restore after translation. The whitespace is preserved separately because
/// machine-translation engines routinely trim it, which would weld inline words
/// together (`Hello <b>world</b>` → `Helloworld`).
struct TextSlot {
    id: NodeId,
    prefix: String,
    suffix: String,
}

/// Walk an HTML fragment and collect every translatable text node in document
/// order: the trimmed core strings (to send to the engine) and a [`TextSlot`]
/// each (to write the result back). Whitespace-only nodes and text inside
/// code/markup elements are skipped.
fn collect_text(doc: &Html) -> (Vec<TextSlot>, Vec<String>) {
    let mut slots = Vec::new();
    let mut cores = Vec::new();
    for node in doc.tree.nodes() {
        let Node::Text(t) = node.value() else { continue };
        let s: &str = t;
        if s.trim().is_empty() {
            continue;
        }
        let in_code = node.ancestors().any(|a| {
            a.value()
                .as_element()
                .is_some_and(|e| SKIP_TEXT_TAGS.contains(&e.name()))
        });
        if in_code {
            continue;
        }
        // Whitespace is ASCII, so these byte slices never split a UTF-8 char.
        let prefix = s[..s.len() - s.trim_start().len()].to_string();
        let suffix = s[s.trim_end().len()..].to_string();
        slots.push(TextSlot { id: node.id(), prefix, suffix });
        cores.push(s.trim().to_string());
    }
    (slots, cores)
}

/// Write translated strings back into their text nodes, restoring each node's
/// original surrounding whitespace. Extra or missing translations (an engine
/// that returned the wrong count) are ignored rather than misaligned: `zip`
/// stops at the shorter side, leaving any unmatched node as its source text.
fn apply(doc: &mut Html, slots: &[TextSlot], translated: &[String]) {
    for (slot, text) in slots.iter().zip(translated) {
        if let Some(mut node) = doc.tree.get_mut(slot.id) {
            if let Node::Text(t) = node.value() {
                t.text = format!("{}{}{}", slot.prefix, text, slot.suffix).into();
            }
        }
    }
}

/// Serialize a parsed fragment's top-level nodes back to an HTML string,
/// mirroring how `chunk_blocks` reads the fragment (the parse wrapper is
/// dropped).
fn serialize_fragment(doc: &Html) -> String {
    let mut out = String::new();
    for child in doc.root_element().children() {
        match child.value() {
            Node::Element(_) => {
                if let Some(el) = ElementRef::wrap(child) {
                    out.push_str(&el.html());
                }
            }
            Node::Text(t) => out.push_str(t),
            _ => {}
        }
    }
    out
}

/// Re-parse a fragment and write `translated` back into its text nodes (in the
/// same document order `collect_text` produced them), returning the serialized
/// result. Parsing happens twice — once to extract, once here to rewrite —
/// because `scraper::Html` is not `Send` and so cannot be held across the
/// network `await` between the two steps (Tauri commands require `Send` futures).
fn rewrite_fragment(html: &str, translated: &[String]) -> String {
    let mut doc = Html::parse_fragment(html);
    let (slots, _cores) = collect_text(&doc);
    apply(&mut doc, &slots, translated);
    serialize_fragment(&doc)
}

/// Translate one HTML batch with a machine-translation engine, preserving markup
/// by translating only its text nodes. A batch with no translatable text (e.g. a
/// lone image) is returned unchanged. The parsed DOM is fully dropped before the
/// network call so the returned future stays `Send`.
async fn translate_fragment(
    client: &Client,
    backend: &Backend,
    html: &str,
    target: &str,
) -> AppResult<String> {
    let cores = {
        let doc = Html::parse_fragment(html);
        let (_slots, cores) = collect_text(&doc);
        cores
    };
    if cores.is_empty() {
        return Ok(html.to_string());
    }
    let translated = translate_segments(client, backend, &cores, target).await?;
    Ok(rewrite_fragment(html, &translated))
}

/// Dispatch a list of plain-text segments to the selected engine. The returned
/// vector is positionally aligned with `cores`: all three machine-translation
/// engines send the segments as an ordered array and error when the response
/// count does not match the request, so a success always carries every segment.
async fn translate_segments(
    client: &Client,
    backend: &Backend,
    cores: &[String],
    target: &str,
) -> AppResult<Vec<String>> {
    match backend {
        Backend::Google => google_segments(client, cores, target).await,
        Backend::Deepl => deepl_segments(client, cores, target).await,
        Backend::Bing => bing_segments(client, cores, target).await,
        Backend::Llm(_) => unreachable!("LLM batches are translated as whole HTML, not segments"),
    }
}

// ── per-engine target language codes ─────────────────────────────────────────
// papr stores the target as `zh` / `ja` / `en`; each engine names them its own way.

fn google_code(target: &str) -> &'static str {
    match target {
        "zh" => "zh-CN",
        "ja" => "ja",
        _ => "en",
    }
}

fn deepl_code(target: &str) -> &'static str {
    match target {
        "zh" => "ZH",
        "ja" => "JA",
        _ => "EN",
    }
}

fn bing_code(target: &str) -> &'static str {
    match target {
        "zh" => "zh-Hans",
        "ja" => "ja",
        _ => "en",
    }
}

/// Map a non-success HTTP response to an `AppError` naming the engine and
/// carrying the response body.
async fn http_error(resp: reqwest::Response, engine: &str) -> AppError {
    let status = resp.status();
    let detail = resp.text().await.unwrap_or_default();
    AppError::other(format!("{engine} translate error {status}: {detail}"))
}

// ── Google (free, keyless) ───────────────────────────────────────────────────

/// Max text segments per Google request. Each segment rides as its own repeated
/// `q=` form field and comes back positionally aligned, so batches stay small
/// and the alignment is exact.
const GOOGLE_MAX_SEGMENTS: usize = 20;
/// Byte cap on the text carried by one Google request (the sum of the `q=`
/// values, not counting encoding overhead) — mirrors the web client's
/// conservative per-request ceiling so an oversized body is never rejected.
const GOOGLE_MAX_REQUEST_BYTES: usize = 1800;

/// Translate segments with Google's `clients5` dict endpoint (`client=
/// dict-chrome-ex`, the same one Chrome's built-in translate uses). Each
/// segment is sent as a repeated `q=` form field and answered positionally, so
/// no newline-join reconstruction — and none of its per-segment request
/// fallback, which multiplied request volume exactly when the engine was
/// rate-limiting — is needed.
async fn google_segments(
    client: &Client,
    cores: &[String],
    target: &str,
) -> AppResult<Vec<String>> {
    let mut out = Vec::with_capacity(cores.len());
    let mut batch: Vec<&str> = Vec::with_capacity(GOOGLE_MAX_SEGMENTS);
    let mut batch_bytes = 0usize;
    for core in cores {
        // Flush when the next segment would push the request past the segment
        // or byte ceiling; a batch is never empty at flush time, so every
        // segment lands in exactly one request.
        if !batch.is_empty()
            && (batch.len() >= GOOGLE_MAX_SEGMENTS
                || batch_bytes + core.len() > GOOGLE_MAX_REQUEST_BYTES)
        {
            out.extend(google_request(client, &batch, target).await?);
            batch.clear();
            batch_bytes = 0;
        }
        batch_bytes += core.len();
        batch.push(core);
    }
    if !batch.is_empty() {
        out.extend(google_request(client, &batch, target).await?);
    }
    Ok(out)
}

/// One `translate_a/t` request for `batch`, returning its positionally aligned
/// translations.
async fn google_request(
    client: &Client,
    batch: &[&str],
    target: &str,
) -> AppResult<Vec<String>> {
    let form: Vec<(&str, &str)> = batch.iter().map(|c| ("q", *c)).collect();
    let resp = client
        .post("https://clients5.google.com/translate_a/t")
        .query(&[
            ("client", "dict-chrome-ex"),
            ("sl", "auto"),
            ("tl", google_code(target)),
        ])
        .form(&form)
        .timeout(MT_TIMEOUT)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(http_error(resp, "Google").await);
    }
    let v: Value = resp.json().await.map_err(|e| {
        AppError::other(format!("Google translate error: decoding response failed ({e})"))
    })?;
    let parsed = parse_google_segments(&v)?;
    // The endpoint answers one entry per `q`; a shorter or longer response
    // would silently misalign every following text node.
    if parsed.len() != batch.len() {
        return Err(AppError::other("Google translate error: segment count mismatch"));
    }
    Ok(parsed)
}

/// Parse one `translate_a/t` response body into positionally aligned
/// translations. With `sl=auto` each entry is a `[text, detectedLang]` pair;
/// with an explicit source language it is a bare string — accept both.
fn parse_google_segments(v: &Value) -> AppResult<Vec<String>> {
    let arr = v
        .as_array()
        .ok_or_else(|| AppError::other("Google translate error: malformed response"))?;
    arr.iter()
        .map(|item| match item {
            Value::String(s) => Ok(s.clone()),
            Value::Array(pair) => pair
                .first()
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| AppError::other("Google translate error: malformed response")),
            _ => Err(AppError::other("Google translate error: malformed response")),
        })
        .collect()
}

// ── DeepL (free web endpoint, keyless) ───────────────────────────────────────
//
// This is the unofficial JSON-RPC endpoint the DeepL web client itself calls —
// no API key required. It mimics the web client's anti-abuse quirks: a random
// request id, a timestamp derived from the count of the letter `i` in the text,
// and a one-space/no-space tweak to the serialized `method` field keyed off the
// id. The `texts` array returns translations positionally, so no reconstruction
// is needed; requests are chunked to stay well within the endpoint's limits.

/// Milliseconds since the Unix epoch — used for DeepL's request timestamp / id.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// DeepL's timestamp must be a multiple of (1 + number of `i`s across the texts),
/// offset by that amount — the web client computes it this way and the endpoint
/// validates it.
fn deepl_timestamp(i_count: u64) -> u64 {
    let ts = now_millis();
    if i_count == 0 {
        return ts;
    }
    let n = i_count + 1;
    ts - (ts % n) + n
}

async fn deepl_segments(
    client: &Client,
    cores: &[String],
    target: &str,
) -> AppResult<Vec<String>> {
    let mut out = Vec::with_capacity(cores.len());
    for chunk in cores.chunks(50) {
        out.extend(deepl_free_call(client, chunk, target).await?);
    }
    Ok(out)
}

/// One JSON-RPC `LMT_handle_texts` call against DeepL's free web endpoint.
async fn deepl_free_call(
    client: &Client,
    cores: &[String],
    target: &str,
) -> AppResult<Vec<String>> {
    // A pseudo-random request id in the same range the web client uses; the
    // sub-millisecond clock is enough entropy here (no `rand` dependency).
    let id = (now_millis() % 99_999 + 100_000) * 1000;
    let i_count = cores.iter().map(|t| t.matches('i').count() as u64).sum();
    let texts: Vec<Value> = cores
        .iter()
        .map(|t| json!({ "text": t, "requestAlternatives": 0 }))
        .collect();
    let body = json!({
        "jsonrpc": "2.0",
        "method": "LMT_handle_texts",
        "params": {
            "splitting": "newlines",
            "lang": { "source_lang_user_selected": "auto", "target_lang": deepl_code(target) },
            "texts": texts,
            "timestamp": deepl_timestamp(i_count),
        },
        "id": id,
    });
    // The web client varies the spacing around the `method` value based on the
    // id; the endpoint expects that exact shape.
    let mut body_str = serde_json::to_string(&body)
        .map_err(|e| AppError::other(format!("DeepL request encode error: {e}")))?;
    if (id + 5).is_multiple_of(29) || (id + 3).is_multiple_of(13) {
        body_str = body_str.replace("\"method\":\"", "\"method\" : \"");
    } else {
        body_str = body_str.replace("\"method\":\"", "\"method\": \"");
    }
    let resp = client
        .post("https://www2.deepl.com/jsonrpc")
        .header("Content-Type", "application/json")
        .body(body_str)
        .timeout(MT_TIMEOUT)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(http_error(resp, "DeepL").await);
    }
    let v: Value = resp.json().await.map_err(|e| {
        AppError::other(format!("DeepL translate error: decoding response failed ({e})"))
    })?;
    let arr = v
        .pointer("/result/texts")
        .and_then(|x| x.as_array())
        .ok_or_else(|| AppError::other("DeepL translate error: malformed response"))?;
    Ok(arr
        .iter()
        .map(|t| t.get("text").and_then(|x| x.as_str()).unwrap_or_default().to_string())
        .collect())
}

// ── Bing / Edge translator (keyless) ─────────────────────────────────────────

/// Max text segments per Bing request, well under its per-call array limit.
const BING_MAX_SEGMENTS: usize = 100;

/// Translate segments with the Edge translator's keyless `translatetext`
/// endpoint. The Bearer-token flow this replaces — fetching a token from
/// `edge.microsoft.com/translate/auth` — is gone: that endpoint now answers 404.
/// The body is an array of plain strings translated positionally. The endpoint
/// rejects UA-less clients with 400 ("Client Browser Version not supported"),
/// so it depends on the shared client's desktop `User-Agent` — don't strip it.
async fn bing_segments(
    client: &Client,
    cores: &[String],
    target: &str,
) -> AppResult<Vec<String>> {
    let mut out = Vec::with_capacity(cores.len());
    for chunk in cores.chunks(BING_MAX_SEGMENTS) {
        let resp = client
            .post("https://edge.microsoft.com/translate/translatetext")
            .query(&[
                ("from", ""),
                ("to", bing_code(target)),
                ("isEnterpriseClient", "false"),
            ])
            .json(chunk)
            .timeout(MT_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(http_error(resp, "Bing").await);
        }
        let v: Value = resp.json().await.map_err(|e| {
            AppError::other(format!("Bing translate error: decoding response failed ({e})"))
        })?;
        out.extend(parse_bing_segments(&v, chunk.len())?);
    }
    Ok(out)
}

/// Parse one `translatetext` response body into positionally aligned
/// translations. Each entry is `{ translations: [{ text }] }`; the entry count
/// must match the request's segment count, and every entry must carry a
/// translation — a missing one would silently wipe that text node.
fn parse_bing_segments(v: &Value, expected: usize) -> AppResult<Vec<String>> {
    let arr = v
        .as_array()
        .ok_or_else(|| AppError::other("Bing translate error: malformed response"))?;
    if arr.len() != expected {
        return Err(AppError::other("Bing translate error: segment count mismatch"));
    }
    arr.iter()
        .map(|item| {
            item.get("translations")
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .and_then(|t| t.get("text"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| AppError::other("Bing translate error: malformed response"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── chunk_blocks ────────────────────────────────────────────────────

    #[test]
    fn groups_multiple_blocks_into_one_batch_under_budget() {
        let out = chunk_blocks("<p>a</p><p>b</p><p>c</p>", 1000);
        assert_eq!(out, vec!["<p>a</p><p>b</p><p>c</p>"]);
    }

    #[test]
    fn splits_into_batches_when_over_budget() {
        // Each `<p>x</p>` is 8 bytes; a budget of 8 forces one block per batch.
        let out = chunk_blocks("<p>a</p><p>b</p><p>c</p>", 8);
        assert_eq!(out, vec!["<p>a</p>", "<p>b</p>", "<p>c</p>"]);
        // Reassembly reproduces the source block sequence.
        assert_eq!(out.concat(), "<p>a</p><p>b</p><p>c</p>");
    }

    #[test]
    fn never_splits_a_single_oversized_block() {
        let big = format!("<p>{}</p>", "x".repeat(100));
        let out = chunk_blocks(&big, 8);
        assert_eq!(out, vec![big]);
    }

    #[test]
    fn unwraps_a_single_generic_container() {
        // The wrapping <div> is dropped so its children can be batched.
        let out = chunk_blocks("<div><p>a</p><p>b</p></div>", 8);
        assert_eq!(out, vec!["<p>a</p>", "<p>b</p>"]);
    }

    #[test]
    fn unwraps_nested_generic_containers() {
        let out = chunk_blocks("<div><section><p>a</p><p>b</p></section></div>", 8);
        assert_eq!(out, vec!["<p>a</p>", "<p>b</p>"]);
    }

    #[test]
    fn does_not_unwrap_structural_containers() {
        // A <ul> carries list structure; its <li> children must stay wrapped.
        let out = chunk_blocks("<ul><li>a</li><li>b</li></ul>", 1000);
        assert_eq!(out, vec!["<ul><li>a</li><li>b</li></ul>"]);
    }

    #[test]
    fn keeps_images_inside_their_block() {
        let html = r#"<p>intro</p><figure><img src="https://e.com/a.png"></figure>"#;
        let out = chunk_blocks(html, 8);
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("https://e.com/a.png"));
    }

    #[test]
    fn empty_or_whitespace_input_yields_no_batches() {
        assert!(chunk_blocks("", 1000).is_empty());
        assert!(chunk_blocks("   \n  ", 1000).is_empty());
    }

    #[test]
    fn bare_text_is_a_single_batch() {
        assert_eq!(chunk_blocks("just text", 1000), vec!["just text"]);
    }

    // ── translate_system_prompt ─────────────────────────────────────────

    #[test]
    fn prompt_names_the_target_language() {
        let p = translate_system_prompt("Simplified Chinese");
        assert!(p.contains("Simplified Chinese"), "missing language: {p}");
    }

    #[test]
    fn prompt_demands_html_be_preserved() {
        let p = translate_system_prompt("Japanese");
        let lower = p.to_lowercase();
        assert!(lower.contains("html"), "no HTML mention: {p}");
        assert!(lower.contains("preserve") || lower.contains("keep"), "no preserve directive: {p}");
    }

    // ── strip_code_fence ────────────────────────────────────────────────

    #[test]
    fn strips_language_tagged_fence() {
        assert_eq!(strip_code_fence("```html\n<p>x</p>\n```"), "<p>x</p>");
    }

    #[test]
    fn strips_bare_fence() {
        assert_eq!(strip_code_fence("```\n<p>x</p>\n```"), "<p>x</p>");
    }

    #[test]
    fn leaves_unfenced_content_untouched() {
        assert_eq!(strip_code_fence("<p>x</p>"), "<p>x</p>");
        assert_eq!(strip_code_fence("  <p>x</p>  "), "<p>x</p>");
    }

    // ── language_name ───────────────────────────────────────────────────

    #[test]
    fn language_name_maps_codes_with_english_fallback() {
        assert_eq!(language_name("zh"), "Simplified Chinese");
        assert_eq!(language_name("ja"), "Japanese");
        assert_eq!(language_name("en"), "English");
        assert_eq!(language_name("xx"), "English");
    }

    // ── text-node extraction / rewrite (the machine-translation path) ────
    //
    // The network call sits between collecting the source text and writing the
    // result back; this helper stands in for it with a synchronous mapper so the
    // structure-preserving collect → apply → serialize round-trip can be tested.

    fn rewrite_with(html: &str, f: impl Fn(&[String]) -> Vec<String>) -> String {
        let mut doc = Html::parse_fragment(html);
        let (slots, cores) = collect_text(&doc);
        let translated = f(&cores);
        apply(&mut doc, &slots, &translated);
        serialize_fragment(&doc)
    }

    fn upper(cores: &[String]) -> Vec<String> {
        cores.iter().map(|c| c.to_uppercase()).collect()
    }

    #[test]
    fn rewrite_preserves_inline_tags_and_attributes() {
        let out = rewrite_with(r#"<p>Hello <a href="/x">world</a></p>"#, upper);
        assert_eq!(out, r#"<p>HELLO <a href="/x">WORLD</a></p>"#);
    }

    #[test]
    fn rewrite_preserves_whitespace_between_inline_words() {
        // The trailing space on "a " must survive so the words stay separated.
        let out = rewrite_with("<p>a <b>b</b></p>", upper);
        assert_eq!(out, "<p>A <b>B</b></p>");
    }

    #[test]
    fn rewrite_skips_code_and_pre_text() {
        let out = rewrite_with("<p>run</p><pre>let x = 1;</pre>", upper);
        assert_eq!(out, "<p>RUN</p><pre>let x = 1;</pre>");
    }

    #[test]
    fn rewrite_keeps_images_and_blocks_with_no_text() {
        let html = r#"<figure><img src="https://e.com/a.png"></figure>"#;
        // No translatable text nodes: collect_text returns nothing, and the
        // fragment serializes back unchanged.
        assert_eq!(rewrite_with(html, upper), html);
    }

    #[test]
    fn rewrite_tolerates_a_short_translation_count() {
        // An engine returning too few segments must not panic or misalign: the
        // unmatched node keeps its source text.
        let out = rewrite_with("<p>one</p><p>two</p>", |_| vec!["UNO".to_string()]);
        assert_eq!(out, "<p>UNO</p><p>two</p>");
    }

    // ── per-engine language codes ───────────────────────────────────────

    #[test]
    fn chunk_budget_is_tuned_per_engine() {
        // The keyless machine-translation engines all take POST bodies, so they
        // share the largest budget; the LLM uses the shared token-bounded
        // default, and an unknown engine falls back to it too.
        assert_eq!(chunk_budget("google"), 8000);
        assert_eq!(chunk_budget("deepl"), 8000);
        assert_eq!(chunk_budget("bing"), 8000);
        assert_eq!(chunk_budget("llm"), ai::TRANSLATE_CHUNK_BUDGET);
        assert_eq!(chunk_budget("???"), ai::TRANSLATE_CHUNK_BUDGET);
    }

    #[test]
    fn engine_codes_map_targets_with_english_fallback() {
        assert_eq!(google_code("zh"), "zh-CN");
        assert_eq!(deepl_code("ja"), "JA");
        assert_eq!(deepl_code("zh"), "ZH");
        assert_eq!(bing_code("zh"), "zh-Hans");
        assert_eq!(google_code("ja"), "ja");
        assert_eq!(deepl_code("xx"), "EN");
        assert_eq!(bing_code("xx"), "en");
    }

    // ── engine response parsers ─────────────────────────────────────────

    #[test]
    fn google_parser_reads_both_response_shapes() {
        // With `sl=auto` each entry is a [text, detectedLang] pair; an explicit
        // source language answers bare strings. Both must parse positionally.
        let pairs = json!([["你好，世界", "en"], ["早上好", "en"]]);
        assert_eq!(
            parse_google_segments(&pairs).unwrap(),
            vec!["你好，世界", "早上好"]
        );
        let bare = json!(["你好，世界", "早上好"]);
        assert_eq!(
            parse_google_segments(&bare).unwrap(),
            vec!["你好，世界", "早上好"]
        );
    }

    #[test]
    fn google_parser_rejects_malformed_bodies() {
        // Not an array at all.
        assert!(parse_google_segments(&json!({"error": "x"})).is_err());
        // An entry that is neither a string nor a [text, lang] pair.
        assert!(parse_google_segments(&json!([42])).is_err());
        // A pair whose first element is not a string.
        assert!(parse_google_segments(&json!([[null, "en"]])).is_err());
        // An empty body parses empty; the call site's count check is what
        // turns an empty response for a non-empty request into an error.
        assert_eq!(parse_google_segments(&json!([])).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn bing_parser_reads_positional_translations() {
        let body = json!([
            {"translations": [{"text": "你好"}]},
            {"translations": [{"text": "世界"}]}
        ]);
        assert_eq!(parse_bing_segments(&body, 2).unwrap(), vec!["你好", "世界"]);
        // An empty request expects an empty response.
        assert!(parse_bing_segments(&json!([]), 0).unwrap().is_empty());
    }

    #[test]
    fn bing_parser_requires_the_response_to_align_with_the_request() {
        let one = json!([{"translations": [{"text": "你好"}]}]);
        // A shorter (or longer) response would silently misalign every
        // following text node, so it must be an error rather than truncated.
        assert!(parse_bing_segments(&one, 2).is_err());
        assert!(parse_bing_segments(&json!("x"), 1).is_err());
    }

    #[test]
    fn bing_parser_rejects_an_entry_without_a_translation() {
        // Blanking the node would silently drop text; surface it as an error.
        assert!(parse_bing_segments(&json!([{"translations": []}]), 1).is_err());
        assert!(parse_bing_segments(&json!([{}]), 1).is_err());
    }
}
