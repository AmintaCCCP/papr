//! The local RSSHub engine — a bundled Node + RSSHub build executed on the
//! user's machine, so `rsshub://` subscriptions never depend on a hosted
//! RSSHub instance (public instances rate-limit and blacklist; local execution
//! is the whole point of the feature).
//!
//! The manager is UI-free: the engine root directory is supplied by the
//! caller (the desktop app passes its data dir; tests pass a temp dir).
//!
//! Layout on disk:
//!
//! ```text
//! <engine_root>/engine.json            { version, rsshubRevision, nodeVersion, installedAt }
//! <engine_root>/engine.log             the engine's stdout/stderr, appended per run
//! <engine_root>/<version>/bin/node     the bundled Node runtime (node.exe on Windows)
//! <engine_root>/<version>/app/         the RSSHub build product
//!     package.json  dist/…  node_modules/…
//! ```
//!
//! [`EngineManager::ensure`] lazily spawns the engine on a random loopback
//! port with a minimal environment whitelist plus the route environment
//! variables the user configured in the marketplace, waits for the HTTP
//! server to answer (60 s budget), and an idle watchdog tears the process
//! down after 10 idle minutes. Nothing ever binds beyond 127.0.0.1: the
//! spawn pins `LISTEN_INADDR_ANY=0` *after* the user's variables (RSSHub's
//! own default is to bind every interface).

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Coded error keys — the frontend maps each to an `error.<code>` i18n key.
/// The X parser's coded errors are the established pattern (see
/// `parsers::x::CODE_*`).
pub const CODE_ENGINE_MISSING: &str = "rsshubEngineMissing";
pub const CODE_ENGINE_START: &str = "rsshubEngineStart";
pub const CODE_ROUTE_FETCH: &str = "rsshubRouteFetch";
pub const CODE_ARCHIVE_INVALID: &str = "rsshubEngineArchiveInvalid";
pub const CODE_ARCHIVE_CHECKSUM: &str = "rsshubEngineChecksum";
pub const CODE_ENV_TOO_LARGE: &str = "rsshubEngineEnvTooLarge";

/// Health-check budget when *starting* the engine. First launch loads
/// thousands of route modules; a cold start can take tens of seconds.
const START_BUDGET: Duration = Duration::from_secs(60);
/// Budget for the per-call liveness ping of an already-running engine.
const PING_BUDGET: Duration = Duration::from_secs(3);
/// Interval between health probes.
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
/// Per-request timeout for a health probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the idle watchdog checks `last_used`.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Tear the engine down after this much idle time. The engine holds several
/// hundred MB of RSS — no reason to keep it warm while the user reads.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Hard cap on an installed engine archive (a full Node + RSSHub + node_modules
/// bundle is a few hundred MB compressed; 1 GiB leaves generous headroom while
/// still refusing a runaway download).
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
/// Hard cap on the *extracted* tree. A real install is ~1.1 GB (measured:
/// RSSHub node_modules ≈ 900 MB + Node); the cap mainly refuses a crafted
/// zip/tar bomb whose compressed form fits under [`MAX_ARCHIVE_BYTES`].
const MAX_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Environment variables the engine process may inherit. Everything else is
/// stripped: the child runs arbitrary scraping code with credentials the user
/// typed into the marketplace, so it gets the minimum needed to run Node and
/// reach the internet (proxy variables included — routes behind a corporate
/// proxy need the same egress as the rest of the app, and `PROXY_URI` is
/// RSSHub's own proxy setting).
const ENV_ALLOWLIST: &[&str] = &[
    // Unix basics.
    "PATH",
    "HOME",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "TZ",
    "USER",
    "LOGNAME",
    "SHELL",
    // TLS trust store overrides (custom CAs).
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    // Windows basics Node's `os`/`child_process` modules expect.
    "SYSTEMROOT",
    "COMSPEC",
    "PATHEXT",
    "PROGRAMFILES",
    "PROGRAMDATA",
    "SYSTEMDRIVE",
    "HOMEDRIVE",
    "HOMEPATH",
    "ALLUSERSPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "WINDIR",
    // Proxy egress.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "PROXY_URI",
];

/// Sanity ceiling on the engine child's *total* environment size (names +
/// values, bytes, including the C-string separators). `execve` caps argv +
/// env together (`E2BIG` past the platform limit), so individually valid
/// values (`rsshub::MAX_ENV_VALUE_BYTES` each) can still accumulate past it
/// once merged across routes. The *effective* budget adapts to what this
/// kernel actually accepts ([`execve_budget`]); this fixed ceiling only
/// keeps a host with a huge `ARG_MAX` from accepting environments that are
/// almost certainly a mistake.
const MAX_TOTAL_ENV_BYTES: usize = 1024 * 1024;
/// Headroom subtracted from the platform's `ARG_MAX` for argv (the node
/// binary + entry path) and the kernel's per-string accounting slop.
const ARGV_HEADROOM_BYTES: usize = 64 * 1024;
/// Floor for the effective budget: even a tiny `RLIMIT_STACK`-derived
/// `ARG_MAX` (Linux: soft stack / 4, floored at 32 pages ≈ 128 KiB) leaves
/// this much room for a real environment, and a budget of zero would error
/// every start regardless of configuration.
const MIN_ENV_BUDGET_BYTES: usize = 96 * 1024;
/// `E2BIG` — the errno `execve` returns when argv+env exceed the platform
/// limit. The number is the same across the Unix targets we ship (Linux,
/// macOS).
#[cfg(unix)]
const E2BIG: i32 = libc::E2BIG;

/// The environment size this process's `execve` actually accepts, as a
/// budget: `ARG_MAX − argv headroom`, clamped to the sanity ceiling. On
/// Linux `_SC_ARG_MAX` already reflects the current `RLIMIT_STACK` (soft
/// stack / 4, floored at 32 pages), so a low stack limit shrinks the budget
/// instead of failing the spawn with `E2BIG`. Non-Unix platforms have no
/// portable `ARG_MAX`; they get the fixed ceiling.
fn execve_budget() -> usize {
    budget_for(platform_arg_max())
}

/// The budget arithmetic, parameterized over the platform's `ARG_MAX` so the
/// low-stack shape is testable without mutating this process's rlimits.
fn budget_for(arg_max: Option<usize>) -> usize {
    let budget = match arg_max {
        Some(arg_max) => arg_max.saturating_sub(ARGV_HEADROOM_BYTES),
        None => MAX_TOTAL_ENV_BYTES,
    };
    budget.clamp(MIN_ENV_BUDGET_BYTES, MAX_TOTAL_ENV_BYTES)
}

#[cfg(unix)]
fn platform_arg_max() -> Option<usize> {
    // `sysconf(_SC_ARG_MAX)` ≥ 1 MiB on macOS; on Linux it tracks
    // `RLIMIT_STACK`. A negative or failed query falls back to the ceiling.
    match unsafe { libc::sysconf(libc::_SC_ARG_MAX) } {
        n if n > 0 => Some(n as usize),
        _ => None,
    }
}

#[cfg(not(unix))]
fn platform_arg_max() -> Option<usize> {
    None
}

