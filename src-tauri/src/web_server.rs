//! Local LAN HTTP server for the mobile web / iOS PWA client.
//!
//! Serves a self-contained mobile SPA (`src-tauri/mobile-web/`) plus a token-
//! authenticated JSON API backed directly by the desktop app's `AppState`.
//! Pairing uses a short-lived one-time code (`?pair=...`) exchanged for a
//! bearer/cookie session; ordinary `/api/*` routes never accept URL query
//! credentials. Mutations emit `"mobile-sync"` to the desktop webview.
//!
//! Transport is deliberately plaintext HTTP on the local network: a random
//! LAN address cannot present a certificate the phone already trusts, and iOS
//! refuses to install a service worker on an untrusted origin anyway. Read the
//! session token as a bearer credential — pair only on networks you trust, and
//! the server is meant to be stopped when not in use. Consequently there is no
//! service-worker/offline mode; "Add to Home Screen" gets the standalone UI
//! from the manifest only.

use crate::commands::{
    build_translate_selection, escape_preview_text, extract_fulltext_with_state, load_ai_config,
    mark_all_read, mark_read, mark_starred, referer_candidates, summary_system_prompt,
    summary_user_turn, text_for_selector, translate_target_lang, IMAGE_UA, MAX_IMAGE_BYTES,
};
use crate::error::{AppError, AppResult};
use crate::integrations::{run_integration_with_state, IntegrationConfig};
use crate::models::ArticleQuery;
use crate::state::AppState;
use crate::{ai, db, sanitize, translate};
use bytes::Bytes;
use http::{header, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(not(unix))]
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::net::TcpListener;
use tokio::sync::watch;

pub const DEFAULT_PORT: u16 = 14320;
const MAX_JSON_BODY_BYTES: usize = 64 * 1024;
const PAIR_CODE_TTL: Duration = Duration::from_secs(600);
const MAX_IMAGE_REDIRECTS: usize = 5;
/// Concurrent connections kept alive for the phone; extra sockets are dropped
/// immediately instead of piling up tasks inside the desktop process.
const MAX_CONNECTIONS: usize = 64;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT_SERVER_ID: AtomicU64 = AtomicU64::new(1);

const INDEX_HTML: &str = include_str!("../mobile-web/index.html");
const STYLES_CSS: &str = include_str!("../mobile-web/styles.css");
const APP_JS: &str = include_str!("../mobile-web/app.js");
const MANIFEST_JSON: &str = include_str!("../mobile-web/manifest.webmanifest");
const ICON_SVG: &str = include_str!("../icons/papr-icon.svg");
const APPLE_TOUCH_ICON_PNG: &[u8] = include_bytes!("../icons/128x128@2x.png");
const ICON_128_PNG: &[u8] = include_bytes!("../icons/128x128.png");
const ICON_512_PNG: &[u8] = include_bytes!("../icons/icon.png");

pub struct WebServerHandle {
    pub id: u64,
    pub port: u16,
    pub token: String,
    pub pair_code: std::sync::Mutex<Option<(String, Instant)>>,
    shutdown: watch::Sender<bool>,
    join: Option<tauri::async_runtime::JoinHandle<()>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebServerStatus {
    pub running: bool,
    pub port: u16,
    pub token: Option<String>,
    pub url: Option<String>,
    pub urls: Vec<String>,
}

pub(crate) fn read_saved_port(conn: &rusqlite::Connection) -> u16 {
    db::get_setting(conn, "web_server_port")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&p| p >= 1024)
        .unwrap_or(DEFAULT_PORT)
}

fn random_code(len: usize) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

fn ensure_web_token(conn: &rusqlite::Connection) -> AppResult<String> {
    if let Ok(Some(t)) = db::get_setting(conn, "web_server_token") {
        let trimmed = t.trim();
        if trimmed.len() >= 12 {
            return Ok(trimmed.to_string());
        }
    }
    let token = random_code(20);
    db::set_setting(conn, "web_server_token", &token)?;
    Ok(token)
}

fn is_rfc1918(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
}

fn is_virtual_tun(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    (a == 198 && (b == 18 || b == 19)) || (a == 100 && (64..=127).contains(&b))
}

fn is_virtual_iface_name(name: &str) -> bool {
    const PREFIXES: [&str; 10] = [
        "lo", "utun", "tun", "tap", "awdl", "llw", "bridge", "docker", "veth", "wg",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(unix)]
fn detect_lan_ips() -> Vec<String> {
    let mut primary = Vec::new();
    let mut secondary = Vec::new();

    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) == 0 && !ifap.is_null() {
            let mut cur = ifap;
            while !cur.is_null() {
                let ifa = &*cur;
                cur = ifa.ifa_next;
                let flags = ifa.ifa_flags as i32;
                if (flags & libc::IFF_UP) == 0
                    || (flags & libc::IFF_RUNNING) == 0
                    || (flags & libc::IFF_LOOPBACK) != 0
                    || ifa.ifa_addr.is_null()
                {
                    continue;
                }
                if i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET {
                    continue;
                }
                let name = if ifa.ifa_name.is_null() {
                    ""
                } else {
                    std::ffi::CStr::from_ptr(ifa.ifa_name)
                        .to_str()
                        .unwrap_or("")
                };
                if is_virtual_iface_name(name) {
                    continue;
                }
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_link_local()
                    || is_virtual_tun(ip)
                {
                    continue;
                }
                let s = ip.to_string();
                if is_rfc1918(ip) {
                    if !primary.contains(&s) {
                        primary.push(s);
                    }
                } else if !secondary.contains(&s) {
                    secondary.push(s);
                }
            }
            libc::freeifaddrs(ifap);
        }
    }

    primary.extend(secondary);
    if primary.is_empty() {
        primary.push("127.0.0.1".to_string());
    }
    primary
}

