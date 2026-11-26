//! The local RSSHub engine — an in-app Bun + official `rsshub@latest` npm
//! package runtime executed on the user's machine, so `rsshub://` subscriptions
//! never depend on a hosted RSSHub instance (public instances rate-limit and
//! blacklist; local execution is the whole point of the feature).
//!
//! The manager is UI-free: the engine root directory is supplied by the
//! caller (the desktop app passes its data dir; tests pass a temp dir).
//!
//! Layout on disk:
//!
//! ```text
//! <engine_root>/engine.json                { version: "2", flavor, rsshubVersion, bunVersion, installedAt }
//! <engine_root>/engine.log                 the engine's stdout/stderr, appended per run
//! <engine_root>/bin/bun                    single-file Bun binary (bin/bun.exe on Windows)
//! <engine_root>/app/package.json           {"name":"papr-rsshub-engine","private":true}
//! <engine_root>/app/server.mjs             boots rsshub/dist-lib/magpie-server.mjs
//! <engine_root>/app/node_modules/rsshub/   official npm package
//! <engine_root>/browser/                   optional headless Chromium via patchright
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
use std::cmp::Ordering;
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
pub const CODE_ENV_TOO_LARGE: &str = "rsshubEngineEnvTooLarge";

/// Engine disk-layout format version ("2" for Bun + npm rsshub layout).
const ENGINE_FORMAT_VERSION: &str = "2";

const NPM_REGISTRY_DEFAULT: &str = "https://registry.npmjs.org";
const NPM_REGISTRY_CN: &str = "https://registry.npmmirror.com";
const BUN_GITHUB_LATEST_URL: &str = "https://github.com/oven-sh/bun/releases/latest";
const BUN_GITHUB_RELEASE_BASE: &str = "https://github.com/oven-sh/bun/releases/latest/download";
const BUN_GITHUB_TAG_DOWNLOAD_BASE: &str = "https://github.com/oven-sh/bun/releases/download";
const BUN_CN_MIRROR_PREFIX: &str = "https://gh-proxy.org/";

const APP_PACKAGE_JSON: &str = "{\"name\":\"papr-rsshub-engine\",\"private\":true}\n";
const APP_SERVER_MJS: &str = r#"import { existsSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
const distLib = path.join(import.meta.dirname, "node_modules", "rsshub", "dist-lib");
const magpieServer = path.join(distLib, "magpie-server.mjs");
if (existsSync(magpieServer)) {
  await import(pathToFileURL(magpieServer).href);
} else {
  throw new Error(`RSSHub server entry not found at ${magpieServer}`);
}
"#;

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
/// Hard cap on a downloaded Bun release zip archive (official zips are ~40-60 MB).
const MAX_BUN_ZIP_BYTES: u64 = 512 * 1024 * 1024;
/// Hard cap on the extracted single `bun` binary (guards against zip bombs).
const MAX_BUN_BINARY_BYTES: u64 = 512 * 1024 * 1024;
/// Hard cap on npm registry JSON metadata responses.
const MAX_REGISTRY_JSON_BYTES: usize = 4 * 1024 * 1024;
/// Hard cap on Bun `SHASUMS256.txt` responses.
const MAX_SHASUMS_BYTES: usize = 256 * 1024;

/// How Bun install/update subprocesses should route outbound network requests,
/// matching Papr's persisted `net_proxy` setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineProxyMode {
    System,
    None,
    Explicit(String),
}

impl EngineProxyMode {
    pub fn from_setting(raw: Option<&str>) -> Self {
        match raw.map(str::trim).unwrap_or("") {
            "" | "system" => Self::System,
            "none" => Self::None,
            custom => Self::Explicit(custom.to_string()),
        }
    }
}

/// Environment variables the engine process may inherit. Everything else is
/// stripped: the child runs arbitrary scraping code with credentials the user
/// typed into the marketplace, so it gets the minimum needed to run Bun and
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
    "NODE_EXTRA_CA_CERTS",
    // Windows basics Bun/Node/Playwright expect.
    "SYSTEMROOT",
    "COMSPEC",
    "PATHEXT",
    "PROGRAMFILES",
    "PROGRAMDATA",
    "SYSTEMDRIVE",
    "HOMEDRIVE",
    "HOMEPATH",
    "USERPROFILE",
    "ALLUSERSPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "WINDIR",
    // Proxy egress.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
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
/// Headroom subtracted from the platform's `ARG_MAX` for argv (the bun
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

/// The `engine.json` manifest. Written atomically by `install_or_update`,
/// `install_browser`, and `uninstall_browser`. Legacy v1 field aliases are
/// accepted when deserializing so stale manifests parse cleanly before being
/// rejected by format version check (`version == "2"`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    /// Engine format version — `"2"` for the Bun + npm `rsshub` layout.
    pub version: String,
    /// `"standard"` (Bun + RSSHub only; browser routes are gated in the
    /// marketplace) or `"browser"` (also ships a headless Chromium under
    /// `<engine_root>/browser` for `requirePuppeteer` routes).
    #[serde(default)]
    pub flavor: String,
    /// Installed `rsshub` npm package version.
    #[serde(alias = "rsshubRevision", alias = "rsshub_revision")]
    pub rsshub_version: String,
    /// Bundled Bun runtime version.
    #[serde(default, alias = "nodeVersion", alias = "node_version")]
    pub bun_version: String,
    /// RFC3339 timestamp when this engine was installed or updated.
    #[serde(default, alias = "installed_at")]
    pub installed_at: String,
}

impl EngineInfo {
    /// True when the installed engine includes headless Chromium under
    /// `<engine_root>/browser` for `requirePuppeteer` routes.
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

/// What `engine_check_update` reports to the settings UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EngineUpdateInfo {
    pub current_version: Option<String>,
    pub latest_version: String,
    pub update_available: bool,
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
    /// Serializes cold starts and install/uninstall mutations. The `running`
    /// slot must not be held across the up-to-a-minute start budget (the
    /// `engine_status` command polls it), so this separate lock stops
    /// concurrent `ensure()` callers from each spawning a Bun process and stops
    /// installs/uninstalls from racing a cold start.
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