/// The `engine.json` manifest. Written by `scripts/build-engine.sh` (camelCase)
/// and by the installer after a successful install; snake_case aliases are
/// accepted when reading so hand-assembled engines still load.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    /// Engine format version — bumped when the layout changes.
    pub version: String,
    /// `standard` (Node + RSSHub only; browser routes are gated in the
    /// marketplace) or `browser` (also ships a headless Chromium under
    /// `<version dir>/browser` for `requirePuppeteer` routes). Older
    /// manifests predate the field and read as the empty string, which
    /// behaves exactly like `standard`.
    #[serde(default)]
    pub flavor: String,
    /// The RSSHub git revision the build was made from.
    #[serde(alias = "rsshub_revision")]
    pub rsshub_revision: String,
    /// The bundled Node runtime version.
    #[serde(alias = "node_version")]
    pub node_version: String,
    /// When this engine was installed into the local root. Build-time
    /// manifests don't carry it (the installer stamps it), so reading an
    /// archive manifest must tolerate its absence.
    #[serde(default, alias = "installed_at")]
    pub installed_at: String,
}

impl EngineInfo {
    /// True when the installed engine bundles the headless Chromium the
    /// `requirePuppeteer` routes need (`scripts/build-engine.sh --browser`).
    pub fn is_browser_flavor(&self) -> bool {
        self.flavor == "browser"
    }
}

/// What `engine_status` reports to the settings UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    pub installed: bool,
    pub running: bool,
    pub port: Option<u16>,
    pub info: Option<EngineInfo>,
}

/// The live engine process and its idle bookkeeping. `None` when stopped.
struct Running {
    child: Child,
    port: u16,
    last_used: Instant,
    /// The idle watchdog task for this run; aborted/killed with the engine.
    watcher: JoinHandle<()>,
}

/// Manages the installed engine under one root directory and its process.
pub struct EngineManager {
    root: PathBuf,
    running: std::sync::Arc<Mutex<Option<Running>>>,
    /// Serializes cold starts. The `running` slot must not be held across the
    /// up-to-a-minute start budget (the `engine_status` command polls it), so
    /// this separate lock stops concurrent `ensure()` callers — the refresh
    /// leg's tasks and the `rsshub_test_route` command — from each spawning a
    /// full Node process.
    spawn_lock: std::sync::Arc<Mutex<()>>,
}

impl EngineManager {
    /// A manager for `root`. Does not touch the filesystem — the root may not
    /// exist yet (nothing installed).
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            running: std::sync::Arc::new(Mutex::new(None)),
            spawn_lock: std::sync::Arc::new(Mutex::new(())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The installed engine's manifest, or `None` when nothing (valid) is
    /// installed. Small file read, safe to call from sync contexts.
    pub fn info(&self) -> Option<EngineInfo> {
        let raw = std::fs::read_to_string(self.root.join("engine.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn installed(&self) -> bool {
        self.info().is_some()
    }

    /// The path of the installed engine's version directory.
    fn version_dir(info: &EngineInfo, root: &Path) -> AppResult<PathBuf> {
        // The version comes from the (untrusted) archive manifest — sanitize
        // so a crafted `version` cannot escape the root: keep a conservative
        // alphabet and reject anything that smells like a path segment
        // (`..`, `.`).
        let safe: String = info
            .version
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            .collect();
        if safe.is_empty() || safe.contains("..") || safe == "." {
            return Err(AppError::code(CODE_ARCHIVE_INVALID));
        }
        Ok(root.join(safe))
    }

    fn node_bin(dir: &Path) -> PathBuf {
        if cfg!(windows) {
            dir.join("bin").join("node.exe")
        } else {
            dir.join("bin").join("node")
        }
    }

    /// Locate the RSSHub entry script inside the version dir. The tsdown
    /// build emits `app/dist/index.js`; the candidates cover layout drift
    /// between RSSHub revisions without making the manifest carry it.
    fn entry_script(dir: &Path) -> Option<PathBuf> {
        const CANDIDATES: &[&str] = &[
            "app/dist/index.js",
            "app/dist/index.mjs",
            "app/dist/index.cjs",
            "app/lib/index.js",
        ];
        CANDIDATES.iter().map(|c| dir.join(c)).find(|p| p.is_file())
    }

    /// Current status for the settings UI.
    pub async fn status(&self) -> EngineStatus {
        let info = self.info();
        let (running, port) = {
            let mut guard = self.running.lock().await;
            match guard.as_mut() {
                Some(r) => (
                    r.child.try_wait().map(|s| s.is_none()).unwrap_or(false),
                    Some(r.port),
                ),
                None => (false, None),
            }
        };
        EngineStatus {
            installed: info.is_some(),
            running,
            port,
            info,
        }
    }

    /// Make sure the engine is running and return its loopback port.
    ///
    /// `user_env` is the merged environment-variable map from the user's
    /// enabled marketplace routes (see `db::rsshub_enabled_env`); it is applied
    /// on top of the allowlisted host environment at spawn time.
    pub async fn ensure(&self, user_env: &BTreeMap<String, String>) -> AppResult<u16> {
        if let Some(port) = self.try_running().await {
            return Ok(port);
        }
        // Serialize cold starts: concurrent callers (the refresh leg's tasks
        // racing the `rsshub_test_route` command) queue here instead of each
        // spawning a several-hundred-MB Node process. Whoever wakes second
        // re-checks the fast path and reuses the winner's engine.
        let _spawn = self.spawn_lock.lock().await;
        if let Some(port) = self.try_running().await {
            return Ok(port);
        }
        self.cold_start(user_env, START_BUDGET).await
    }

    /// The fast half of [`ensure`](Self::ensure): when a healthy engine is
    /// already running, touch its idle timer and return its port. A dead,
    /// exited or wedged (alive but not answering) engine is torn down and
    /// `None` returned so a cold start can proceed with a clean slot.
    async fn try_running(&self) -> Option<u16> {
        let mut guard = self.running.lock().await;
        let r = guard.as_mut()?;
        let alive = r.child.try_wait().map(|s| s.is_none()).unwrap_or(false);
        if alive && wait_healthy(r.port, PING_BUDGET).await {
            r.last_used = Instant::now();
            return Some(r.port);
        }
        log::info!("rsshub engine not healthy; restarting");
        let dead = guard.take();
        if let Some(mut r) = dead {
            r.watcher.abort();
            let _ = r.child.kill().await;
            // Reap so the killed process cannot linger as a zombie.
            let _ = r.child.wait().await;
        }
        None
    }

    /// The slow path behind [`ensure`](Self::ensure): validate the install,
    /// spawn the process, wait for health. `budget` is a parameter so tests
    /// can exercise the failure path without waiting the full start budget.
    async fn cold_start(
        &self,
        user_env: &BTreeMap<String, String>,
        budget: Duration,
    ) -> AppResult<u16> {
        let info = self
            .info()
            .ok_or_else(|| AppError::code(CODE_ENGINE_MISSING))?;
        let dir = Self::version_dir(&info, &self.root)?;
        let node_bin = Self::node_bin(&dir);
        let entry = Self::entry_script(&dir).ok_or_else(|| AppError::code(CODE_ENGINE_MISSING))?;
        if !node_bin.is_file() {
            log::error!(
                "rsshub engine: node binary missing at {}",
                node_bin.display()
            );
            return Err(AppError::code(CODE_ENGINE_MISSING));
        }

        let port = free_port()
            .map_err(|e| AppError::other(format!("rsshub engine: no free port: {e}")))?;
        // Enforce the execve size budget before touching the log files: an
        // over-budget environment fails the spawn with E2BIG, and the coded
        // error here is the actionable form of that failure.
        let child_env = build_child_env(user_env, port, &dir)?;
        let log_path = self.root.join("engine.log");

        // The log file is opened twice (stdout + stderr share it in append
        // mode), so a run's output interleaves into one chronological tail.
        let log_out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| AppError::other(format!("rsshub engine: open log: {e}")))?;
        let log_err = log_out
            .try_clone()
            .map_err(|e| AppError::other(format!("rsshub engine: open log: {e}")))?;

        let mut cmd = tokio::process::Command::new(&node_bin);
        cmd.current_dir(&dir)
            .arg(entry)
            .env_clear()
            .stdout(std::process::Stdio::from(log_out))
            .stderr(std::process::Stdio::from(log_err))
            .kill_on_drop(true);
        for (k, v) in &child_env {
            cmd.env(k, v);
        }

        log::info!(
            "rsshub engine: starting {} on 127.0.0.1:{port}",
            node_bin.display()
        );
        let child = cmd.spawn().map_err(|e| map_spawn_error(&e))?;

        if wait_healthy(port, budget).await {
            let mut guard = self.running.lock().await;
            // Another ensure() may have raced us to spawn; never leak two
            // processes — kill the loser.
            if let Some(existing) = guard.as_mut() {
                if existing
                    .child
                    .try_wait()
                    .map(|s| s.is_none())
                    .unwrap_or(false)
                {
                    log::info!("rsshub engine: another ensure() already started it");
                    let mut loser = child;
                    let _ = loser.kill().await;
                    existing.last_used = Instant::now();
                    return Ok(existing.port);
                }
            }
            let watcher = spawn_idle_watcher(self.running.clone(), log_path);
            *guard = Some(Running {
                child,
                port,
                last_used: Instant::now(),
                watcher,
            });
            return Ok(port);
        }

        // Start failed — reap the child so it cannot outlive the call, and
        // surface the coded error (the log tail lives in engine.log). The
        // wait mirrors try_running/shutdown: kill alone can leave the
        // process unreaped until the runtime's orphan sweep gets to it.
        let mut child = child;
        let _ = child.kill().await;
        let _ = child.wait().await;
        log::error!("rsshub engine: did not become healthy within budget; see engine.log");
        Err(AppError::code(CODE_ENGINE_START))
    }

    /// Refresh the idle timer after a successful request through the engine.
    pub async fn touch(&self) {
        if let Some(r) = self.running.lock().await.as_mut() {
            r.last_used = Instant::now();
        }
    }

    /// Stop the engine process, if running. Called on app exit.
    pub async fn shutdown(&self) {
        let running = self.running.lock().await.take();
        if let Some(mut r) = running {
            log::info!("rsshub engine: shutting down");
            r.watcher.abort();
            let _ = r.child.kill().await;
            // Reap so the zombie does not linger.
            let _ = r.child.wait().await;
        }
    }

    /// Remove the installed engine (and any staging remnants). Stops a
    /// running engine first.
    pub async fn uninstall(&self) -> AppResult<()> {
        // Serialize against a cold start: it may be mid-health-wait about to
        // register a child under the root this call is about to wipe.
        let _spawn = self.spawn_lock.lock().await;
        self.shutdown().await;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || -> AppResult<()> {
            std::fs::create_dir_all(&root)
                .map_err(|e| AppError::other(format!("rsshub engine: create root: {e}")))?;
            for entry in std::fs::read_dir(&root)
                .map_err(|e| AppError::other(format!("rsshub engine: read root: {e}")))?
            {
                let path = entry
                    .map_err(|e| AppError::other(format!("rsshub engine: read root: {e}")))?
                    .path();
                let keep = matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("engine.log")
                );
                if keep {
                    continue;
                }
                let _ = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
            }
            Ok(())
        })
        .await
        .map_err(|e| AppError::other(format!("rsshub engine: uninstall task: {e}")))?
    }