#[cfg(not(unix))]
fn detect_lan_ips() -> Vec<String> {
    let probes = [
        "192.168.1.1:80",
        "192.168.0.1:80",
        "10.0.0.1:80",
        "172.16.0.1:80",
        "8.8.8.8:80",
    ];
    let mut rfc1918 = Vec::new();
    let mut other = Vec::new();

    for target in probes {
        if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
            if sock.connect(target).is_ok() {
                if let Ok(addr) = sock.local_addr() {
                    if let IpAddr::V4(v4) = addr.ip() {
                        if v4.is_loopback() || v4.is_unspecified() || v4.is_link_local() {
                            continue;
                        }
                        let s = v4.to_string();
                        if is_rfc1918(v4) {
                            if !rfc1918.contains(&s) {
                                rfc1918.push(s);
                            }
                        } else if !is_virtual_tun(v4) && !other.contains(&s) {
                            other.push(s);
                        }
                    }
                }
            }
        }
    }

    rfc1918.extend(other);
    if rfc1918.is_empty() {
        rfc1918.push("127.0.0.1".to_string());
    }
    rfc1918
}

fn build_status(
    running: bool,
    port: u16,
    token: Option<String>,
    pair_code: Option<String>,
) -> WebServerStatus {
    if !running {
        return WebServerStatus {
            running: false,
            port,
            token: None,
            url: None,
            urls: Vec::new(),
        };
    }
    let urls: Vec<String> = detect_lan_ips()
        .into_iter()
        .map(|ip| match &pair_code {
            Some(code) => format!("http://{ip}:{port}/?pair={code}"),
            None => format!("http://{ip}:{port}/"),
        })
        .collect();
    let url = urls.first().cloned();
    WebServerStatus {
        running: true,
        port,
        token,
        url,
        urls,
    }
}

/// Start (or reuse) the server. Caller MUST hold `state.web_server_lock`.
pub(crate) async fn start_server_locked(app: &AppHandle, port: u16) -> AppResult<WebServerStatus> {
    let state = app.state::<AppState>();

    {
        let guard = state.web_server.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.as_ref() {
            if existing.port == port {
                let mut pc = existing.pair_code.lock().unwrap_or_else(|e| e.into_inner());
                let code = match pc.as_ref() {
                    Some((c, exp)) if Instant::now() < *exp => c.clone(),
                    _ => {
                        let c = random_code(12);
                        *pc = Some((c.clone(), Instant::now() + PAIR_CODE_TTL));
                        c
                    }
                };
                return Ok(build_status(
                    true,
                    port,
                    Some(existing.token.clone()),
                    Some(code),
                ));
            }
        }
    }

    // Bind the new port BEFORE tearing down any existing listener so a bind
    // failure (e.g. port already in use) keeps the old server running.
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| AppError::other(format!("Failed to bind port {port}: {e}")))?;

    let token = {
        let conn = state.db.lock().await;
        ensure_web_token(&conn)?
    };
    let pair_code = random_code(12);

    // Commit the "started" intent BEFORE switching listeners: if the write
    // fails, the new socket is dropped, the old server (if any) keeps serving
    // and saved state still matches what is actually running.
    {
        let mut conn = state.db.lock().await;
        let tx = conn.transaction()?;
        db::set_setting(&tx, "web_server_enabled", "1")?;
        db::set_setting(&tx, "web_server_port", &port.to_string())?;
        tx.commit()?;
    }

    stop_server_await(app).await;

    let server_id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = watch::channel(false);
    let app_clone = app.clone();
    let token_clone = token.clone();
    let conn_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    let join = tauri::async_runtime::spawn(async move {
        let mut consecutive_errs: u32 = 0;
        loop {
            tokio::select! {
                _ = rx.changed() => {
                    if *rx.borrow() {
                        break;
                    }
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((stream, _)) => {
                            consecutive_errs = 0;
                            let permit = match conn_slots.clone().try_acquire_owned() {
                                Ok(permit) => permit,
                                Err(_) => {
                                    log::debug!(
                                        "mobile web connection limit reached; dropping socket"
                                    );
                                    drop(stream);
                                    continue;
                                }
                            };
                            let io = TokioIo::new(stream);
                            let app_req = app_clone.clone();
                            let tok_req = token_clone.clone();
                            let mut conn_rx = rx.clone();
                            tauri::async_runtime::spawn(async move {
                                let _permit = permit; // released when the connection ends
                                let svc = service_fn(move |req| {
                                    handle_request(app_req.clone(), tok_req.clone(), req)
                                });
                                let conn = http1::Builder::new()
                                    .header_read_timeout(HEADER_READ_TIMEOUT)
                                    .serve_connection(io, svc);
                                tokio::select! {
                                    res = conn => {
                                        if let Err(err) = res {
                                            log::debug!("mobile web connection closed: {err}");
                                        }
                                    }
                                    _ = conn_rx.changed() => {}
                                }
                            });
                        }
                        Err(err) => {
                            consecutive_errs += 1;
                            log::warn!("mobile web accept error ({consecutive_errs}): {err}");
                            if consecutive_errs >= 8 {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(50 * u64::from(consecutive_errs))).await;
                        }
                    }
                }
            }
        }
        // Clear stale handle only if it still belongs to this listener instance.
        let st = app_clone.state::<AppState>();
        let mut guard = st.web_server.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_some_and(|h| h.id == server_id) {
            guard.take();
        }
        log::info!("mobile web server on port {port} stopped");
    });

    {
        let mut guard = state.web_server.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(WebServerHandle {
            id: server_id,
            port,
            token: token.clone(),
            pair_code: std::sync::Mutex::new(Some((
                pair_code.clone(),
                Instant::now() + PAIR_CODE_TTL,
            ))),
            shutdown: tx,
            join: Some(join),
        });
    }

    Ok(build_status(true, port, Some(token), Some(pair_code)))
}