    fn bun_filename() -> &'static str {
        if cfg!(windows) {
            "bun.exe"
        } else {
            "bun"
        }
    }

    fn bun_bin(root: &Path) -> PathBuf {
        root.join("bin").join(Self::bun_filename())
    }

    fn app_dir(root: &Path) -> PathBuf {
        root.join("app")
    }

    fn server_mjs(root: &Path) -> PathBuf {
        Self::app_dir(root).join("server.mjs")
    }

    fn rsshub_dir(root: &Path) -> PathBuf {
        Self::app_dir(root).join("node_modules").join("rsshub")
    }

    fn browser_dir(root: &Path) -> PathBuf {
        root.join("browser")
    }

    /// The installed engine's manifest, or `None` when nothing valid (v2 layout)
    /// is installed. Small file read, safe to call from sync contexts.
    pub fn info(&self) -> Option<EngineInfo> {
        let raw = std::fs::read_to_string(self.root.join("engine.json")).ok()?;
        let info: EngineInfo = serde_json::from_str(&raw).ok()?;
        if info.version != ENGINE_FORMAT_VERSION {
            return None;
        }
        if !is_regular_file(&Self::bun_bin(&self.root))
            || !is_regular_file(&Self::server_mjs(&self.root))
            || !Self::rsshub_dir(&self.root).is_dir()
        {
            return None;
        }
        Some(info)
    }

    pub fn installed(&self) -> bool {
        self.info().is_some()
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
    /// spawn `<root>/bin/bun run <root>/app/server.mjs`, wait for health.
    /// `budget` is a parameter so tests can exercise the failure path without
    /// waiting the full start budget.
    async fn cold_start(
        &self,
        user_env: &BTreeMap<String, String>,
        budget: Duration,
    ) -> AppResult<u16> {
        let info = self
            .info()
            .ok_or_else(|| AppError::code(CODE_ENGINE_MISSING))?;
        let bun_bin = Self::bun_bin(&self.root);
        let server_mjs = Self::server_mjs(&self.root);
        let app_dir = Self::app_dir(&self.root);
        if !is_regular_file(&bun_bin) || !is_regular_file(&server_mjs) {
            log::error!(
                "rsshub engine: runtime or entry missing under {}",
                self.root.display()
            );
            return Err(AppError::code(CODE_ENGINE_MISSING));
        }

        let port = free_port()
            .map_err(|e| AppError::other(format!("rsshub engine: no free port: {e}")))?;
        // Enforce the execve size budget before touching the log files.
        let child_env = build_child_env(user_env, port, &self.root, info.is_browser_flavor())?;
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

        let mut cmd = tokio::process::Command::new(&bun_bin);
        cmd.current_dir(&app_dir)
            .arg("run")
            .arg(&server_mjs)
            .env_clear()
            // Keep an open stdin pipe held by `Running.child`: `magpie-server.mjs`
            // exits immediately on stdin EOF/close when `RSSHUB_MAGPIE_HOST=1`,
            // which otherwise kills the engine at launch inside GUI apps whose
            // inherited stdin is `/dev/null`.
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::from(log_out))
            .stderr(std::process::Stdio::from(log_err))
            .kill_on_drop(true);
        for (k, v) in &child_env {
            cmd.env(k, v);
        }

        log::info!(
            "rsshub engine: starting {} on 127.0.0.1:{port}",
            bun_bin.display()
        );
        let child = cmd.spawn().map_err(|e| map_spawn_error(&e))?;

        if wait_healthy(port, budget).await {
            let mut guard = self.running.lock().await;
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

    /// Stop the engine process, if running. Called on app exit and before
    /// install/uninstall/browser mutations.
    pub async fn shutdown(&self) {
        let running = self.running.lock().await.take();
        if let Some(mut r) = running {
            log::info!("rsshub engine: shutting down");
            r.watcher.abort();
            let _ = r.child.kill().await;
            let _ = r.child.wait().await;
        }
    }

    /// Query npm for the latest published `rsshub` version and compare against
    /// the locally installed version.
    pub async fn check_update(
        &self,
        client: &reqwest::Client,
        use_cn_mirror: bool,
    ) -> AppResult<EngineUpdateInfo> {
        let registry = if use_cn_mirror {
            NPM_REGISTRY_CN
        } else {
            NPM_REGISTRY_DEFAULT
        };
        let url = format!("{registry}/rsshub/latest");
        self.check_update_from_url(client, &url).await
    }

    async fn check_update_from_url(
        &self,
        client: &reqwest::Client,
        url: &str,
    ) -> AppResult<EngineUpdateInfo> {
        let mut resp = client.get(url).send().await?.error_for_status()?;
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if buf.len() + chunk.len() > MAX_REGISTRY_JSON_BYTES {
                return Err(AppError::other(
                    "rsshub engine: registry response exceeded size limit",
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct NpmLatest {
            version: String,
        }
        let parsed: NpmLatest = serde_json::from_slice(&buf).map_err(|e| {
            AppError::other(format!("rsshub engine: parse registry metadata: {e}"))
        })?;
        let latest_version = parsed.version.trim().to_string();
        if latest_version.is_empty() {
            return Err(AppError::other(
                "rsshub engine: registry returned empty rsshub version",
            ));
        }
        let current_version = self.info().map(|i| i.rsshub_version);
        let update_available = match current_version.as_deref() {
            None => true,
            Some(cur) => compare_npm_versions(&latest_version, cur) == Ordering::Greater,
        };
        Ok(EngineUpdateInfo {
            current_version,
            latest_version,
            update_available,
        })
    }

    /// Install or update the Bun + `rsshub@latest` engine under `<engine_root>`.
    ///
    /// Installs into an isolated staging directory first and only publishes
    /// the new `app/`, `browser/`, and `engine.json` once validated; legacy v1
    /// directories (including `1/browser`) and any previous working v2 layout
    /// stay untouched if any step fails.
    pub async fn install_or_update(
        &self,
        client: &reqwest::Client,
        use_cn_mirror: bool,
        proxy: &EngineProxyMode,
    ) -> AppResult<EngineInfo> {
        let _spawn = self.spawn_lock.lock().await;
        self.shutdown().await;

        std::fs::create_dir_all(&self.root)
            .map_err(|e| AppError::other(format!("rsshub engine: create root: {e}")))?;

        let prev_info = self.info();
        let had_v2_browser = prev_info.as_ref().is_some_and(|i| i.is_browser_flavor());

        let bun_version = ensure_bun(&self.root, client, use_cn_mirror).await?;
        let bun_bin = Self::bun_bin(&self.root);

        let stamp = format!(
            "{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        );
        let staging_app = self.root.join(format!(".app-staging-{stamp}"));
        let staging_browser = self.root.join(format!(".browser-staging-{stamp}"));
        let _ = std::fs::remove_dir_all(&staging_app);
        let _ = std::fs::remove_dir_all(&staging_browser);

        let stage_result = async {
            std::fs::create_dir_all(&staging_app)
                .map_err(|e| AppError::other(format!("rsshub engine: create staging app: {e}")))?;
            std::fs::write(staging_app.join("package.json"), APP_PACKAGE_JSON)
                .map_err(|e| AppError::other(format!("rsshub engine: write package.json: {e}")))?;

            let registry_arg = format!(
                "--registry={}",
                if use_cn_mirror {
                    NPM_REGISTRY_CN
                } else {
                    NPM_REGISTRY_DEFAULT
                }
            );

            let mut cmd = bun_command(&bun_bin, Some(&staging_app), proxy);
            cmd.args(["add", "rsshub@latest", "--exact", &registry_arg])
                .env("PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD", "1")
                .env("PUPPETEER_SKIP_DOWNLOAD", "1")
                .env("PUPPETEER_SKIP_CHROMIUM_DOWNLOAD", "1");

            log::info!(
                "rsshub engine: installing rsshub@latest in staging via {} ({registry_arg})",
                bun_bin.display()
            );
            let output = cmd
                .output()
                .await
                .map_err(|e| AppError::other(format!("rsshub engine: run bun add: {e}")))?;
            if !output.status.success() {
                return Err(format_cmd_failure("bun add rsshub@latest", &output));
            }

            let staged_server = staging_app.join("server.mjs");
            std::fs::write(&staged_server, APP_SERVER_MJS)
                .map_err(|e| AppError::other(format!("rsshub engine: write server.mjs: {e}")))?;

            let rsshub_version = read_installed_rsshub_version_in(&staging_app)?;
            let staged_entry = staging_app
                .join("node_modules")
                .join("rsshub")
                .join("dist-lib")
                .join("magpie-server.mjs");
            if !is_regular_file(&staged_server) || !is_regular_file(&staged_entry) {
                return Err(AppError::other(
                    "rsshub engine: staged install is missing server entrypoint",
                ));
            }

            let browser_dir = Self::browser_dir(&self.root);
            let v1_browser_dir = self.root.join("1").join("browser");
            let mut browser_plan = BrowserPlan::None;
            let flavor = if browser_dir.exists() {
                if verify_browser_usable(&bun_bin, &staging_app, &browser_dir).await {
                    "browser".to_string()
                } else if had_v2_browser {
                    log::info!(
                        "rsshub engine: existing Chromium needs refresh for updated patchright"
                    );
                    if std::fs::create_dir_all(&staging_browser).is_ok()
                        && run_patchright_install_chromium(
                            &bun_bin,
                            &staging_app,
                            &staging_browser,
                            proxy,
                        )
                        .await
                        .is_ok()
                        && verify_browser_usable(&bun_bin, &staging_app, &staging_browser).await
                    {
                        browser_plan = BrowserPlan::SwapStaging(staging_browser.clone());
                        "browser".to_string()
                    } else {
                        let _ = std::fs::remove_dir_all(&staging_browser);
                        log::warn!(
                            "rsshub engine: existing Chromium is incompatible with updated patchright and automatic refresh failed; please reinstall Chromium in Settings"
                        );
                        browser_plan = BrowserPlan::RemoveExistingAfterPublish;
                        "standard".to_string()
                    }
                } else {
                    browser_plan = BrowserPlan::RemoveExistingAfterPublish;
                    "standard".to_string()
                }
            } else if v1_browser_dir.exists() {
                if verify_browser_usable(&bun_bin, &staging_app, &v1_browser_dir).await {
                    browser_plan = BrowserPlan::MigrateV1(v1_browser_dir);
                    "browser".to_string()
                } else {
                    log::warn!(
                        "rsshub engine: legacy v1 Chromium is incompatible with updated patchright; please reinstall Chromium in Settings"
                    );
                    "standard".to_string()
                }
            } else {
                "standard".to_string()
            };

            let info = EngineInfo {
                version: ENGINE_FORMAT_VERSION.to_string(),
                flavor,
                rsshub_version,
                bun_version,
                installed_at: chrono::Utc::now().to_rfc3339(),
            };
            publish_staged_install(&self.root, &staging_app, browser_plan, &info, &stamp)?;
            Ok(info)
        }
        .await;

        if stage_result.is_err() {
            let _ = std::fs::remove_dir_all(&staging_app);
            let _ = std::fs::remove_dir_all(&staging_browser);
        } else if let Err(e) = cleanup_legacy_layout(&self.root) {
            log::warn!("rsshub engine: post-install legacy cleanup warning: {e}");
        }

        stage_result
    }

    /// Install the optional headless Chromium runtime (`patchright install chromium`)
    /// into `<engine_root>/browser` and mark the engine flavor as `"browser"`.
    pub async fn install_browser(&self, proxy: &EngineProxyMode) -> AppResult<EngineInfo> {
        let _spawn = self.spawn_lock.lock().await;
        self.shutdown().await;

        let mut info = self
            .info()
            .ok_or_else(|| AppError::code(CODE_ENGINE_MISSING))?;
        let bun_bin = Self::bun_bin(&self.root);
        let app_dir = Self::app_dir(&self.root);
        if !is_regular_file(&bun_bin) || !Self::rsshub_dir(&self.root).is_dir() {
            return Err(AppError::code(CODE_ENGINE_MISSING));
        }

        let stamp = format!(
            "{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        );
        let staging_browser = self.root.join(format!(".browser-staging-{stamp}"));
        let _ = std::fs::remove_dir_all(&staging_browser);
        std::fs::create_dir_all(&staging_browser)
            .map_err(|e| AppError::other(format!("rsshub engine: create browser staging: {e}")))?;

        let result = async {
            run_patchright_install_chromium(&bun_bin, &app_dir, &staging_browser, proxy).await?;
            if !verify_browser_usable(&bun_bin, &app_dir, &staging_browser).await {
                return Err(AppError::other(
                    "rsshub engine: patchright install chromium did not produce a usable Chromium binary",
                ));
            }
            info.flavor = "browser".to_string();
            swap_browser_dir_and_manifest(&self.root, &staging_browser, &info, &stamp)?;
            Ok(info)
        }
        .await;

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging_browser);
        }
        result
    }

    /// Remove `<engine_root>/browser` and reset the engine flavor to `"standard"`.
    pub async fn uninstall_browser(&self) -> AppResult<EngineInfo> {
        let _spawn = self.spawn_lock.lock().await;
        self.shutdown().await;

        let mut info = self
            .info()
            .ok_or_else(|| AppError::code(CODE_ENGINE_MISSING))?;
        let browser_dir = Self::browser_dir(&self.root);
        if browser_dir.exists() {
            std::fs::remove_dir_all(&browser_dir)
                .map_err(|e| AppError::other(format!("rsshub engine: remove browser dir: {e}")))?;
        }

        info.flavor = "standard".to_string();
        write_manifest_atomic(&self.root, &info)?;
        Ok(info)
    }

    /// Remove the installed engine (including any legacy v1 directories and
    /// browser runtime). Stops a running engine first and keeps `engine.log`.
    pub async fn uninstall(&self) -> AppResult<()> {
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
}

/// A client for requests to the engine's loopback port (route fetches, health
/// probes). It deliberately bypasses every configured proxy.
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

fn is_regular_file(path: &Path) -> bool {
    path.symlink_metadata().map(|m| m.is_file()).unwrap_or(false)
}

fn is_executable_regular_file(path: &Path) -> bool {
    let Ok(meta) = path.symlink_metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (meta.permissions().mode() & 0o111) != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn dir_is_non_empty(path: &Path) -> bool {
    path.is_dir()
        && std::fs::read_dir(path)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
}

enum BrowserPlan {
    None,
    SwapStaging(PathBuf),
    MigrateV1(PathBuf),
    RemoveExistingAfterPublish,
}

fn publish_staged_install(
    root: &Path,
    staging_app: &Path,
    browser_plan: BrowserPlan,
    info: &EngineInfo,
    stamp: &str,
) -> AppResult<()> {
    let app_dir = EngineManager::app_dir(root);
    let browser_dir = EngineManager::browser_dir(root);
    let backup_app = root.join(format!(".app-backup-{stamp}"));
    let backup_browser = root.join(format!(".browser-backup-{stamp}"));
    let _ = std::fs::remove_dir_all(&backup_app);
    let _ = std::fs::remove_dir_all(&backup_browser);

    let had_app = app_dir.exists();
    if had_app {
        std::fs::rename(&app_dir, &backup_app)
            .map_err(|e| AppError::other(format!("rsshub engine: backup existing app: {e}")))?;
    }

    if let Err(e) = std::fs::rename(staging_app, &app_dir) {
        if had_app {
            let _ = std::fs::rename(&backup_app, &app_dir);
        }
        return Err(AppError::other(format!(
            "rsshub engine: publish staged app: {e}"
        )));
    }

    let mut migrated_from: Option<PathBuf> = None;
    let mut swapped_browser = false;
    let mut remove_browser_after = false;

    let browser_step: AppResult<()> = match browser_plan {
        BrowserPlan::None => Ok(()),
        BrowserPlan::RemoveExistingAfterPublish => {
            remove_browser_after = true;
            Ok(())
        }
        BrowserPlan::SwapStaging(staging_browser) => {
            if browser_dir.exists() {
                std::fs::rename(&browser_dir, &backup_browser).map_err(|e| {
                    AppError::other(format!("rsshub engine: backup existing browser: {e}"))
                })?;
                swapped_browser = true;
            }
            std::fs::rename(&staging_browser, &browser_dir).map_err(|e| {
                AppError::other(format!("rsshub engine: publish staged browser: {e}"))
            })
        }
        BrowserPlan::MigrateV1(v1_browser) => {
            std::fs::rename(&v1_browser, &browser_dir).map_err(|e| {
                AppError::other(format!("rsshub engine: migrate v1 browser: {e}"))
            })?;
            migrated_from = Some(v1_browser);
            Ok(())
        }
    };

    if let Err(e) = browser_step.and_then(|()| write_manifest_atomic(root, info)) {
        if let Some(v1_origin) = migrated_from {
            if browser_dir.exists() {
                let _ = std::fs::rename(&browser_dir, v1_origin);
            }
        } else if swapped_browser {
            let _ = std::fs::remove_dir_all(&browser_dir);
            let _ = std::fs::rename(&backup_browser, &browser_dir);
        }
        let _ = std::fs::remove_dir_all(&app_dir);
        if had_app {
            let _ = std::fs::rename(&backup_app, &app_dir);
        }
        return Err(e);
    }

    if had_app {
        let _ = std::fs::remove_dir_all(&backup_app);
    }
    if swapped_browser {
        let _ = std::fs::remove_dir_all(&backup_browser);
    }
    if remove_browser_after && browser_dir.exists() {
        let _ = std::fs::remove_dir_all(&browser_dir);
    }
    Ok(())
}

fn swap_browser_dir_and_manifest(
    root: &Path,
    staging_browser: &Path,
    info: &EngineInfo,
    stamp: &str,
) -> AppResult<()> {
    let browser_dir = EngineManager::browser_dir(root);
    let backup_browser = root.join(format!(".browser-backup-{stamp}"));
    let _ = std::fs::remove_dir_all(&backup_browser);

    let had_browser = browser_dir.exists();
    if had_browser {
        std::fs::rename(&browser_dir, &backup_browser).map_err(|e| {
            AppError::other(format!("rsshub engine: backup existing browser: {e}"))
        })?;
    }

    if let Err(e) = std::fs::rename(staging_browser, &browser_dir)
        .map_err(|e| AppError::other(format!("rsshub engine: publish staged browser: {e}")))
        .and_then(|()| write_manifest_atomic(root, info))
    {
        let _ = std::fs::remove_dir_all(&browser_dir);
        if had_browser {
            let _ = std::fs::rename(&backup_browser, &browser_dir);
        }
        return Err(e);
    }

    if had_browser {
        let _ = std::fs::remove_dir_all(&backup_browser);
    }
    Ok(())
}

async fn run_patchright_install_chromium(
    bun_bin: &Path,
    app_dir: &Path,
    browser_dir: &Path,
    proxy: &EngineProxyMode,
) -> AppResult<()> {
    // `--bun` forces Bun to execute patchright's `#!/usr/bin/env node` CLI
    // directly without requiring a host `node` installation.
    let mut cmd = bun_command(bun_bin, Some(app_dir), proxy);
    cmd.args(["x", "--bun", "patchright", "install", "chromium"])
        .env("PLAYWRIGHT_BROWSERS_PATH", browser_dir);

    log::info!("rsshub engine: installing patchright chromium");
    let output = cmd
        .output()
        .await
        .map_err(|e| AppError::other(format!("rsshub engine: run patchright install: {e}")))?;
    if !output.status.success() {
        return Err(format_cmd_failure("patchright install chromium", &output));
    }
    Ok(())
}

/// Probe whether `browser_dir` contains a Chromium executable that `patchright`
/// inside `app_dir` can actually resolve and find on disk.
async fn verify_browser_usable(bun_bin: &Path, app_dir: &Path, browser_dir: &Path) -> bool {
    if !dir_is_non_empty(browser_dir) || !is_regular_file(bun_bin) {
        return false;
    }
    let mut cmd = bun_command(bun_bin, Some(app_dir), &EngineProxyMode::None);
    cmd.env("PLAYWRIGHT_BROWSERS_PATH", browser_dir).args([
        "-e",
        "import { chromium } from 'patchright'; import { existsSync } from 'node:fs'; const p = chromium.executablePath(); if (!p || !existsSync(p)) process.exit(1);",
    ]);
    cmd.output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn allowlisted_host_env() -> BTreeMap<String, String> {
    ENV_ALLOWLIST
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_string(), value))
        })
        .collect()
}

fn bun_command(
    bun_bin: &Path,
    cwd: Option<&Path>,
    proxy: &EngineProxyMode,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bun_bin);
    cmd.env_clear().kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in allowlisted_host_env() {
        cmd.env(k, v);
    }
    match proxy {
        EngineProxyMode::System => {}
        EngineProxyMode::None => {
            for k in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
                "PROXY_URI",
            ] {
                cmd.env_remove(k);
            }
        }
        EngineProxyMode::Explicit(url) => {
            cmd.env_remove("PROXY_URI");
            for k in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                cmd.env(k, url);
            }
        }
    }
    cmd
}