    /// Install an engine from a local archive (`.tar.gz` / `.tgz` / `.zip`).
    /// Blocking filesystem work runs on the blocking pool; a running engine is
    /// stopped first because its version directory is replaced.
    pub async fn install_from_archive(&self, archive: &Path) -> AppResult<EngineInfo> {
        // Serialize against a cold start: a spawn that is mid-health-wait
        // would otherwise register a child running from the version directory
        // this install is about to replace (a sharing violation on Windows,
        // a zombie engine on Unix).
        let _spawn = self.spawn_lock.lock().await;
        self.shutdown().await;
        let root = self.root.clone();
        let archive = archive.to_path_buf();
        tokio::task::spawn_blocking(move || install_archive_blocking(&root, &archive))
            .await
            .map_err(|e| AppError::other(format!("rsshub engine: install task: {e}")))?
    }

    /// Download an engine archive from `url` (streamed, size-capped), verify
    /// its sha256 when `expected_sha256` is given, then install it.
    pub async fn install_from_url(
        &self,
        url: &str,
        client: &reqwest::Client,
        expected_sha256: Option<&str>,
    ) -> AppResult<EngineInfo> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| AppError::other(format!("rsshub engine: create root: {e}")))?;
        let tmp = self.root.join(format!(".download-{}", std::process::id()));
        let resp = client.get(url).send().await?.error_for_status()?;
        let total = resp.content_length().unwrap_or(0);
        if total > MAX_ARCHIVE_BYTES {
            return Err(AppError::code(CODE_ARCHIVE_INVALID));
        }
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| AppError::other(format!("rsshub engine: download: {e}")))?;
        let mut hasher = Sha256::new();
        let mut downloaded: u64 = 0;
        let mut resp = resp;
        while let Some(chunk) = resp.chunk().await? {
            downloaded += chunk.len() as u64;
            if downloaded > MAX_ARCHIVE_BYTES {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err(AppError::code(CODE_ARCHIVE_INVALID));
            }
            hasher.update(&chunk);
            std::io::copy(&mut chunk.as_ref(), &mut file)
                .map_err(|e| AppError::other(format!("rsshub engine: download: {e}")))?;
        }
        drop(file);
        let result = async {
            if let Some(expected) = expected_sha256 {
                let actual = format!("{:x}", hasher.finalize());
                if !actual.eq_ignore_ascii_case(expected.trim()) {
                    return Err(AppError::code(CODE_ARCHIVE_CHECKSUM));
                }
            }
            self.install_from_archive(&tmp).await
        }
        .await;
        let _ = std::fs::remove_file(&tmp);
        result
    }
}