async fn stop_server_await(app: &AppHandle) {
    let handle = {
        let state = app.state::<AppState>();
        let mut guard = state.web_server.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    if let Some(mut h) = handle {
        let _ = h.shutdown.send(true);
        if let Some(join) = h.join.take() {
            let _ = join.await;
        }
    }
}

pub fn stop_server_sync(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut guard = state.web_server.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = guard.take() {
        let _ = h.shutdown.send(true);
    }
}

#[tauri::command]
pub async fn web_server_status(state: State<'_, AppState>) -> AppResult<WebServerStatus> {
    let running = {
        let guard = state.web_server.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|h| {
            let pc = h.pair_code.lock().unwrap_or_else(|e| e.into_inner());
            let code = pc
                .as_ref()
                .filter(|(_, exp)| Instant::now() < *exp)
                .map(|(c, _)| c.clone());
            (h.port, h.token.clone(), code)
        })
    };
    if let Some((port, token, code)) = running {
        return Ok(build_status(true, port, Some(token), code));
    }
    let conn = state.read().await;
    Ok(build_status(false, read_saved_port(&conn), None, None))
}

#[tauri::command]
pub async fn web_server_start(
    app: AppHandle,
    state: State<'_, AppState>,
    port: Option<u16>,
) -> AppResult<WebServerStatus> {
    let port = port.filter(|&p| p >= 1024).unwrap_or(DEFAULT_PORT);
    let _op = state.web_server_lock.lock().await;
    start_server_locked(&app, port).await
}

#[tauri::command]
pub async fn web_server_stop(app: AppHandle, state: State<'_, AppState>) -> AppResult<WebServerStatus> {
    let _op = state.web_server_lock.lock().await;
    // Persist the disabled flag first: if that write fails we return an error
    // and leave the server running, so runtime and saved state still agree —
    // the next launch will not silently reopen a server the user stopped.
    {
        let conn = state.db.lock().await;
        db::set_setting(&conn, "web_server_enabled", "0")?;
    }
    stop_server_await(&app).await;
    let port = {
        let conn = state.db.lock().await;
        read_saved_port(&conn)
    };
    Ok(build_status(false, port, None, None))
}

// ─────────────────────────── Security helpers ───────────────────────────

fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn extract_cookie_token(req: &Request<Incoming>) -> Option<String> {
    let cookie = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let trimmed = part.trim();
        if let Some(val) = trimmed.strip_prefix("papr_token=") {
            return Some(val.to_string());
        }
    }
    None
}

fn is_authorized(req: &Request<Incoming>, expected: &str) -> bool {
    if let Some(auth) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(bearer) = auth.strip_prefix("Bearer ") {
            if ct_eq(bearer.trim(), expected) {
                return true;
            }
        }
    }
    if let Some(cookie_tok) = extract_cookie_token(req) {
        if ct_eq(cookie_tok.trim(), expected) {
            return true;
        }
    }
    false
}

fn consume_pair_code(app: &AppHandle, code: &str) -> bool {
    let state = app.state::<AppState>();
    let guard = state.web_server.lock().unwrap_or_else(|e| e.into_inner());
    let Some(handle) = guard.as_ref() else {
        return false;
    };
    let mut pc = handle.pair_code.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((expected, exp)) = pc.as_ref() {
        if Instant::now() < *exp && ct_eq(code.trim(), expected) {
            *pc = None;
            return true;
        }
    }
    false
}

fn is_global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !v4.is_loopback()
                && !v4.is_private()
                && !v4.is_link_local()
                && !v4.is_multicast()
                && !v4.is_broadcast()
                && !v4.is_unspecified()
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 192 && b == 0)
        }
        IpAddr::V6(v6) => {
            // `::ffff:127.0.0.1` and friends are loopback/private in disguise;
            // judge them by the embedded IPv4 address, not by IPv6 flags.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_global_ip(IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            !v6.is_loopback()
                && !v6.is_multicast()
                && !v6.is_unspecified()
                && (seg0 & 0xfe00) != 0xfc00
                && (seg0 & 0xffc0) != 0xfe80
        }
    }
}