fn format_cmd_failure(action: &str, output: &std::process::Output) -> AppError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = match (stderr.is_empty(), stdout.is_empty()) {
        (false, true) => stderr,
        (true, false) => stdout,
        (false, false) => format!("{stderr}\n{stdout}"),
        (true, true) => "no output".to_string(),
    };
    let clipped: String = detail.chars().take(1000).collect();
    AppError::other(format!(
        "rsshub engine: {action} failed ({}): {clipped}",
        output.status
    ))
}

fn cleanup_legacy_layout(root: &Path) -> AppResult<()> {
    if !root.exists() {
        return Ok(());
    }
    // Remove legacy v1 engine.json if present so a failed mid-install never
    // leaves a stale v1 manifest around.
    let manifest = root.join("engine.json");
    if manifest.is_file() {
        let is_v2 = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|s| serde_json::from_str::<EngineInfo>(&s).ok())
            .is_some_and(|i| i.version == ENGINE_FORMAT_VERSION);
        if !is_v2 {
            let _ = std::fs::remove_file(&manifest);
        }
    }

    for entry in std::fs::read_dir(root)
        .map_err(|e| AppError::other(format!("rsshub engine: read root: {e}")))?
    {
        let path = entry
            .map_err(|e| AppError::other(format!("rsshub engine: read root: {e}")))?
            .path();
        let keep = matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("bin" | "app" | "browser" | "engine.json" | "engine.log")
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
}