/// A client for requests to the engine's loopback port (route fetches, health
/// probes). It deliberately bypasses every configured proxy: a loopback
/// address is never reachable *through* a proxy, and a user with `HTTP_PROXY`
/// — or an in-app `net_proxy` setting, which `fetch::build_client` applies to
/// every URL — would otherwise have engine requests leave the machine or fail
/// against the proxy's own error page. The timeout is generous because a
/// route scrape fetches the upstream site inside the request.
pub fn loopback_client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(ENGINE_REQUEST_TIMEOUT)
            .build()
            .expect("failed to build engine loopback client")
    });
    &CLIENT
}

/// Per-request budget for a route fetch through the engine. The request
/// includes the route's own upstream scrape (possibly a slow site), so this
/// is deliberately wider than the feed client's default.
const ENGINE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Assemble the engine child's environment and enforce the execve size
/// budget: allowlisted host variables first, then the user's route variables
/// (winning on collision), then the manager's own fixed variables — which
/// the user's route variables must NOT override (`PORT` must match the port
/// the health check probes, and `LISTEN_INADDR_ANY=0` keeps RSSHub bound to
/// 127.0.0.1 — its default (bind any) would expose the engine, user cookies
/// included, to the whole network; RSSHub's `toBoolean` reads "0" as false
/// and `lib/index.ts` then binds 127.0.0.1. The header size matches RSSHub's
/// own production start command). When the installed engine is the browser
/// flavor (`<version dir>/browser` exists — see `scripts/build-engine.sh
/// --browser`), `PLAYWRIGHT_BROWSERS_PATH` is pinned to that directory so
/// RSSHub's patchright launch resolves the *bundled* Chromium instead of
/// hunting the host's (or trying to download one — `install` was already
/// run at build time, and a runtime download inside the stripped child
/// environment would be a surprise network fetch). Returns the coded
/// [`CODE_ENV_TOO_LARGE`](CODE_ENV_TOO_LARGE) error when the merged size
/// exceeds what this platform's `execve` accepts ([`execve_budget`]).
fn build_child_env(
    user_env: &BTreeMap<String, String>,
    port: u16,
    version_dir: &Path,
) -> AppResult<BTreeMap<String, String>> {
    let mut env: BTreeMap<String, String> = ENV_ALLOWLIST
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_string(), value))
        })
        .collect();
    env.extend(user_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.insert("PORT".into(), port.to_string());
    env.insert("NODE_ENV".into(), "production".into());
    env.insert("LISTEN_INADDR_ANY".into(), "0".into());
    env.insert("NODE_OPTIONS".into(), "--max-http-header-size=32768".into());
    let bundled_browser = version_dir.join("browser");
    if bundled_browser.is_dir() {
        // `to_string_lossy`: an engine root with non-UTF8 bytes is not a
        // thing this code creates, and patchright's env parsing wants a
        // String anyway — the lossy form keeps a pathological root from
        // failing the whole spawn.
        env.insert(
            "PLAYWRIGHT_BROWSERS_PATH".into(),
            bundled_browser.to_string_lossy().into_owned(),
        );
        env.insert("PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD".into(), "1".into());
    }

    // NAME + '=' + value + NUL per entry — the layout execve charges for.
    let budget = execve_budget();
    let total: usize = env.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
    if total > budget {
        log::error!(
            "rsshub engine: merged environment is {total} bytes, over the {budget}-byte budget"
        );
        return Err(AppError::code(CODE_ENV_TOO_LARGE));
    }
    Ok(env)
}

/// Map a spawn failure onto the coded error space: `E2BIG` means the
/// environment/argv budget check above was still too permissive for what
/// this kernel accepts right now (budgets are advisory limits, errno is the
/// truth) — surface the same actionable code instead of a cryptic OS error.
fn map_spawn_error(err: &std::io::Error) -> AppError {
    #[cfg(unix)]
    if err.raw_os_error().is_some_and(|code| code == E2BIG) {
        log::error!("rsshub engine: spawn failed with E2BIG — environment over the kernel's limit");
        return AppError::code(CODE_ENV_TOO_LARGE);
    }
    AppError::other(format!("rsshub engine: spawn: {err}"))
}

/// Poll the engine's health endpoint until it answers or the budget runs out.
///
/// Any HTTP response counts — even a 404 — because the question is "is an
/// HTTP server listening on that port", not "does this exact route exist".
pub(crate) async fn wait_healthy(port: u16, budget: Duration) -> bool {
    let client = loopback_client();
    let url = format!("http://127.0.0.1:{port}/healthz");
    let deadline = Instant::now() + budget;
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        // A short per-request timeout: a probe answers "is a server there",
        // and a wedged process must not hold the probe for the route-fetch
        // budget the client defaults to.
        if let Ok(_resp) = client.get(&url).timeout(PROBE_TIMEOUT).send().await {
            return true;
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Grab a random free loopback port: bind port 0, read the assignment, drop.
pub(crate) fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Idle watchdog: every minute, check `last_used`; after 10 idle minutes kill
/// the process and clear the slot so the next `ensure` cold-starts. Exits on
/// its own once the slot is cleared (including by `shutdown` / `uninstall`).
fn spawn_idle_watcher(
    running: std::sync::Arc<Mutex<Option<Running>>>,
    _log_path: PathBuf,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(IDLE_CHECK_INTERVAL).await;
            let mut guard = running.lock().await;
            match guard.as_mut() {
                Some(r) => {
                    if r.last_used.elapsed() >= IDLE_TIMEOUT {
                        log::info!("rsshub engine: idle for a while — stopping");
                        let r = guard.take();
                        if let Some(mut r) = r {
                            let _ = r.child.kill().await;
                            let _ = r.child.wait().await;
                        }
                        break;
                    }
                }
                None => break,
            }
        }
    })
}