async fn validate_and_resolve_public_url(raw: &str) -> AppResult<(url::Url, String, Vec<SocketAddr>)> {
    let parsed = url::Url::parse(raw).map_err(|_| AppError::other("Invalid image URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::other("Unsupported image URL scheme"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| AppError::other("Image URL missing host"))?
        .to_string();
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") || host.ends_with(".local") {
        return Err(AppError::other("Localhost image URL blocked"));
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_global_ip(ip) {
            return Err(AppError::other("Private IP image URL blocked"));
        }
        return Ok((parsed, host, vec![SocketAddr::new(ip, port)]));
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| AppError::other("Failed to resolve image host"))?
        .collect();
    if addrs.is_empty() {
        return Err(AppError::other("No addresses resolved for image host"));
    }
    for addr in &addrs {
        if !is_global_ip(addr.ip()) {
            return Err(AppError::other("Resolved private IP blocked"));
        }
    }
    Ok((parsed, host, addrs))
}

/// SSRF-safe image proxy fetch: disables automatic redirects, pins DNS
/// resolution to the validated public `SocketAddr`s on each hop, and streams
/// chunks up to `MAX_IMAGE_BYTES`.
async fn fetch_public_image_bytes(raw_url: &str, page_url: Option<&str>) -> AppResult<Vec<u8>> {
    let mut current_url = raw_url.to_string();
    for _hop in 0..=MAX_IMAGE_REDIRECTS {
        let (parsed, host, addrs) = validate_and_resolve_public_url(&current_url).await?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            // `resolve_to_addrs` binds only this client's DNS answer; an HTTP
            // proxy from the environment would re-resolve the host elsewhere,
            // so the pinned addresses must connect directly.
            .no_proxy()
            .resolve_to_addrs(&host, &addrs)
            .build()
            .map_err(|e| AppError::other(format!("image client: {e}")))?;

        let mut redirected_to: Option<String> = None;
        let mut last_err = AppError::code("badImageUrl");
        for referer in referer_candidates(parsed.as_str(), page_url) {
            let mut req = client.get(parsed.as_str()).header("User-Agent", IMAGE_UA);
            if let Some(r) = &referer {
                req = req.header("Referer", r.as_str());
            }
            let mut resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.into();
                    break;
                }
            };
            if resp.status().is_redirection() {
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| AppError::other("Redirect missing Location"))?;
                let next = parsed
                    .join(loc)
                    .map_err(|_| AppError::other("Invalid redirect URL"))?;
                redirected_to = Some(next.to_string());
                break;
            }
            if !resp.status().is_success() {
                last_err = AppError::other(format!("HTTP {}", resp.status()));
                continue;
            }
            if resp.content_length().is_some_and(|n| n > MAX_IMAGE_BYTES) {
                return Err(AppError::code("imageTooLarge"));
            }
            let mut buf = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if buf.len() as u64 + chunk.len() as u64 > MAX_IMAGE_BYTES {
                    return Err(AppError::code("imageTooLarge"));
                }
                buf.extend_from_slice(&chunk);
            }
            return Ok(buf);
        }

        if let Some(next_url) = redirected_to {
            current_url = next_url;
            continue;
        }
        return Err(last_err);
    }
    Err(AppError::other("Too many image redirects"))
}

// ─────────────────────────── HTTP routing ───────────────────────────

type Resp = Response<Full<Bytes>>;

fn text_resp(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Resp {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Full::new(body.into()))
        .unwrap()
}

fn asset_resp(content_type: &str, body: impl Into<Bytes>, cache: &str) -> Resp {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, cache)
        .body(Full::new(body.into()))
        .unwrap()
}

fn json_resp<T: Serialize>(status: StatusCode, value: &T) -> Resp {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap()
}

fn err_resp(err: AppError) -> Resp {
    let msg = err.to_string();
    let status = if msg.contains("articleChanged") {
        StatusCode::CONFLICT
    } else if msg.to_ascii_lowercase().contains("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    json_resp(status, &json!({ "error": msg }))
}

fn parse_query_params(raw: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(q) = raw else { return map };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        let dk = percent_encoding::percent_decode_str(k)
            .decode_utf8_lossy()
            .to_string();
        let dv = percent_encoding::percent_decode_str(v)
            .decode_utf8_lossy()
            .to_string();
        map.insert(dk, dv);
    }
    map
}

async fn read_json_body<T: for<'de> Deserialize<'de>>(req: Request<Incoming>) -> Result<T, AppError> {
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("application/json"))
        .unwrap_or(false);
    if !is_json {
        return Err(AppError::other("Content-Type must be application/json"));
    }
    let collected = tokio::time::timeout(
        BODY_READ_TIMEOUT,
        Limited::new(req.into_body(), MAX_JSON_BODY_BYTES).collect(),
    )
    .await
    .map_err(|_| AppError::other("read body: timed out"))?
    .map_err(|e| AppError::other(format!("read body: {e}")))?;
    let bytes = collected.to_bytes();
    if bytes.is_empty() {
        serde_json::from_slice(b"{}").map_err(|e| AppError::other(format!("invalid json: {e}")))
    } else {
        serde_json::from_slice(&bytes).map_err(|e| AppError::other(format!("invalid json: {e}")))
    }
}

fn build_article_query(kind: Option<&str>, value: Option<i64>) -> ArticleQuery {
    match kind.unwrap_or("unread") {
        "all" => ArticleQuery::All,
        "starred" => ArticleQuery::Starred,
        "readLater" => ArticleQuery::ReadLater,
        "feed" => ArticleQuery::Feed(value.unwrap_or(0)),
        "folder" => ArticleQuery::Folder(value.unwrap_or(0)),
        "tag" => ArticleQuery::Tag(value.unwrap_or(0)),
        _ => ArticleQuery::Unread,
    }
}