fn write_manifest_atomic(root: &Path, info: &EngineInfo) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(info)
        .map_err(|e| AppError::other(format!("rsshub engine: serialize manifest: {e}")))?;
    let tmp = root.join(format!(".engine.json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)
        .map_err(|e| AppError::other(format!("rsshub engine: write manifest: {e}")))?;
    let dest = root.join("engine.json");
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AppError::other(format!(
            "rsshub engine: finalize manifest: {e}"
        )));
    }
    Ok(())
}

fn read_installed_rsshub_version_in(app_dir: &Path) -> AppResult<String> {
    let pkg_path = app_dir
        .join("node_modules")
        .join("rsshub")
        .join("package.json");
    let raw = std::fs::read_to_string(&pkg_path).map_err(|e| {
        AppError::other(format!(
            "rsshub engine: read rsshub package.json at {}: {e}",
            pkg_path.display()
        ))
    })?;
    #[derive(Deserialize)]
    struct PkgJson {
        version: String,
    }
    let pkg: PkgJson = serde_json::from_str(&raw)
        .map_err(|e| AppError::other(format!("rsshub engine: parse rsshub package.json: {e}")))?;
    let ver = pkg.version.trim().to_string();
    if ver.is_empty() {
        return Err(AppError::other(
            "rsshub engine: rsshub package.json has empty version",
        ));
    }
    Ok(ver)
}

/// Compare two npm/SemVer strings (`a` vs `b`), supporting RSSHub's
/// `YYYY.MM.DD-build.g<sha>` prerelease tags as well as standard SemVer.
fn compare_npm_versions(a: &str, b: &str) -> Ordering {
    fn strip_prefix_and_build(s: &str) -> &str {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        s.split('+').next().unwrap_or(s)
    }
    fn parse_parts(s: &str) -> (&str, Option<&str>) {
        match s.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre)),
            _ => (s, None),
        }
    }
    fn compare_core(a: &str, b: &str) -> Ordering {
        let a_parts: Vec<&str> = a.split('.').collect();
        let b_parts: Vec<&str> = b.split('.').collect();
        let max_len = a_parts.len().max(b_parts.len());
        let a_nums: Option<Vec<u64>> = a_parts.iter().map(|p| p.parse::<u64>().ok()).collect();
        let b_nums: Option<Vec<u64>> = b_parts.iter().map(|p| p.parse::<u64>().ok()).collect();
        if let (Some(an), Some(bn)) = (a_nums, b_nums) {
            for i in 0..max_len {
                let x = an.get(i).copied().unwrap_or(0);
                let y = bn.get(i).copied().unwrap_or(0);
                match x.cmp(&y) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
            }
            return Ordering::Equal;
        }
        a_parts.cmp(&b_parts)
    }
    fn compare_prerelease(a: &str, b: &str) -> Ordering {
        let a_ids: Vec<&str> = a.split('.').collect();
        let b_ids: Vec<&str> = b.split('.').collect();
        for (x, y) in a_ids.iter().zip(b_ids.iter()) {
            let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        a_ids.len().cmp(&b_ids.len())
    }

    let (ca, pa) = parse_parts(strip_prefix_and_build(a));
    let (cb, pb) = parse_parts(strip_prefix_and_build(b));
    match compare_core(ca, cb) {
        Ordering::Equal => match (pa, pb) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => compare_prerelease(x, y),
        },
        ord => ord,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BunDownloadSource {
    url: String,
    expected_sha256: Option<String>,
}

fn bun_release_asset(os: &str, arch: &str) -> AppResult<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("bun-darwin-aarch64.zip"),
        ("macos", "x86_64") => Ok("bun-darwin-x64.zip"),
        ("linux", "x86_64") => Ok("bun-linux-x64.zip"),
        ("linux", "aarch64") => Ok("bun-linux-aarch64.zip"),
        ("windows", "x86_64") => Ok("bun-windows-x64.zip"),
        _ => Err(AppError::other(format!(
            "rsshub engine: unsupported platform for Bun bootstrap: {os}/{arch}"
        ))),
    }
}

fn extract_bun_tag_from_url(url: &reqwest::Url) -> Option<String> {
    let mut segs = url.path_segments()?;
    while let Some(seg) = segs.next() {
        if seg == "releases" && segs.next() == Some("tag") {
            let tag = segs.next()?.trim();
            if !tag.is_empty() {
                return Some(tag.to_string());
            }
        }
    }
    None
}