/// Validate an extracted staging tree: the manifest plus a node binary plus
/// the RSSHub entry script. Returns the manifest.
fn validate_staging(staging: &Path) -> AppResult<EngineInfo> {
    let manifest_path = staging.join("engine.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|_| AppError::code(CODE_ARCHIVE_INVALID))?;
    let info: EngineInfo =
        serde_json::from_str(&raw).map_err(|_| AppError::code(CODE_ARCHIVE_INVALID))?;
    let node = if cfg!(windows) {
        staging.join("bin").join("node.exe")
    } else {
        staging.join("bin").join("node")
    };
    // The two executed files must be *regular* files, not symlinks: a link
    // would be followed at spawn/parse time, letting a crafted archive run a
    // host binary instead of the bundled runtime. (`is_file` follows links.)
    let is_regular = |p: &Path| p.symlink_metadata().map(|m| m.is_file()).unwrap_or(false);
    let entry = EngineManager::entry_script(staging);
    if !is_regular(&node) || !entry.as_ref().is_some_and(|p| is_regular(p)) {
        return Err(AppError::code(CODE_ARCHIVE_INVALID));
    }
    // A browser-flavor archive ships its headless Chromium under `browser/`
    // (the directory `build_child_env` pins `PLAYWRIGHT_BROWSERS_PATH` at).
    // Installing one without it would boot fine and then 500 on every
    // `requirePuppeteer` route, so refuse the install instead.
    if info.is_browser_flavor() {
        let browser = staging.join("browser");
        let populated = std::fs::read_dir(&browser)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if !populated {
            return Err(AppError::code(CODE_ARCHIVE_INVALID));
        }
    }
    Ok(info)
}

/// Blocking installer: extract → validate → atomically swap into
/// `<root>/<version>` → write the root manifest.
fn install_archive_blocking(root: &Path, archive: &Path) -> AppResult<EngineInfo> {
    std::fs::create_dir_all(root)
        .map_err(|e| AppError::other(format!("rsshub engine: create root: {e}")))?;
    let staging = root.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| AppError::other(format!("rsshub engine: staging: {e}")))?;

    let result = extract_archive(archive, &staging)
        .and_then(|()| validate_staging(&staging))
        .and_then(|info| {
            let dest = EngineManager::version_dir(&info, root)?;
            // Replace any previous build of the same version atomically
            // enough for our purposes: stage fully, then a single rename.
            if dest.exists() {
                std::fs::remove_dir_all(&dest)
                    .map_err(|e| AppError::other(format!("rsshub engine: replace old: {e}")))?;
            }
            std::fs::rename(&staging, &dest)
                .map_err(|e| AppError::other(format!("rsshub engine: finalize: {e}")))?;
            let manifest = EngineInfo {
                installed_at: chrono::Utc::now().to_rfc3339(),
                ..info
            };
            std::fs::write(
                root.join("engine.json"),
                serde_json::to_vec_pretty(&manifest)
                    .map_err(|e| AppError::other(format!("rsshub engine: manifest: {e}")))?,
            )
            .map_err(|e| AppError::other(format!("rsshub engine: manifest: {e}")))?;
            Ok(manifest)
        });

    if result.is_err() {
        // Never leave a half-extracted staging tree behind.
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// Extract `archive` (tar.gz / tgz / zip, detected by magic bytes then
/// extension) into `dest`, refusing entries that would escape it.
fn extract_archive(archive: &Path, dest: &Path) -> AppResult<()> {
    let mut magic = [0u8; 2];
    let kind = std::fs::File::open(archive)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(|e| AppError::other(format!("rsshub engine: read archive: {e}")))
        .map(|()| match magic {
            [0x1f, 0x8b] => ArchiveKind::TarGz,
            [0x50, 0x4b] => ArchiveKind::Zip,
            _ => ArchiveKind::Unknown,
        })?;
    match kind {
        ArchiveKind::TarGz => extract_tar_gz(archive, dest),
        ArchiveKind::Zip => extract_zip(archive, dest),
        ArchiveKind::Unknown => Err(AppError::code(CODE_ARCHIVE_INVALID)),
    }
}

enum ArchiveKind {
    TarGz,
    Zip,
    Unknown,
}

/// `true` when `candidate` — an archive entry path joined under `dest` —
/// stays inside `dest` (zip-slip / tar traversal guard).
fn stays_within(dest: &Path, candidate: &Path) -> bool {
    candidate.starts_with(dest)
}

/// `true` when a link entry's *target*, resolved lexically against the
/// directory holding the link, stays inside `dest`.
///
/// The engine archive legitimately contains symlinks (pnpm's `node_modules`
/// layout is built from relative links), so links cannot simply be refused —
/// but a link whose target escapes the staging tree would let a crafted
/// archive point `bin/node` or `app/dist/index.js` at any host executable,
/// which the manager then runs. Absolute targets and `..`-escapes are
/// rejected; relative targets that resolve inside the tree pass.
fn link_stays_within(dest: &Path, link_dir: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut resolved = link_dir.to_path_buf();
    for part in target.components() {
        match part {
            std::path::Component::Normal(p) => resolved.push(p),
            std::path::Component::ParentDir => {
                if !resolved.pop() {
                    return false;
                }
            }
            std::path::Component::CurDir => {}
            // A relative link target cannot contain RootDir/Prefix, but if a
            // crafted one does, refuse it outright.
            _ => return false,
        }
    }
    stays_within(dest, &resolved)
}

/// Running total of extracted bytes, shared by both extractors. Refuses a
/// compressed bomb whose declared sizes blow past [`MAX_EXTRACTED_BYTES`].
#[derive(Default)]
struct ExtractBudget(u64);

impl ExtractBudget {
    /// Charge `n` bytes; `false` when the cap is exceeded.
    fn charge(&mut self, n: u64) -> bool {
        self.0 = self.0.saturating_add(n);
        self.0 <= MAX_EXTRACTED_BYTES
    }
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> AppResult<()> {
    let file = std::fs::File::open(archive)
        .map_err(|e| AppError::other(format!("rsshub engine: open archive: {e}")))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    tar.set_preserve_permissions(true);
    let mut budget = ExtractBudget::default();
    for entry in tar
        .entries()
        .map_err(|e| AppError::other(format!("rsshub engine: archive: {e}")))?
    {
        let mut entry =
            entry.map_err(|e| AppError::other(format!("rsshub engine: archive: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| AppError::other(format!("rsshub engine: archive: {e}")))?
            .into_owned();
        let target = dest.join(&path);
        if !stays_within(dest, &target) {
            log::warn!("rsshub engine: skipping archive entry outside root: {path:?}");
            continue;
        }
        // Link entries need their target validated too — `unpack` would
        // happily create a symlink pointing anywhere on the host, and a link
        // on `bin/node` or the entry script would redirect execution. Real
        // archives only ever carry in-tree relative links, so an escaping
        // link fails the install instead of being silently skipped.
        let header_type = entry.header().entry_type();
        if header_type.is_symlink() || header_type.is_hard_link() {
            let link_target = entry
                .link_name()
                .map_err(|e| AppError::other(format!("rsshub engine: archive: {e}")))?
                .ok_or_else(|| AppError::code(CODE_ARCHIVE_INVALID))?;
            let link_dir = target.parent().unwrap_or(dest).to_path_buf();
            if !link_stays_within(dest, &link_dir, &link_target) {
                log::warn!(
                    "rsshub engine: refusing link entry escaping the root: {path:?} -> {link_target:?}"
                );
                return Err(AppError::code(CODE_ARCHIVE_INVALID));
            }
            entry
                .unpack(&target)
                .map_err(|e| AppError::other(format!("rsshub engine: unpack: {e}")))?;
            continue;
        }
        let entry_size = entry
            .header()
            .size()
            .map_err(|e| AppError::other(format!("rsshub engine: archive: {e}")))?;
        if !budget.charge(entry_size) {
            log::warn!("rsshub engine: archive expands past the extraction cap");
            return Err(AppError::code(CODE_ARCHIVE_INVALID));
        }
        entry
            .unpack(&target)
            .map_err(|e| AppError::other(format!("rsshub engine: unpack: {e}")))?;
    }
    Ok(())
}

fn extract_zip(archive: &Path, dest: &Path) -> AppResult<()> {
    let file = std::fs::File::open(archive)
        .map_err(|e| AppError::other(format!("rsshub engine: open archive: {e}")))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| AppError::other(format!("rsshub engine: archive: {e}")))?;
    let mut budget = ExtractBudget::default();
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| AppError::other(format!("rsshub engine: archive: {e}")))?;
        let Some(rel) = entry.enclosed_name() else {
            log::warn!(
                "rsshub engine: skipping unsafe zip entry: {:?}",
                entry.name()
            );
            continue;
        };
        let target = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| AppError::other(format!("rsshub engine: unpack: {e}")))?;
        } else {
            // The zip crate reports the *declared* uncompressed size up front;
            // counting actual copied bytes guards against a lying header.
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AppError::other(format!("rsshub engine: unpack: {e}")))?;
            }
            let mut out = std::fs::File::create(&target)
                .map_err(|e| AppError::other(format!("rsshub engine: unpack: {e}")))?;
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let n = entry
                    .read(&mut chunk)
                    .map_err(|e| AppError::other(format!("rsshub engine: unpack: {e}")))?;
                if n == 0 {
                    break;
                }
                if !budget.charge(n as u64) {
                    log::warn!("rsshub engine: archive expands past the extraction cap");
                    return Err(AppError::code(CODE_ARCHIVE_INVALID));
                }
                out.write_all(&chunk[..n])
                    .map_err(|e| AppError::other(format!("rsshub engine: unpack: {e}")))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("papr-engine-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a valid engine tree and tar.gz it, returning (archive, manifest).
    fn make_targz(root: &Path, node_file_body: &str) -> (PathBuf, EngineInfo) {
        let build = root.join("build");
        let app = build.join("app").join("dist");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(build.join("bin")).unwrap();
        std::fs::write(build.join("bin").join("node"), node_file_body).unwrap();
        std::fs::write(app.join("index.js"), "// entry\n").unwrap();
        let info = EngineInfo {
            version: "1".into(),
            flavor: "standard".into(),
            rsshub_revision: "abcdef123456".into(),
            node_version: "v22.0.0".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
        };
        std::fs::write(
            build.join("engine.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();

        let archive = root.join("engine.tar.gz");
        let tar_gz = std::fs::File::create(&archive).unwrap();
        let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        // Archive entries are relative, rooted at the build dir.
        tar.append_dir_all(".", &build).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        (archive, info)
    }

    fn make_zip(root: &Path) -> PathBuf {
        let build = root.join("build-zip");
        let app = build.join("app").join("dist");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(build.join("bin")).unwrap();
        std::fs::write(build.join("bin").join("node"), "binary").unwrap();
        std::fs::write(app.join("index.js"), "// entry\n").unwrap();
        let info = EngineInfo {
            version: "2".into(),
            flavor: "standard".into(),
            rsshub_revision: "rev".into(),
            node_version: "v22".into(),
            installed_at: "x".into(),
        };
        std::fs::write(
            build.join("engine.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();
        let archive = root.join("engine.zip");
        let file = std::fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = Default::default();
        for rel in ["engine.json", "bin/node", "app/dist/index.js"] {
            zip.start_file(rel, opts).unwrap();
            zip.write_all(&std::fs::read(build.join(rel)).unwrap())
                .unwrap();
        }
        zip.finish().unwrap();
        archive
    }

    #[tokio::test]
    async fn install_from_targz_lays_out_and_reports() {
        let root = temp_root("targz");
        let (archive, built) = make_targz(&root, "binary");
        let mgr = EngineManager::new(root.join("install"));
        let info = mgr.install_from_archive(&archive).await.unwrap();
        // The archive's manifest carries the build data; the installer stamps
        // its own `installed_at`, so that field is expected to differ.
        assert_eq!(info.version, built.version);
        assert_eq!(info.rsshub_revision, built.rsshub_revision);
        assert_eq!(info.node_version, built.node_version);
        assert_ne!(info.installed_at, built.installed_at);
        let stored = mgr.info().unwrap();
        assert_eq!(stored, info);
        let dir = root.join("install").join("1");
        assert!(dir.join("bin").join("node").is_file());
        assert!(dir.join("app").join("dist").join("index.js").is_file());
        assert_eq!(
            EngineManager::entry_script(&dir),
            Some(dir.join("app").join("dist").join("index.js"))
        );
        assert!(mgr.installed());
        mgr.uninstall().await.unwrap();
        assert!(!mgr.installed());
        assert!(!dir.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn install_from_zip_works() {
        let root = temp_root("zip");
        let archive = make_zip(&root);
        let mgr = EngineManager::new(root.join("install"));
        let info = mgr.install_from_archive(&archive).await.unwrap();
        assert_eq!(info.version, "2");
        assert!(
            root.join("install")
                .join("2")
                .join("bin")
                .join("node.exe")
                .is_file()
                || root
                    .join("install")
                    .join("2")
                    .join("bin")
                    .join("node")
                    .is_file()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn install_rejects_garbage_archive() {
        let root = temp_root("garbage");
        let bad = root.join("bad.bin");
        std::fs::write(&bad, b"this is not an archive").unwrap();
        let mgr = EngineManager::new(root.join("install"));
        let err = mgr.install_from_archive(&bad).await.unwrap_err();
        assert_eq!(err.code_str(), CODE_ARCHIVE_INVALID);
        assert!(!mgr.installed());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A staging tree for `validate_staging`: manifest + node + entry, with
    /// the manifest's flavor and an optional populated `browser/` dir.
    fn make_staging(root: &Path, tag: &str, flavor: &str, with_browser: bool) -> PathBuf {
        let staging = root.join(format!("staging-{tag}"));
        let app = staging.join("app").join("dist");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(staging.join("bin")).unwrap();
        std::fs::write(staging.join("bin").join("node"), "binary").unwrap();
        std::fs::write(app.join("index.js"), "// entry\n").unwrap();
        let info = EngineInfo {
            version: "1".into(),
            flavor: flavor.into(),
            rsshub_revision: "rev".into(),
            node_version: "v24".into(),
            installed_at: "x".into(),
        };
        std::fs::write(
            staging.join("engine.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();
        if with_browser {
            // One registry entry — validate_staging only asks that the
            // directory exists and is populated, matching what the
            // `patchright install` step produces.
            std::fs::create_dir_all(staging.join("browser").join("chromium-1")).unwrap();
        }
        staging
    }

    #[test]
    fn validate_staging_requires_browser_dir_for_browser_flavor() {
        let root = temp_root("validate-browser");
        // flavor=browser without a populated browser/ dir: would boot fine
        // and then 500 on every requirePuppeteer route — refuse the install.
        let missing = make_staging(&root, "missing", "browser", false);
        assert_eq!(
            validate_staging(&missing).unwrap_err().code_str(),
            CODE_ARCHIVE_INVALID
        );
        // Populated: accepted.
        let ok = make_staging(&root, "ok", "browser", true);
        assert!(validate_staging(&ok).is_ok());
        // The standard flavor never needs a browser dir (and must not gain
        // the requirement).
        let standard = make_staging(&root, "standard", "standard", false);
        assert!(validate_staging(&standard).is_ok());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn install_replaces_same_version() {
        let root = temp_root("replace");
        let (archive, _) = make_targz(&root, "v1 body");
        let mgr = EngineManager::new(root.join("install"));
        mgr.install_from_archive(&archive).await.unwrap();
        // Rebuild the archive with different content, same version.
        let (archive2, _) = make_targz(&root, "v2 body");
        mgr.install_from_archive(&archive2).await.unwrap();
        let body = std::fs::read_to_string(root.join("install").join("1").join("bin").join("node"))
            .unwrap();
        assert_eq!(body, "v2 body");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn install_from_url_verifies_checksum() {
        let root = temp_root("url");
        let (archive, _) = make_targz(&root, "binary");
        let expected = {
            let mut f = std::fs::File::open(&archive).unwrap();
            let mut hasher = Sha256::new();
            std::io::copy(&mut f, &mut hasher).unwrap();
            format!("{:x}", hasher.finalize())
        };
        let body = std::fs::read(&archive).unwrap();
        let server = tiny_server(body);
        let url = format!("http://127.0.0.1:{}/engine.tar.gz", server.port);
        let client = reqwest::Client::new();
        let mgr = EngineManager::new(root.join("install"));
        // Wrong checksum is refused before extraction.
        let err = mgr
            .install_from_url(&url, &client, Some("deadbeef"))
            .await
            .unwrap_err();
        assert_eq!(err.code_str(), CODE_ARCHIVE_CHECKSUM);
        // Correct checksum installs.
        let info = mgr
            .install_from_url(&url, &client, Some(&expected))
            .await
            .unwrap();
        assert_eq!(info.version, "1");
        server.shutdown();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn wait_healthy_accepts_any_http_response() {
        let server = tiny_server(b"ok".to_vec());
        assert!(wait_healthy(server.port, Duration::from_secs(3)).await);
        server.shutdown();
    }

    #[tokio::test]
    async fn wait_healthy_times_out_on_dead_port() {
        let port = free_port().unwrap();
        assert!(!wait_healthy(port, Duration::from_millis(1200)).await);
    }

    #[tokio::test]
    async fn ensure_fails_coded_when_not_installed() {
        let root = temp_root("notinstalled");
        let mgr = EngineManager::new(root.join("install"));
        let err = mgr.ensure(&BTreeMap::new()).await.unwrap_err();
        assert_eq!(err.code_str(), CODE_ENGINE_MISSING);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn build_child_env_layers_host_user_and_fixed_vars() {
        // The manager's own variables survive a colliding user entry: PORT,
        // NODE_ENV, LISTEN_INADDR_ANY and NODE_OPTIONS are not user-overrideable.
        let mut user = BTreeMap::new();
        user.insert("PORT".to_string(), "99999".to_string());
        user.insert("MY_TOKEN".to_string(), "secret".to_string());
        let dir = temp_root("env-nodir");
        let env = build_child_env(&user, 49152, &dir).unwrap();
        assert_eq!(env.get("PORT").map(String::as_str), Some("49152"));
        assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("production"));
        assert_eq!(env.get("LISTEN_INADDR_ANY").map(String::as_str), Some("0"));
        assert_eq!(env.get("MY_TOKEN").map(String::as_str), Some("secret"));
        // A standard engine (no `browser/` dir) must not advertise a browser
        // registry — patchright would then look for a browser that is not
        // there instead of failing the route cleanly.
        assert!(!env.contains_key("PLAYWRIGHT_BROWSERS_PATH"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn build_child_env_pins_the_bundled_browser_registry() {
        // The browser flavor's `<version dir>/browser` pins
        // PLAYWRIGHT_BROWSERS_PATH on the fixed layer — a user route variable
        // of the same name must not redirect patchright away from the
        // bundled Chromium.
        let dir = temp_root("env-browser");
        std::fs::create_dir_all(dir.join("browser").join("chromium-1")).unwrap();
        let mut user = BTreeMap::new();
        user.insert(
            "PLAYWRIGHT_BROWSERS_PATH".to_string(),
            "/elsewhere".to_string(),
        );
        let env = build_child_env(&user, 49152, &dir).unwrap();
        assert_eq!(
            env.get("PLAYWRIGHT_BROWSERS_PATH").map(String::as_str),
            Some(dir.join("browser").to_str().unwrap())
        );
        assert_eq!(
            env.get("PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD").map(String::as_str),
            Some("1")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn build_child_env_refuses_budget_overrun() {
        // Individually valid entries (each well under the per-value ceiling)
        // must still trip the *total* budget instead of failing execve with
        // E2BIG later.
        let mut user = BTreeMap::new();
        for i in 0..24 {
            user.insert(format!("BULK_VAR_{i}"), "v".repeat(60_000));
        }
        let dir = temp_root("env-budget");
        let err = build_child_env(&user, 49152, &dir).unwrap_err();
        assert_eq!(err.code_str(), CODE_ENV_TOO_LARGE);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn budget_tracks_the_platform_arg_max() {
        // A generous ARG_MAX (macOS: 1 MiB) is capped by the sanity ceiling;
        // Linux's big-stack 2 MiB case likewise.
        assert_eq!(
            budget_for(Some(1024 * 1024)),
            MAX_TOTAL_ENV_BYTES - ARGV_HEADROOM_BYTES
        );
        assert_eq!(budget_for(Some(4 * 1024 * 1024)), MAX_TOTAL_ENV_BYTES);
        // A small RLIMIT_STACK (512 KiB → ARG_MAX 128 KiB on Linux) shrinks
        // the budget instead of letting the spawn fail with E2BIG; the floor
        // keeps it above zero. The Linux kernel floors ARG_MAX at 32 pages
        // (128 KiB), which stays above the floor budget.
        assert_eq!(budget_for(Some(32 * 4096)), MIN_ENV_BUDGET_BYTES);
        // No portable ARG_MAX (non-Unix, or a failed sysconf): the ceiling.
        assert_eq!(budget_for(None), MAX_TOTAL_ENV_BYTES);
        // The live budget never exceeds what this process's execve accepts.
        if let Some(arg_max) = platform_arg_max() {
            assert!(execve_budget() <= arg_max);
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawn_error_maps_e2big_to_the_coded_key() {
        let e2big = std::io::Error::from_raw_os_error(libc::E2BIG);
        assert_eq!(map_spawn_error(&e2big).code_str(), CODE_ENV_TOO_LARGE);
        let other = std::io::Error::from_raw_os_error(libc::EACCES);
        assert_ne!(map_spawn_error(&other).code_str(), CODE_ENV_TOO_LARGE);
        let plain = std::io::Error::other("pathological");
        assert_ne!(map_spawn_error(&plain).code_str(), CODE_ENV_TOO_LARGE);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_reports_start_failure_when_process_dies() {
        // A "node" that exits immediately: the health budget expires and
        // ensure() must surface the coded start failure, not hang.
        let root = temp_root("die");
        let (archive, _) = make_targz(&root, "binary");
        let mgr = EngineManager::new(root.join("install"));
        mgr.install_from_archive(&archive).await.unwrap();
        // Replace the bundled node with a script that exits at once.
        let dir = root.join("install").join("1");
        let node = dir.join("bin").join("node");
        std::fs::write(&node, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = mgr
            .cold_start(&BTreeMap::new(), Duration::from_secs(3))
            .await
            .unwrap_err();
        assert_eq!(err.code_str(), CODE_ENGINE_START);
        assert!(mgr.status().await.port.is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_boots_a_fake_engine_and_reuses_it() {
        // A "node" that listens on $PORT and answers any request: the whole
        // ensure() state machine — spawn, health, reuse — without RSSHub.
        let root = temp_root("fake");
        let (archive, _) = make_targz(&root, "binary");
        let mgr = EngineManager::new(root.join("install"));
        mgr.install_from_archive(&archive).await.unwrap();
        let dir = root.join("install").join("1");
        let node = dir.join("bin").join("node");
        // The fake engine: a shell wrapper around a python one-liner that
        // replies 200 to everything on the port we hand it. macOS/Linux dev
        // machines and CI all ship python3.
        std::fs::write(
            &node,
            "#!/bin/sh\nexec python3 -c \"import os,http.server\n\
             class H(http.server.BaseHTTPRequestHandler):\n\
             \x20 def do_GET(self): self.send_response(200); self.end_headers(); self.wfile.write(b'ok')\n\
             \x20 def log_message(self, *a): pass\n\
             http.server.HTTPServer(('127.0.0.1', int(os.environ['PORT'])), H).serve_forever()\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();

        let port = mgr.ensure(&BTreeMap::new()).await.unwrap();
        assert_eq!(mgr.status().await.port, Some(port));
        // Second ensure reuses the same process (same port), no restart.
        let again = mgr.ensure(&BTreeMap::new()).await.unwrap();
        assert_eq!(again, port);
        mgr.shutdown().await;
        assert!(!mgr.status().await.running);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn version_dir_is_sanitized() {
        let root = Path::new("/tmp/eng");
        let evil = EngineInfo {
            version: "sub/../../../etc".into(),
            flavor: "standard".into(),
            rsshub_revision: "r".into(),
            node_version: "n".into(),
            installed_at: "i".into(),
        };
        // Slashes are dropped, but the `..` sequence is rejected outright.
        assert!(EngineManager::version_dir(&evil, root).is_err());
        let dotdot = EngineInfo {
            version: "..".into(),
            ..evil.clone()
        };
        assert!(EngineManager::version_dir(&dotdot, root).is_err());
        let ok = EngineInfo {
            version: "1.2.3-beta_1".into(),
            ..evil.clone()
        };
        assert_eq!(
            EngineManager::version_dir(&ok, root).unwrap(),
            root.join("1.2.3-beta_1")
        );
    }

    /// A tar.gz containing one symlink entry. `target` is the link's target.
    fn tar_with_symlink(root: &Path, link: &str, target: &str) -> PathBuf {
        let build = root.join("build-link");
        std::fs::create_dir_all(build.join("bin")).unwrap();
        std::fs::write(build.join("bin").join("node"), "binary").unwrap();
        let app = build.join("app").join("dist");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("index.js"), "// entry\n").unwrap();
        let info = EngineInfo {
            version: "1".into(),
            flavor: "standard".into(),
            rsshub_revision: "rev".into(),
            node_version: "v22".into(),
            installed_at: "x".into(),
        };
        std::fs::write(
            build.join("engine.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();

        let archive = root.join(format!("link-{}.tar.gz", link.replace('/', "_")));
        let f = std::fs::File::create(&archive).unwrap();
        let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all(".", &build).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_path(Path::new(link)).unwrap();
        header.set_link_name(Path::new(target)).unwrap();
        header.set_cksum();
        tar.append(&header, std::io::empty()).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        archive
    }

    #[tokio::test]
    async fn install_rejects_symlink_escaping_the_staging_tree() {
        let root = temp_root("symlink-escape");
        let archive = tar_with_symlink(&root, "bin/node-evil", "/etc/passwd");
        let mgr = EngineManager::new(root.join("install"));
        let err = mgr.install_from_archive(&archive).await.unwrap_err();
        assert_eq!(err.code_str(), CODE_ARCHIVE_INVALID);
        // The failing install leaves no half-extracted version dir, so the
        // absolute-target symlink is never live on disk.
        assert!(!root.join("install").join("1").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn install_rejects_dotdot_symlink_targets() {
        let root = temp_root("symlink-dotdot");
        let archive = tar_with_symlink(&root, "app/dist/escape.js", "../../../outside.js");
        let mgr = EngineManager::new(root.join("install"));
        let err = mgr.install_from_archive(&archive).await.unwrap_err();
        assert_eq!(err.code_str(), CODE_ARCHIVE_INVALID);
        // The failing install leaves no half-extracted version dir.
        assert!(!root.join("install").join("1").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn install_keeps_relative_symlinks_inside_the_tree() {
        // Real pnpm archives are full of relative links — they must survive.
        let root = temp_root("symlink-ok");
        let archive = tar_with_symlink(&root, "app/dist/linked.js", "index.js");
        let mgr = EngineManager::new(root.join("install"));
        mgr.install_from_archive(&archive).await.unwrap();
        assert!(root
            .join("install")
            .join("1")
            .join("app")
            .join("dist")
            .join("linked.js")
            .symlink_metadata()
            .is_ok());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn link_guard_decides_lexically() {
        let dest = Path::new("/tmp/eng");
        let inside = dest.join("bin");
        assert!(link_stays_within(dest, &inside, Path::new("node")));
        assert!(link_stays_within(
            dest,
            &inside,
            Path::new("../app/dist/index.js")
        ));
        assert!(!link_stays_within(
            dest,
            &inside,
            Path::new("../../../etc/passwd")
        ));
        // An absolute target is always refused — on every platform.
        assert!(!link_stays_within(dest, &inside, Path::new("/etc/passwd")));
    }

    #[test]
    fn extract_budget_refuses_overruns() {
        let mut b = ExtractBudget::default();
        assert!(b.charge(MAX_EXTRACTED_BYTES));
        assert!(!b.charge(1));
    }

    /// A minimal HTTP server on a random loopback port that answers 200 with
    /// a fixed body — the fake engine for health-check / download tests.
    struct TinyServer {
        port: u16,
        shutdown: std::sync::mpsc::Sender<()>,
        handle: std::thread::JoinHandle<()>,
    }

    impl TinyServer {
        fn shutdown(self) {
            let _ = self.shutdown.send(());
            self.handle.join().ok();
        }
    }

    fn tiny_server(body: Vec<u8>) -> TinyServer {
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            // Poll the listener in non-blocking mode with a stop signal —
            // simple and portable, no async runtime needed here.
            loop {
                if rx.try_recv().is_ok() {
                    return;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 4096];
                        let _ = std::io::Read::read(&mut stream, &mut buf);
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = std::io::Write::write_all(&mut stream, head.as_bytes());
                        let _ = std::io::Write::write_all(&mut stream, &body);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });
        TinyServer {
            port,
            shutdown: tx,
            handle,
        }
    }
}