fn sniff_image_mime(bytes: &[u8], url: &str) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else if bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        "image/x-icon"
    } else if bytes.len() >= 12 && &bytes[4..12] == b"ftypavif" {
        "image/avif"
    } else {
        let trimmed = std::str::from_utf8(&bytes[..bytes.len().min(256)])
            .unwrap_or("")
            .trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
        if trimmed.starts_with("<svg") || trimmed.starts_with("<?xml") || url.to_ascii_lowercase().contains(".svg") {
            "image/svg+xml"
        } else {
            "application/octet-stream"
        }
    }
}

async fn handle_request(
    app: AppHandle,
    expected_token: String,
    req: Request<Incoming>,
) -> Result<Resp, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let params = parse_query_params(req.uri().query());

    if method == Method::GET {
        match path.as_str() {
            "/" | "/index.html" => {
                return Ok(text_resp(StatusCode::OK, "text/html; charset=utf-8", INDEX_HTML));
            }
            "/styles.css" => return Ok(text_resp(StatusCode::OK, "text/css; charset=utf-8", STYLES_CSS)),
            "/app.js" => {
                return Ok(text_resp(
                    StatusCode::OK,
                    "application/javascript; charset=utf-8",
                    APP_JS,
                ))
            }
            "/manifest.webmanifest" => {
                return Ok(asset_resp(
                    "application/manifest+json; charset=utf-8",
                    MANIFEST_JSON,
                    "no-cache",
                ))
            }
            "/apple-touch-icon.png" => {
                return Ok(asset_resp("image/png", APPLE_TOUCH_ICON_PNG, "public, max-age=86400"))
            }
            "/icon-128.png" => {
                return Ok(asset_resp("image/png", ICON_128_PNG, "public, max-age=86400"))
            }
            "/icon-512.png" => {
                return Ok(asset_resp("image/png", ICON_512_PNG, "public, max-age=86400"))
            }
            "/icon.svg" => {
                return Ok(asset_resp("image/svg+xml", ICON_SVG, "public, max-age=86400"))
            }
            _ => {}
        }
    }

    // One-time pairing code exchange -> returns long-lived token + sets HttpOnly cookie
    if method == Method::POST && path == "/api/pair" {
        #[derive(Deserialize)]
        struct Body {
            code: String,
        }
        return Ok(match read_json_body::<Body>(req).await {
            Ok(b) if consume_pair_code(&app, &b.code) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-store")
                .header(
                    header::SET_COOKIE,
                    format!("papr_token={expected_token}; Path=/; HttpOnly; SameSite=Lax"),
                )
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&json!({ "token": expected_token })).unwrap(),
                )))
                .unwrap(),
            Ok(_) => json_resp(StatusCode::UNAUTHORIZED, &json!({ "error": "Invalid or expired pairing code" })),
            Err(e) => err_resp(e),
        });
    }

    // All other /api/* endpoints require Bearer header or HttpOnly cookie (never URL query params)
    if path.starts_with("/api/") && !is_authorized(&req, &expected_token) {
        return Ok(json_resp(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "Unauthorized" }),
        ));
    }

    let resp = match (method.clone(), path.as_str()) {
        // ── Auth check / cookie sync ──
        (Method::POST, "/api/auth") => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .header(
                header::SET_COOKIE,
                format!("papr_token={expected_token}; Path=/; HttpOnly; SameSite=Lax"),
            )
            .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
            .unwrap(),

        // ── Bootstrap ──
        (Method::GET, "/api/bootstrap") => match api_bootstrap(&app).await {
            Ok(v) => json_resp(StatusCode::OK, &v),
            Err(e) => err_resp(e),
        },

        // ── Articles list ──
        (Method::GET, "/api/articles") => {
            let kind = params.get("kind").map(String::as_str);
            let value = params.get("value").and_then(|s| s.parse::<i64>().ok());
            let unread_only = params
                .get("unreadOnly")
                .map(|s| s == "true" || s == "1")
                .unwrap_or(false);
            let search = params
                .get("search")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let limit = params
                .get("limit")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(40)
                .clamp(1, 100);
            let offset = params
                .get("offset")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0)
                .max(0);
            let query = build_article_query(kind, value);
            let state = app.state::<AppState>();
            let conn = state.read().await;
            match db::list_articles(
                &conn,
                &query,
                unread_only,
                search.as_deref(),
                false,
                limit,
                offset,
            ) {
                Ok(list) => json_resp(StatusCode::OK, &list),
                Err(e) => err_resp(e),
            }
        }

        // ── Mark all read ──
        (Method::POST, "/api/articles/mark-all-read") => {
            #[derive(Deserialize)]
            struct Body {
                kind: Option<String>,
                value: Option<i64>,
            }
            match read_json_body::<Body>(req).await {
                Ok(b) => {
                    let q = build_article_query(b.kind.as_deref(), b.value);
                    match mark_all_read(app.clone(), q).await {
                        Ok(count) => {
                            let _ = app.emit("mobile-sync", ());
                            json_resp(StatusCode::OK, &json!({ "count": count }))
                        }
                        Err(e) => err_resp(e),
                    }
                }
                Err(e) => err_resp(e),
            }
        }

        // ── Image proxy (SSRF-safe + SVG sandboxed + private cache) ──
        (Method::GET, "/api/image") => {
            let Some(url) = params.get("url").cloned().filter(|u| !u.is_empty()) else {
                return Ok(text_resp(StatusCode::BAD_REQUEST, "text/plain", "missing url"));
            };
            let page_url = params.get("pageUrl").cloned();
            match fetch_public_image_bytes(&url, page_url.as_deref()).await {
                Ok(bytes) => {
                    let mime = sniff_image_mime(&bytes, &url);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, mime)
                        .header(header::CACHE_CONTROL, "private, max-age=86400")
                        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
                        .header(
                            header::CONTENT_SECURITY_POLICY,
                            "default-src 'none'; sandbox",
                        )
                        .body(Full::new(Bytes::from(bytes)))
                        .unwrap()
                }
                Err(_) => text_resp(StatusCode::BAD_GATEWAY, "text/plain", "Image fetch failed"),
            }
        }

        // ── Dynamic /api/articles/:id... routes ──
        _ if path.starts_with("/api/articles/") => {
            handle_article_route(app, method, &path, req).await
        }

        _ => text_resp(StatusCode::NOT_FOUND, "text/plain", "Not Found"),
    };

    Ok(resp)
}