fn parse_shasums256(content: &str, asset: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name_raw) = parts.next() else {
            continue;
        };
        let name = name_raw.trim_start_matches('*');
        if name == asset && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn build_bun_download_sources(
    asset: &str,
    use_cn_mirror: bool,
    official_release: Option<(&str, &str)>,
) -> Vec<BunDownloadSource> {
    if let Some((tag, sha256)) = official_release {
        let direct = format!("{BUN_GITHUB_TAG_DOWNLOAD_BASE}/{tag}/{asset}");
        if use_cn_mirror {
            vec![
                BunDownloadSource {
                    url: format!("{BUN_CN_MIRROR_PREFIX}{direct}"),
                    expected_sha256: Some(sha256.to_string()),
                },
                BunDownloadSource {
                    url: direct,
                    expected_sha256: Some(sha256.to_string()),
                },
            ]
        } else {
            vec![BunDownloadSource {
                url: direct,
                expected_sha256: Some(sha256.to_string()),
            }]
        }
    } else {
        // Without an official SHA-256 digest from GitHub, refuse third-party
        // mirrors so an unverified mirror archive is never extracted/executed.
        vec![BunDownloadSource {
            url: format!("{BUN_GITHUB_RELEASE_BASE}/{asset}"),
            expected_sha256: None,
        }]
    }
}

async fn fetch_official_bun_release_digest(
    client: &reqwest::Client,
    asset: &str,
) -> Option<(String, String)> {
    let head_resp = client.head(BUN_GITHUB_LATEST_URL).send().await;
    let tag = match head_resp {
        Ok(r) if r.status().is_success() => extract_bun_tag_from_url(r.url()),
        _ => None,
    };
    let tag = match tag {
        Some(t) => t,
        None => {
            let get_resp = client
                .get(BUN_GITHUB_LATEST_URL)
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?;
            extract_bun_tag_from_url(get_resp.url())?
        }
    };

    let shasums_url = format!("{BUN_GITHUB_TAG_DOWNLOAD_BASE}/{tag}/SHASUMS256.txt");
    let mut resp = client
        .get(&shasums_url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.ok()? {
        if buf.len() + chunk.len() > MAX_SHASUMS_BYTES {
            return None;
        }
        buf.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(buf).ok()?;
    let sha = parse_shasums256(&text, asset)?;
    Some((tag, sha))
}

async fn read_bun_version(bun_bin: &Path) -> AppResult<String> {
    let output = bun_command(bun_bin, None, &EngineProxyMode::None)
        .arg("--version")
        .output()
        .await
        .map_err(|e| AppError::other(format!("rsshub engine: run bun --version: {e}")))?;
    if !output.status.success() {
        return Err(format_cmd_failure("bun --version", &output));
    }
    let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if ver.is_empty() {
        return Err(AppError::other("rsshub engine: bun --version returned empty"));
    }
    Ok(ver)
}

/// Ensure `<root>/bin/bun(.exe)` exists as a regular executable file and return
/// its trimmed `bun --version` string.
async fn ensure_bun(
    root: &Path,
    client: &reqwest::Client,
    use_cn_mirror: bool,
) -> AppResult<String> {
    let bun_bin = EngineManager::bun_bin(root);
    if is_executable_regular_file(&bun_bin) {
        if let Ok(ver) = read_bun_version(&bun_bin).await {
            return Ok(ver);
        }
    }

    let asset = bun_release_asset(std::env::consts::OS, std::env::consts::ARCH)?;
    let official = fetch_official_bun_release_digest(client, asset).await;
    if use_cn_mirror && official.is_none() {
        log::warn!(
            "rsshub engine: could not fetch official Bun SHASUMS256.txt; skipping mirror and downloading directly from GitHub"
        );
    }
    let sources = build_bun_download_sources(
        asset,
        use_cn_mirror,
        official.as_ref().map(|(t, s)| (t.as_str(), s.as_str())),
    );
    ensure_bun_with_sources(root, client, &sources).await
}

async fn ensure_bun_with_sources(
    root: &Path,
    client: &reqwest::Client,
    sources: &[BunDownloadSource],
) -> AppResult<String> {
    let bun_bin = EngineManager::bun_bin(root);
    if is_executable_regular_file(&bun_bin) {
        if let Ok(ver) = read_bun_version(&bun_bin).await {
            return Ok(ver);
        }
    }

    std::fs::create_dir_all(root.join("bin"))
        .map_err(|e| AppError::other(format!("rsshub engine: create bin dir: {e}")))?;

    let expected_name = EngineManager::bun_filename();
    let mut last_err: Option<AppError> = None;

    for (idx, source) in sources.iter().enumerate() {
        let tmp_zip = root.join(format!(
            ".bun-download-{}-{}-{idx}.zip",
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        ));
        let attempt = async {
            download_bun_zip(
                client,
                &source.url,
                &tmp_zip,
                source.expected_sha256.as_deref(),
            )
            .await?;
            let zip_for_extract = tmp_zip.clone();
            let bin_dest = bun_bin.clone();
            tokio::task::spawn_blocking(move || {
                extract_bun_from_zip(&zip_for_extract, &bin_dest, expected_name)
            })
            .await
            .map_err(|e| AppError::other(format!("rsshub engine: extract bun task: {e}")))??;
            if !is_executable_regular_file(&bun_bin) {
                return Err(AppError::other(
                    "rsshub engine: extracted bun binary is not a regular executable",
                ));
            }
            read_bun_version(&bun_bin).await
        }
        .await;

        let _ = std::fs::remove_file(&tmp_zip);
        match attempt {
            Ok(ver) => return Ok(ver),
            Err(err) => {
                log::warn!(
                    "rsshub engine: bun bootstrap from {} failed: {err}",
                    source.url
                );
                last_err = Some(err);
            }
        }
    }

    Err(last_err
        .unwrap_or_else(|| AppError::other("rsshub engine: failed to bootstrap Bun runtime")))
}

async fn download_bun_zip(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_sha256: Option<&str>,
) -> AppResult<()> {
    let mut resp = client.get(url).send().await?.error_for_status()?;
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_BUN_ZIP_BYTES)
    {
        return Err(AppError::other(
            "rsshub engine: bun release archive exceeds size cap",
        ));
    }
    let mut file = std::fs::File::create(dest)
        .map_err(|e| AppError::other(format!("rsshub engine: create temp zip: {e}")))?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    while let Some(chunk) = resp.chunk().await? {
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > MAX_BUN_ZIP_BYTES {
            return Err(AppError::other(
                "rsshub engine: bun release archive exceeds size cap",
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(|e| AppError::other(format!("rsshub engine: write temp zip: {e}")))?;
    }
    file.flush()
        .map_err(|e| AppError::other(format!("rsshub engine: flush temp zip: {e}")))?;
    if let Some(expected) = expected_sha256 {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            let _ = std::fs::remove_file(dest);
            return Err(AppError::other(format!(
                "rsshub engine: bun zip checksum mismatch (expected {expected}, got {actual})"
            )));
        }
    }
    Ok(())
}

/// Extract ONLY the `bun(.exe)` regular file from a Bun release zip into
/// `dest_bin`, enforcing zip-slip/traversal guards, rejecting symlinks, and
/// capping extracted byte count.
fn extract_bun_from_zip(zip_path: &Path, dest_bin: &Path, expected_name: &str) -> AppResult<()> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| AppError::other(format!("rsshub engine: open bun zip: {e}")))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| AppError::other(format!("rsshub engine: invalid bun zip: {e}")))?;

    let bin_dir = dest_bin
        .parent()
        .ok_or_else(|| AppError::other("rsshub engine: invalid bin destination"))?;
    std::fs::create_dir_all(bin_dir)
        .map_err(|e| AppError::other(format!("rsshub engine: create bin dir: {e}")))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| AppError::other(format!("rsshub engine: read zip entry: {e}")))?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(AppError::other(format!(
                "rsshub engine: unsafe path in bun zip: {:?}",
                entry.name()
            )));
        };
        if entry.is_dir() {
            continue;
        }
        let is_symlink = entry.is_symlink()
            || entry
                .unix_mode()
                .is_some_and(|m| (m & 0o170000) == 0o120000);
        if is_symlink {
            continue;
        }
        if rel.file_name().and_then(|n| n.to_str()) != Some(expected_name) {
            continue;
        }
        if entry.size() > MAX_BUN_BINARY_BYTES {
            return Err(AppError::other(
                "rsshub engine: bun binary in zip exceeds size cap",
            ));
        }

        let tmp_bin = bin_dir.join(format!(".bun-tmp-{}", std::process::id()));
        let write_result = (|| -> AppResult<()> {
            let mut out = std::fs::File::create(&tmp_bin)
                .map_err(|e| AppError::other(format!("rsshub engine: create bun binary: {e}")))?;
            let mut buf = [0u8; 64 * 1024];
            let mut total: u64 = 0;
            loop {
                let n = entry
                    .read(&mut buf)
                    .map_err(|e| AppError::other(format!("rsshub engine: read bun binary: {e}")))?;
                if n == 0 {
                    break;
                }
                total = total.saturating_add(n as u64);
                if total > MAX_BUN_BINARY_BYTES {
                    return Err(AppError::other(
                        "rsshub engine: extracted bun binary exceeds size cap",
                    ));
                }
                out.write_all(&buf[..n]).map_err(|e| {
                    AppError::other(format!("rsshub engine: write bun binary: {e}"))
                })?;
            }
            out.flush()
                .map_err(|e| AppError::other(format!("rsshub engine: flush bun binary: {e}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp_bin, std::fs::Permissions::from_mode(0o755)).map_err(
                    |e| AppError::other(format!("rsshub engine: chmod bun binary: {e}")),
                )?;
            }
            if dest_bin.symlink_metadata().is_ok() {
                let _ = std::fs::remove_file(dest_bin);
            }
            std::fs::rename(&tmp_bin, dest_bin)
                .map_err(|e| AppError::other(format!("rsshub engine: place bun binary: {e}")))?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp_bin);
        }
        return write_result;
    }

    Err(AppError::other(format!(
        "rsshub engine: {expected_name} not found in downloaded zip"
    )))
}