async fn handle_article_route(
    app: AppHandle,
    method: Method,
    path: &str,
    req: Request<Incoming>,
) -> Resp {
    let rest = &path["/api/articles/".len()..];
    let mut parts = rest.splitn(2, '/');
    let Ok(id) = parts.next().unwrap_or("").parse::<i64>() else {
        return text_resp(StatusCode::BAD_REQUEST, "text/plain", "invalid article id");
    };
    let action = parts.next();

    match (method, action) {
        (Method::GET, None) => {
            let state = app.state::<AppState>();
            let conn = state.read().await;
            match db::get_article(&conn, id) {
                Ok(detail) => {
                    let summary: Option<String> = conn
                        .query_row("SELECT summary FROM articles WHERE id = ?1", [id], |r| r.get(0))
                        .ok()
                        .flatten();
                    let mut val = serde_json::to_value(&detail).unwrap_or_else(|_| json!({}));
                    if let Some(obj) = val.as_object_mut() {
                        obj.insert("summary".to_string(), json!(summary));
                    }
                    json_resp(StatusCode::OK, &val)
                }
                Err(e) => err_resp(e),
            }
        }
        (Method::POST, Some("read")) => {
            #[derive(Deserialize)]
            struct Body {
                read: bool,
            }
            match read_json_body::<Body>(req).await {
                Ok(b) => match mark_read(app.clone(), id, b.read).await {
                    Ok(()) => {
                        let _ = app.emit("mobile-sync", ());
                        json_resp(StatusCode::OK, &json!({ "ok": true }))
                    }
                    Err(e) => err_resp(e),
                },
                Err(e) => err_resp(e),
            }
        }
        (Method::POST, Some("starred")) => {
            #[derive(Deserialize)]
            struct Body {
                starred: bool,
            }
            match read_json_body::<Body>(req).await {
                Ok(b) => match mark_starred(app.clone(), id, b.starred).await {
                    Ok(()) => {
                        let _ = app.emit("mobile-sync", ());
                        json_resp(StatusCode::OK, &json!({ "ok": true }))
                    }
                    Err(e) => err_resp(e),
                },
                Err(e) => err_resp(e),
            }
        }
        (Method::POST, Some("read-later")) => {
            #[derive(Deserialize)]
            struct Body {
                value: bool,
            }
            match read_json_body::<Body>(req).await {
                Ok(b) => {
                    let state = app.state::<AppState>();
                    let conn = state.db.lock().await;
                    match db::set_read_later(&conn, id, b.value) {
                        Ok(()) => {
                            let _ = app.emit("mobile-sync", ());
                            json_resp(StatusCode::OK, &json!({ "ok": true }))
                        }
                        Err(e) => err_resp(e),
                    }
                }
                Err(e) => err_resp(e),
            }
        }
        (Method::POST, Some("fulltext")) => {
            if let Err(e) = read_json_body::<Value>(req).await {
                return err_resp(e);
            }
            let state = app.state::<AppState>();
            match extract_fulltext_with_state(&state, id).await {
                Ok(html) => {
                    let _ = app.emit("mobile-sync", ());
                    json_resp(StatusCode::OK, &json!({ "html": html }))
                }
                Err(e) => err_resp(e),
            }
        }
        (Method::POST, Some("summarize")) => {
            if let Err(e) = read_json_body::<Value>(req).await {
                return err_resp(e);
            }
            match api_summarize(&app, id).await {
                Ok(summary) => {
                    let _ = app.emit("mobile-sync", ());
                    json_resp(StatusCode::OK, &json!({ "summary": summary }))
                }
                Err(e) => err_resp(e),
            }
        }
        (Method::POST, Some("translate")) => {
            #[derive(Deserialize, Default)]
            struct Body {
                lang: Option<String>,
                engine: Option<String>,
            }
            match read_json_body::<Body>(req).await {
                Ok(body) => match api_translate(&app, id, body.lang, body.engine).await {
                    Ok(v) => {
                        let _ = app.emit("mobile-sync", ());
                        json_resp(StatusCode::OK, &v)
                    }
                    Err(e) => err_resp(e),
                },
                Err(e) => err_resp(e),
            }
        }
        (Method::POST, Some("integration")) => {
            #[derive(Deserialize)]
            struct Body {
                id: String,
            }
            match read_json_body::<Body>(req).await {
                Ok(b) => match api_run_integration(&app, id, &b.id).await {
                    Ok(v) => json_resp(StatusCode::OK, &v),
                    Err(e) => err_resp(e),
                },
                Err(e) => err_resp(e),
            }
        }
        _ => text_resp(StatusCode::NOT_FOUND, "text/plain", "Not Found"),
    }
}