/// Assemble the engine child's environment and enforce the execve size
/// budget: allowlisted host variables first, then the user's route variables
/// (winning on collision), then the manager's own fixed variables — which
/// the user's route variables must NOT override (`PORT` must match the port
/// the health check probes, `LISTEN_INADDR_ANY=0` keeps RSSHub bound to
/// 127.0.0.1, and `RSSHUB_MAGPIE_HOST=1` enables the magpie server host mode).
/// When `<engine_root>/browser` is a non-empty directory,
/// `PLAYWRIGHT_BROWSERS_PATH` is pinned to it so `patchright` resolves the
/// installed Chromium instead of hunting the host or attempting a download.
fn build_child_env(
    user_env: &BTreeMap<String, String>,
    port: u16,
    engine_root: &Path,
    is_browser_flavor: bool,
) -> AppResult<BTreeMap<String, String>> {
    let mut env = allowlisted_host_env();
    env.extend(user_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.insert("PORT".into(), port.to_string());
    env.insert("NODE_ENV".into(), "production".into());
    env.insert("LISTEN_INADDR_ANY".into(), "0".into());
    env.insert("RSSHUB_MAGPIE_HOST".into(), "1".into());

    let browser_dir = EngineManager::browser_dir(engine_root);
    if is_browser_flavor && dir_is_non_empty(&browser_dir) {
        env.insert(
            "PLAYWRIGHT_BROWSERS_PATH".into(),
            browser_dir.to_string_lossy().into_owned(),
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
/// this kernel accepts right now — surface the same actionable code instead
/// of a cryptic OS error.
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

    fn make_installed_v2(root: &Path, flavor: &str, rsshub_ver: &str, bun_script: &str) {
        let bin_dir = root.join("bin");
        let app_dir = root.join("app");
        let rsshub_dir = app_dir.join("node_modules").join("rsshub");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&rsshub_dir).unwrap();

        let bun = EngineManager::bun_bin(root);
        std::fs::write(&bun, bun_script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bun, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(app_dir.join("package.json"), APP_PACKAGE_JSON).unwrap();
        std::fs::write(app_dir.join("server.mjs"), APP_SERVER_MJS).unwrap();
        std::fs::write(
            rsshub_dir.join("package.json"),
            format!(r#"{{"name":"rsshub","version":"{rsshub_ver}"}}"#),
        )
        .unwrap();

        if flavor == "browser" {
            std::fs::create_dir_all(root.join("browser").join("chromium-1")).unwrap();
        }

        let info = EngineInfo {
            version: "2".into(),
            flavor: flavor.into(),
            rsshub_version: rsshub_ver.into(),
            bun_version: "1.2.0".into(),
            installed_at: "2026-11-26T00:00:00Z".into(),
        };
        write_manifest_atomic(root, &info).unwrap();
    }

    fn make_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        for (name, content) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn engine_info_deserializes_legacy_aliases_and_camel_case() {
        let json = r#"{
            "version": "1",
            "flavor": "standard",
            "rsshubRevision": "abcdef",
            "nodeVersion": "v22.0.0",
            "installedAt": "2026-01-01T00:00:00Z"
        }"#;
        let info: EngineInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.version, "1");
        assert_eq!(info.rsshub_version, "abcdef");
        assert_eq!(info.bun_version, "v22.0.0");

        let v2 = EngineInfo {
            version: "2".into(),
            flavor: "browser".into(),
            rsshub_version: "2026.11.20".into(),
            bun_version: "1.3.0".into(),
            installed_at: "2026-11-26T00:00:00Z".into(),
        };
        let serialized = serde_json::to_string(&v2).unwrap();
        assert!(serialized.contains("\"rsshubVersion\":\"2026.11.20\""));
        assert!(serialized.contains("\"bunVersion\":\"1.3.0\""));
        assert!(serialized.contains("\"installedAt\":\"2026-11-26T00:00:00Z\""));
        assert!(v2.is_browser_flavor());
    }

    #[tokio::test]
    async fn legacy_v1_layout_is_treated_as_uninstalled_and_cleaned_up() {
        let root = temp_root("v1-cleanup");
        let v1_dir = root.join("1").join("bin");
        std::fs::create_dir_all(&v1_dir).unwrap();
        std::fs::write(v1_dir.join("node"), "old-node").unwrap();
        std::fs::write(root.join("engine.log"), "keep-me\n").unwrap();
        std::fs::write(
            root.join("engine.json"),
            r#"{"version":"1","flavor":"standard","rsshubRevision":"abc","nodeVersion":"v22"}"#,
        )
        .unwrap();

        let mgr = EngineManager::new(root.clone());
        assert!(!mgr.installed());
        assert!(mgr.info().is_none());

        cleanup_legacy_layout(&root).unwrap();
        assert!(!root.join("1").exists());
        assert!(!root.join("engine.json").exists());
        assert!(root.join("engine.log").is_file());

        // Uninstall also removes v1 layouts cleanly while preserving engine.log.
        std::fs::create_dir_all(root.join("1")).unwrap();
        mgr.uninstall().await.unwrap();
        assert!(!root.join("1").exists());
        assert!(root.join("engine.log").is_file());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn extract_bun_from_zip_extracts_only_bun_and_guards_zip_slip() {
        let root = temp_root("zip-extract");
        let zip_path = root.join("bun.zip");
        let dest_bin = root.join("install").join("bin").join(EngineManager::bun_filename());

        let good_zip = make_zip_bytes(&[
            ("bun-darwin-aarch64/README.md", b"ignored"),
            (
                &format!("bun-darwin-aarch64/{}", EngineManager::bun_filename()),
                b"bun-binary-bytes",
            ),
        ]);
        std::fs::write(&zip_path, good_zip).unwrap();
        extract_bun_from_zip(&zip_path, &dest_bin, EngineManager::bun_filename()).unwrap();
        assert_eq!(std::fs::read(&dest_bin).unwrap(), b"bun-binary-bytes");
        assert!(!root.join("install").join("bin").join("README.md").exists());

        // Zip-slip / path traversal entry must be rejected.
        let evil_zip = make_zip_bytes(&[(
            &format!("../../{}", EngineManager::bun_filename()),
            b"evil",
        )]);
        std::fs::write(&zip_path, evil_zip).unwrap();
        let err = extract_bun_from_zip(&zip_path, &dest_bin, EngineManager::bun_filename())
            .unwrap_err();
        assert!(err.to_string().contains("unsafe path"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_bun_reuses_valid_binary_and_rejects_tampered_mirror() {
        let root = temp_root("ensure-bun");
        let install_root = root.join("install");
        std::fs::create_dir_all(install_root.join("bin")).unwrap();

        let fake_bun = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"1.2.3\"; exit 0; fi\n";
        let good_zip_bytes = make_zip_bytes(&[(
            &format!("bun-linux-x64/{}", EngineManager::bun_filename()),
            fake_bun.as_bytes(),
        )]);
        let good_sha = format!("{:x}", Sha256::digest(&good_zip_bytes));

        let tampered_zip_bytes = make_zip_bytes(&[(
            &format!("bun-linux-x64/{}", EngineManager::bun_filename()),
            b"#!/bin/sh\necho evil\n",
        )]);

        let bad_server = TinyServer::new(b"not a zip".to_vec());
        let tampered_server = TinyServer::new(tampered_zip_bytes);
        let good_server = TinyServer::new(good_zip_bytes);

        let client = reqwest::Client::new();
        let ver = ensure_bun_with_sources(
            &install_root,
            &client,
            &[
                BunDownloadSource {
                    url: format!("http://127.0.0.1:{}/mirror-corrupt.zip", bad_server.port),
                    expected_sha256: Some(good_sha.clone()),
                },
                BunDownloadSource {
                    url: format!(
                        "http://127.0.0.1:{}/mirror-tampered.zip",
                        tampered_server.port
                    ),
                    expected_sha256: Some(good_sha.clone()),
                },
                BunDownloadSource {
                    url: format!("http://127.0.0.1:{}/direct.zip", good_server.port),
                    expected_sha256: Some(good_sha),
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(ver, "1.2.3");
        assert!(is_executable_regular_file(&EngineManager::bun_bin(
            &install_root
        )));

        bad_server.shutdown();
        tampered_server.shutdown();
        good_server.shutdown();

        // Second call reuses existing binary without hitting any URL.
        let reused = ensure_bun_with_sources(&install_root, &client, &[])
            .await
            .unwrap();
        assert_eq!(reused, "1.2.3");

        // A symlink at bin/bun must NOT be trusted/reused.
        let bun_path = EngineManager::bun_bin(&install_root);
        let outside = root.join("outside-bun");
        std::fs::write(&outside, fake_bun).unwrap();
        std::fs::remove_file(&bun_path).unwrap();
        std::os::unix::fs::symlink(&outside, &bun_path).unwrap();
        assert!(!is_executable_regular_file(&bun_path));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn compare_npm_versions_orders_semver_and_rsshub_builds() {
        assert_eq!(compare_npm_versions("2.5.0", "2.4.0"), Ordering::Greater);
        assert_eq!(compare_npm_versions("2.4.0", "2.5.0"), Ordering::Less);
        assert_eq!(compare_npm_versions("2.5.0", "2.5.0"), Ordering::Equal);
        assert_eq!(
            compare_npm_versions("2026.11.26-8761.g2b3a546", "2026.11.26-8760.g478e287"),
            Ordering::Greater
        );
        assert_eq!(
            compare_npm_versions("2026.11.26-8760.g478e287", "2026.11.26-8761.g2b3a546"),
            Ordering::Less
        );
        assert_eq!(
            compare_npm_versions("1.0.0", "1.0.0-alpha.1"),
            Ordering::Greater
        );
        assert_eq!(
            compare_npm_versions("1.0.0-alpha.2", "1.0.0-alpha.10"),
            Ordering::Less
        );
    }

    #[tokio::test]
    async fn check_update_compares_installed_against_registry() {
        let root = temp_root("check-update");
        let install_root = root.join("install");
        let server = TinyServer::new(br#"{"name":"rsshub","version":"2.5.0"}"#.to_vec());
        let url = format!("http://127.0.0.1:{}/rsshub/latest", server.port);
        let client = reqwest::Client::new();
        let mgr = EngineManager::new(install_root.clone());

        // Not installed yet -> updateAvailable = true, currentVersion = None.
        let res = mgr.check_update_from_url(&client, &url).await.unwrap();
        assert_eq!(
            res,
            EngineUpdateInfo {
                current_version: None,
                latest_version: "2.5.0".into(),
                update_available: true,
            }
        );

        // Installed older version -> updateAvailable = true.
        make_installed_v2(&install_root, "standard", "2.4.0", "#!/bin/sh\n");
        let res = mgr.check_update_from_url(&client, &url).await.unwrap();
        assert!(res.update_available);
        assert_eq!(res.current_version.as_deref(), Some("2.4.0"));

        // Installed matching version -> updateAvailable = false.
        make_installed_v2(&install_root, "standard", "2.5.0", "#!/bin/sh\n");
        let res = mgr.check_update_from_url(&client, &url).await.unwrap();
        assert!(!res.update_available);
        assert_eq!(res.current_version.as_deref(), Some("2.5.0"));

        // Installed newer version than mirror -> updateAvailable = false (never offer downgrade).
        make_installed_v2(&install_root, "standard", "2.6.0", "#!/bin/sh\n");
        let res = mgr.check_update_from_url(&client, &url).await.unwrap();
        assert!(!res.update_available);
        assert_eq!(res.current_version.as_deref(), Some("2.6.0"));

        server.shutdown();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_install_or_update_preserves_existing_app_and_v1_browser() {
        let root = temp_root("staged-rollback");
        let failing_bun =
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"1.4.5\"; exit 0; fi\nexit 1\n";
        make_installed_v2(&root, "standard", "2.4.0", failing_bun);
        let v1_browser = root.join("1").join("browser").join("chromium-1");
        std::fs::create_dir_all(&v1_browser).unwrap();
        std::fs::write(v1_browser.join("chrome"), "v1-browser").unwrap();

        let mgr = EngineManager::new(root.clone());
        let client = reqwest::Client::new();
        let err = mgr
            .install_or_update(&client, false, &EngineProxyMode::None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bun add rsshub@latest"));

        // Previous working v2 install and v1 browser directory remain untouched.
        assert_eq!(mgr.info().unwrap().rsshub_version, "2.4.0");
        assert!(v1_browser.join("chrome").is_file());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bogus_browser_dir_does_not_mark_engine_as_browser_flavor() {
        let root = temp_root("bogus-browser");
        let fake_bun = "#!/bin/sh\n\
            if [ \"$1\" = \"--version\" ]; then echo \"1.4.5\"; exit 0; fi\n\
            if [ \"$1\" = \"add\" ]; then\n\
              mkdir -p node_modules/rsshub/dist-lib\n\
              echo '{\"name\":\"rsshub\",\"version\":\"2.5.0\"}' > node_modules/rsshub/package.json\n\
              echo 'console.log(1)' > node_modules/rsshub/dist-lib/magpie-server.mjs\n\
              exit 0\n\
            fi\n\
            exit 1\n";
        make_installed_v2(&root, "standard", "2.4.0", fake_bun);
        // Simulate leftover partial/empty-cache files in browser/.
        std::fs::create_dir_all(root.join("browser")).unwrap();
        std::fs::write(root.join("browser").join(".partial"), b"junk").unwrap();

        let mgr = EngineManager::new(root.clone());
        let client = reqwest::Client::new();
        let info = mgr
            .install_or_update(&client, false, &EngineProxyMode::None)
            .await
            .unwrap();
        assert_eq!(info.flavor, "standard");
        assert!(!root.join("browser").exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bun_command_applies_explicit_and_none_proxy_modes() {
        let out_explicit = bun_command(
            Path::new("/usr/bin/env"),
            None,
            &EngineProxyMode::Explicit("http://127.0.0.1:7890".into()),
        )
        .output()
        .await
        .unwrap();
        let env_str = String::from_utf8_lossy(&out_explicit.stdout);
        assert!(env_str.contains("HTTP_PROXY=http://127.0.0.1:7890"));
        assert!(env_str.contains("HTTPS_PROXY=http://127.0.0.1:7890"));
        assert!(env_str.contains("ALL_PROXY=http://127.0.0.1:7890"));

        let out_none = bun_command(Path::new("/usr/bin/env"), None, &EngineProxyMode::None)
            .output()
            .await
            .unwrap();
        let none_str = String::from_utf8_lossy(&out_none.stdout);
        for key in ["HTTP_PROXY=", "HTTPS_PROXY=", "ALL_PROXY=", "http_proxy=", "https_proxy=", "all_proxy=", "PROXY_URI="] {
            assert!(!none_str.contains(key), "unexpected {key} in None mode");
        }
    }

    #[tokio::test]
    async fn uninstall_browser_removes_browser_dir_and_resets_flavor() {
        let root = temp_root("uninstall-browser");
        make_installed_v2(&root, "browser", "2.5.0", "#!/bin/sh\n");
        let mgr = EngineManager::new(root.clone());
        assert!(mgr.info().unwrap().is_browser_flavor());
        assert!(root.join("browser").is_dir());

        let updated = mgr.uninstall_browser().await.unwrap();
        assert_eq!(updated.flavor, "standard");
        assert!(!updated.is_browser_flavor());
        assert!(!root.join("browser").exists());
        assert_eq!(mgr.info().unwrap().flavor, "standard");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn wait_healthy_accepts_any_http_response() {
        let server = TinyServer::new(b"ok".to_vec());
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
        let mut user = BTreeMap::new();
        user.insert("PORT".to_string(), "99999".to_string());
        user.insert("RSSHUB_MAGPIE_HOST".to_string(), "0".to_string());
        user.insert("MY_TOKEN".to_string(), "secret".to_string());
        let dir = temp_root("env-nodir");
        let env = build_child_env(&user, 49152, &dir, false).unwrap();
        assert_eq!(env.get("PORT").map(String::as_str), Some("49152"));
        assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("production"));
        assert_eq!(env.get("LISTEN_INADDR_ANY").map(String::as_str), Some("0"));
        assert_eq!(env.get("RSSHUB_MAGPIE_HOST").map(String::as_str), Some("1"));
        assert_eq!(env.get("MY_TOKEN").map(String::as_str), Some("secret"));
        // Empty or missing browser/ dir must not advertise PLAYWRIGHT_BROWSERS_PATH.
        std::fs::create_dir_all(dir.join("browser")).unwrap();
        let env2 = build_child_env(&user, 49152, &dir, true).unwrap();
        assert!(!env2.contains_key("PLAYWRIGHT_BROWSERS_PATH"));
        // Non-browser flavor must not advertise PLAYWRIGHT_BROWSERS_PATH even if browser/ is non-empty.
        std::fs::create_dir_all(dir.join("browser").join("chromium-1")).unwrap();
        let env3 = build_child_env(&user, 49152, &dir, false).unwrap();
        assert!(!env3.contains_key("PLAYWRIGHT_BROWSERS_PATH"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn build_child_env_pins_the_bundled_browser_registry() {
        let dir = temp_root("env-browser");
        std::fs::create_dir_all(dir.join("browser").join("chromium-1")).unwrap();
        let mut user = BTreeMap::new();
        user.insert(
            "PLAYWRIGHT_BROWSERS_PATH".to_string(),
            "/elsewhere".to_string(),
        );
        let env = build_child_env(&user, 49152, &dir, true).unwrap();
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
        let mut user = BTreeMap::new();
        for i in 0..24 {
            user.insert(format!("BULK_VAR_{i}"), "v".repeat(60_000));
        }
        let dir = temp_root("env-budget");
        let err = build_child_env(&user, 49152, &dir, false).unwrap_err();
        assert_eq!(err.code_str(), CODE_ENV_TOO_LARGE);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn budget_tracks_the_platform_arg_max() {
        assert_eq!(
            budget_for(Some(1024 * 1024)),
            MAX_TOTAL_ENV_BYTES - ARGV_HEADROOM_BYTES
        );
        assert_eq!(budget_for(Some(4 * 1024 * 1024)), MAX_TOTAL_ENV_BYTES);
        assert_eq!(budget_for(Some(32 * 4096)), MIN_ENV_BUDGET_BYTES);
        assert_eq!(budget_for(None), MAX_TOTAL_ENV_BYTES);
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
        let root = temp_root("die");
        make_installed_v2(&root, "standard", "2.0.0", "#!/bin/sh\nexit 1\n");
        let mgr = EngineManager::new(root.clone());

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
        let root = temp_root("fake");
        let fake_bun = "#!/bin/sh\nexec python3 -c \"import os,http.server\n\
             class H(http.server.BaseHTTPRequestHandler):\n\
             \x20 def do_GET(self): self.send_response(200); self.end_headers(); self.wfile.write(b'ok')\n\
             \x20 def log_message(self, *a): pass\n\
             http.server.HTTPServer(('127.0.0.1', int(os.environ['PORT'])), H).serve_forever()\"\n";
        make_installed_v2(&root, "standard", "2.0.0", fake_bun);
        let mgr = EngineManager::new(root.clone());

        let port = mgr.ensure(&BTreeMap::new()).await.unwrap();
        assert_eq!(mgr.status().await.port, Some(port));
        // Second ensure reuses the same process (same port), no restart.
        let again = mgr.ensure(&BTreeMap::new()).await.unwrap();
        assert_eq!(again, port);
        mgr.shutdown().await;
        assert!(!mgr.status().await.running);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_keeps_stdin_open_when_child_exits_on_stdin_eof() {
        let root = temp_root("stdin-eof");
        let fake_bun = "#!/bin/sh\nexec python3 -c \"import os,sys,threading,http.server\n\
             def watch():\n\
             \x20 sys.stdin.read()\n\
             \x20 os._exit(0)\n\
             threading.Thread(target=watch, daemon=True).start()\n\
             class H(http.server.BaseHTTPRequestHandler):\n\
             \x20 def do_GET(self): self.send_response(200); self.end_headers(); self.wfile.write(b'ok')\n\
             \x20 def log_message(self, *a): pass\n\
             http.server.HTTPServer(('127.0.0.1', int(os.environ['PORT'])), H).serve_forever()\"\n";
        make_installed_v2(&root, "standard", "2.0.0", fake_bun);
        let mgr = EngineManager::new(root.clone());

        let port = mgr.ensure(&BTreeMap::new()).await.unwrap();
        assert!(mgr.status().await.running);
        assert_eq!(mgr.status().await.port, Some(port));

        // Remains healthy across repeated ensure calls instead of exiting on stdin EOF.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(mgr.ensure(&BTreeMap::new()).await.unwrap(), port);
        assert!(mgr.status().await.running);

        mgr.shutdown().await;
        assert!(!mgr.status().await.running);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bun_release_asset_and_sources_cover_platforms_and_shasums() {
        assert_eq!(
            bun_release_asset("macos", "aarch64").unwrap(),
            "bun-darwin-aarch64.zip"
        );
        assert_eq!(
            bun_release_asset("macos", "x86_64").unwrap(),
            "bun-darwin-x64.zip"
        );
        assert_eq!(
            bun_release_asset("linux", "x86_64").unwrap(),
            "bun-linux-x64.zip"
        );
        assert_eq!(
            bun_release_asset("linux", "aarch64").unwrap(),
            "bun-linux-aarch64.zip"
        );
        assert_eq!(
            bun_release_asset("windows", "x86_64").unwrap(),
            "bun-windows-x64.zip"
        );
        assert!(bun_release_asset("freebsd", "x86_64").is_err());

        let shasums = "9f5d08118a09fe1b1194ca956c18999fdc81a22f05027add5486ae9a60fc135c  bun-darwin-aarch64-profile.zip\n\
                       6f5a3467ed9caec4795bf78cd476507d9f870c7d57b86c945fcb338126772ffc  bun-darwin-aarch64.zip\n";
        let sha = parse_shasums256(shasums, "bun-darwin-aarch64.zip").unwrap();
        assert_eq!(
            sha,
            "6f5a3467ed9caec4795bf78cd476507d9f870c7d57b86c945fcb338126772ffc"
        );

        let cn_verified =
            build_bun_download_sources("bun-darwin-aarch64.zip", true, Some(("bun-v1.4.5", &sha)));
        assert_eq!(cn_verified.len(), 2);
        assert!(cn_verified[0].url.starts_with(BUN_CN_MIRROR_PREFIX));
        assert_eq!(cn_verified[0].expected_sha256.as_deref(), Some(sha.as_str()));
        assert!(cn_verified[1].url.starts_with(BUN_GITHUB_TAG_DOWNLOAD_BASE));
        assert_eq!(cn_verified[1].expected_sha256.as_deref(), Some(sha.as_str()));

        // Without an official SHA-256, CN mirror must be skipped.
        let cn_unverified = build_bun_download_sources("bun-darwin-aarch64.zip", true, None);
        assert_eq!(cn_unverified.len(), 1);
        assert_eq!(
            cn_unverified[0].url,
            format!("{BUN_GITHUB_RELEASE_BASE}/bun-darwin-aarch64.zip")
        );
        assert_eq!(cn_unverified[0].expected_sha256, None);
    }

    /// A minimal HTTP server on a random loopback port that answers 200 with
    /// a fixed body — the fake engine/registry/mirror for tests.
    struct TinyServer {
        port: u16,
        shutdown: std::sync::mpsc::Sender<()>,
        handle: std::thread::JoinHandle<()>,
    }

    impl TinyServer {
        fn new(body: Vec<u8>) -> Self {
            use std::net::TcpListener;
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let handle = std::thread::spawn(move || {
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

        fn shutdown(self) {
            let _ = self.shutdown.send(());
            self.handle.join().ok();
        }
    }
}