// ─────────────────────────── Integration helpers ───────────────────────────

const BUILTIN_KEYS: [&str; 7] = [
    "cubox",
    "readwise",
    "instapaper",
    "outline",
    "readeck",
    "zotero",
    "obsidian",
];

fn clean_opt(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn builtin_ready(key: &str, cfg: &Value) -> bool {
    if cfg.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
        return false;
    }
    match key {
        "cubox" => clean_opt(cfg.get("apiUrl")).is_some(),
        "readwise" => clean_opt(cfg.get("token")).is_some(),
        "instapaper" => {
            clean_opt(cfg.get("username")).is_some() && clean_opt(cfg.get("password")).is_some()
        }
        "outline" => {
            clean_opt(cfg.get("endpoint")).is_some()
                && clean_opt(cfg.get("token")).is_some()
                && clean_opt(cfg.get("collection")).is_some()
        }
        "readeck" => clean_opt(cfg.get("endpoint")).is_some() && clean_opt(cfg.get("token")).is_some(),
        "zotero" => clean_opt(cfg.get("userId")).is_some() && clean_opt(cfg.get("token")).is_some(),
        "obsidian" => clean_opt(cfg.get("vaultPath")).is_some(),
        _ => false,
    }
}

fn safe_integration_list(raw: Option<&str>) -> Value {
    let Some(parsed) = raw.and_then(|s| serde_json::from_str::<Value>(s).ok()) else {
        return json!({ "useAiSummary": false, "items": [] });
    };
    let use_ai_summary = parsed
        .get("useAiSummary")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut items = Vec::new();
    if let Some(builtins) = parsed.get("builtins") {
        for key in BUILTIN_KEYS {
            if let Some(cfg) = builtins.get(key) {
                if builtin_ready(key, cfg) {
                    items.push(json!({ "id": key, "kind": key, "name": key }));
                }
            }
        }
    }
    if let Some(customs) = parsed.get("customs").and_then(|v| v.as_array()) {
        for c in customs {
            if c.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            let Some(id) = clean_opt(c.get("id")) else {
                continue;
            };
            let kind = match c.get("kind").and_then(|v| v.as_str()) {
                Some("urlScheme") => "urlScheme",
                _ => "webhook",
            };
            let ready = if kind == "webhook" {
                clean_opt(c.get("method")).is_some() && clean_opt(c.get("url")).is_some()
            } else {
                clean_opt(c.get("scheme")).is_some()
            };
            if ready {
                let name = clean_opt(c.get("name")).unwrap_or_else(|| kind.to_string());
                items.push(json!({ "id": id, "kind": kind, "name": name }));
            }
        }
    }
    json!({ "useAiSummary": use_ai_summary, "items": items })
}

async fn api_run_integration(app: &AppHandle, article_id: i64, target_id: &str) -> AppResult<Value> {
    let state = app.state::<AppState>();
    let (kind, use_ai_summary, config) = {
        let conn = state.read().await;
        let raw = db::get_setting(&conn, "integrations")?
            .ok_or_else(|| AppError::code("integrationUnknown"))?;
        let parsed: Value =
            serde_json::from_str(&raw).map_err(|_| AppError::code("integrationUnknown"))?;
        let use_ai_summary = parsed
            .get("useAiSummary")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if BUILTIN_KEYS.contains(&target_id) {
            let cfg_val = parsed
                .get("builtins")
                .and_then(|b| b.get(target_id))
                .ok_or_else(|| AppError::code("integrationUnknown"))?;
            if !builtin_ready(target_id, cfg_val) {
                return Err(AppError::code("integrationUnknown"));
            }
            let cfg: IntegrationConfig = serde_json::from_value(cfg_val.clone())
                .map_err(|_| AppError::code("integrationUnknown"))?;
            (target_id.to_string(), use_ai_summary, cfg)
        } else {
            let custom = parsed
                .get("customs")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter().find(|c| {
                        c.get("id").and_then(|x| x.as_str()) == Some(target_id)
                            && c.get("enabled").and_then(|x| x.as_bool()) == Some(true)
                    })
                })
                .ok_or_else(|| AppError::code("integrationUnknown"))?;
            let kind = match custom.get("kind").and_then(|v| v.as_str()) {
                Some("urlScheme") => "urlScheme",
                _ => "webhook",
            };
            let cfg: IntegrationConfig = serde_json::from_value(custom.clone())
                .map_err(|_| AppError::code("integrationUnknown"))?;
            (kind.to_string(), use_ai_summary, cfg)
        }
    };

    let outcome = run_integration_with_state(
        app,
        &state,
        kind.clone(),
        use_ai_summary,
        config,
        article_id,
        false,
    )
    .await?;

    Ok(json!({
        "url": outcome.url,
        "clientOpen": kind == "urlScheme",
    }))
}

async fn api_bootstrap(app: &AppHandle) -> AppResult<Value> {
    let state = app.state::<AppState>();
    let conn = state.read().await;
    let folders = db::list_folders(&conn)?;
    let feeds = db::list_feeds(&conn)?;
    let (unread, starred, read_later) = db::smart_counts(&conn)?;
    let language = db::get_setting(&conn, "language")
        .ok()
        .flatten()
        .unwrap_or_else(|| "zh".to_string());
    let translate_engine = db::get_setting(&conn, "translate_engine")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "llm".to_string());
    let translate_target_lang = translate_target_lang(&conn);
    let integrations_raw = db::get_setting(&conn, "integrations").ok().flatten();
    let integrations = safe_integration_list(integrations_raw.as_deref());

    Ok(json!({
        "folders": folders,
        "feeds": feeds,
        "counts": {
            "unread": unread,
            "starred": starred,
            "readLater": read_later,
        },
        "language": language,
        "translateEngine": translate_engine,
        "translateTargetLang": translate_target_lang,
        "integrations": integrations,
    }))
}

async fn api_summarize(app: &AppHandle, article_id: i64) -> AppResult<String> {
    let state = app.state::<AppState>();
    let (title, body, cfg, system) = {
        let conn = state.read().await;
        let detail = db::get_article(&conn, article_id)?;
        if let Some(s) = detail.ai_summary.filter(|s| !s.trim().is_empty()) {
            return Ok(s);
        }
        let (title, body) = db::article_text(&conn, article_id)?;
        (
            title,
            body,
            load_ai_config(&conn)?,
            summary_system_prompt(&conn),
        )
    };
    if body.trim().is_empty() {
        return Err(AppError::code("noArticleBody"));
    }
    let user = summary_user_turn(&title, &body);
    let http = state.http();
    let text = ai::complete_chat(&http, &cfg, &system, &user, ai::MAX_TOKENS).await?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::code("aiEmptyResponse"));
    }
    let conn = state.db.lock().await;
    db::set_ai_summary(&conn, article_id, &text)?;
    Ok(text)
}

async fn api_translate(
    app: &AppHandle,
    article_id: i64,
    lang: Option<String>,
    engine: Option<String>,
) -> AppResult<Value> {
    let state = app.state::<AppState>();
    let (source_html, article_title, sel, target) = {
        let conn = state.read().await;
        let detail = db::get_article(&conn, article_id)?;
        let target = lang
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| translate_target_lang(&conn));
        if let (Some(th), Some(tl)) = (
            detail.translated_html.as_ref().filter(|s| !s.trim().is_empty()),
            detail.translated_lang.as_deref(),
        ) {
            if tl == target {
                return Ok(json!({
                    "translatedHtml": th,
                    "translatedTitle": detail.translated_title,
                    "translatedLang": tl,
                }));
            }
        }
        let source = detail
            .extracted_html
            .filter(|s| !s.trim().is_empty())
            .or(detail.content_html)
            .unwrap_or_default();
        let engine_str = engine
            .filter(|s| !s.trim().is_empty())
            .or_else(|| db::get_setting(&conn, "translate_engine").ok().flatten())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "llm".to_string());
        (
            source,
            detail.title,
            build_translate_selection(&conn, &engine_str)?,
            target,
        )
    };
    if source_html.trim().is_empty() {
        return Err(AppError::code("noArticleBody"));
    }

    let engine_name = match &sel {
        translate::Selection::Google => "google",
        translate::Selection::Bing => "bing",
        translate::Selection::Deepl => "deepl",
        translate::Selection::Llm(_) => "llm",
    };
    let http = state.http();
    let backend = translate::ready(sel)?;

    let is_card = translate::is_tweet_card(&source_html);
    let batches = if is_card {
        translate::tweet_body_batches(&source_html)
    } else {
        translate::chunk_blocks(&source_html, translate::chunk_budget(engine_name))
    };
    let system = translate::translate_system_prompt(translate::language_name(&target));

    let translated_title = if is_card || article_title.trim().is_empty() {
        None
    } else {
        let title_source = format!("<h1>{}</h1>", escape_preview_text(article_title.trim()));
        match backend
            .translate_batch(&http, &system, &title_source, &target)
            .await
        {
            Ok(raw) => {
                let clean = sanitize::sanitize(raw.trim(), None);
                let text = text_for_selector(&clean, "h1");
                (!text.trim().is_empty()).then_some(text)
            }
            Err(_) => None,
        }
    };

    let mut full = String::new();
    let mut bodies = Vec::new();
    for batch in &batches {
        let raw = backend.translate_batch(&http, &system, batch, &target).await?;
        let clean = if is_card {
            sanitize::sanitize_parser(raw.trim(), None)
        } else {
            sanitize::sanitize(raw.trim(), None)
        };
        if is_card {
            bodies.push(clean);
        } else {
            full.push_str(&clean);
            full.push('\n');
        }
    }

    let final_html = if is_card {
        translate::splice_tweet_bodies(&source_html, &bodies)
    } else {
        full.trim().to_string()
    };

    if !final_html.is_empty() {
        let conn = state.db.lock().await;
        let (current_source, current_title) = match db::get_article(&conn, article_id) {
            Ok(d) => (
                d.extracted_html
                    .filter(|s| !s.trim().is_empty())
                    .or(d.content_html)
                    .unwrap_or_default(),
                d.title,
            ),
            Err(_) => (String::new(), String::new()),
        };
        if current_source != source_html || current_title != article_title {
            return Err(AppError::code("articleChanged"));
        }
        db::set_translation(
            &conn,
            article_id,
            &final_html,
            &target,
            translated_title.as_deref(),
        )?;
    }

    Ok(json!({
        "translatedHtml": final_html,
        "translatedTitle": translated_title,
        "translatedLang": target,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_lan_ips_returns_non_empty() {
        let ips = detect_lan_ips();
        println!("detected LAN IPs: {:?}", ips);
        assert!(!ips.is_empty());
    }
}
