<div align="center">

<img src="docs/logo.svg" alt="Papr" width="96" height="96" />

# Papr (Aminta Fork)

**A fast, native RSS reader — and a CLI your AI agent can actually drive.**

> [!NOTE]
> **Fork Notice & Maintenance Scope**
> This repository is a personal fork of [Papr](https://github.com/l0ng-ai/papr), created to serve as a self-hosted, independent alternative to proprietary/paid reader features (such as Folo's premium capabilities).
>
> You are very welcome to use, fork, or adapt this project for your own workflow. However, please note that **this fork is maintained strictly for personal use: external issues and pull requests will not be accepted**. If you encounter bugs or wish to propose general features or improvements, please submit them directly to the upstream project at [l0ng-ai/papr](https://github.com/l0ng-ai/papr).

<img src="docs/screenshot.webp" alt="Papr" width="820" />

</div>

## Fork Highlights & Enhancements

This fork enhances Papr with richer export workflows, a Folo-style inline AI reading experience, native social media parsers, an embedded local RSSHub engine, and hardened translation reliability:

### 1. Rich Export Integrations & Custom Webhooks ([#1](https://github.com/AmintaCCCP/papr/pull/1))
- **Built-in Read-Later & Note-Taking Services**: One-click dispatch from the reader toolbar directly into:
  - **Cubox** (via personal API URL)
  - **Readwise Reader** (full document and metadata ingestion)
  - **Instapaper** (HTTP credential bookmarking)
  - **Outline** (document creation within chosen collections)
  - **Readeck** (bookmark and link ingestion)
  - **Zotero** (creates webpage items in user libraries)
  - **Obsidian** (writes Markdown notes with structured YAML frontmatter directly into a local vault)
- **Custom Webhooks & URL Schemes**:
  - Full HTTP method support (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`) with configurable headers and payload templates.
  - Native URI scheme triggering (e.g. `obsidian://new?vault=...`).
  - 8 dynamic template variables: `[title]`, `[url]`, `[content_html]`, `[content_markdown]`, `[summary]`, `[description]`, `[author]`, and `[published_at]`, with automatic URL percent-encoding and JSON-safe string escaping.
- **On-Demand AI Summary Handoff**: Optionally forward the AI summary as the item's summary/description, generating and persisting it on the fly if not yet present.
- **Native Rust Dispatch**: All requests execute in backend Rust, bypassing webview CORS restrictions, honoring system proxies and timeouts, and guarded by protocol blocklists.

### 2. Inline AI Summary Card & Custom Prompt Editor ([#3](https://github.com/AmintaCCCP/papr/pull/3))
- **Folo-Style Inline Card**: Replaced the sliding drawer with an inline summary card positioned naturally between the article header and content, scrolling with the document. Includes one-click Markdown copy, regeneration, and dismissal.
- **Auto-Summarize on Open**: Added an optional setting (`autoSummarize`) to instantly display cached summaries or generate them in the background upon opening an article.
- **Configurable Prompt Editor**: In-app prompt customization with one-click restoration to default, uniformly applied across reader summaries and integration exports.

### 3. Native First-Class X (Twitter) Parser ([#5](https://github.com/AmintaCCCP/papr/pull/5))
- **Zero Third-Party Middlemen**: Subscribes directly to X via official GraphQL endpoints—no external RSSHub instances or intermediate proxies required.
- **Rich Media & Post Fidelity**: Preserves full entity styling (links, @mentions, hashtags), embeds highest-bitrate MP4 videos and images, formats retweets and quote tweets, and derives exact timestamps via Snowflake IDs.
- **Resilient Query ID Rotation**: Automatically extracts and caches GraphQL query IDs from client bundles, with automatic invalidation and retries on 404s.
- **Dedicated Parser Settings**: Manage credentials (`auth_token` and `ct0`) with built-in connection probe diagnostics, plus background rate-limiting (500ms) and per-feed error isolation.

### 3b. Native Xiaohongshu (RED) & Kuaishou Parsers
- **Xiaohongshu, cookie-optional anonymous mode**: Subscribe directly to `xiaohongshu.com/user/profile/<id>`. The parser reads the profile page's server-rendered `__INITIAL_STATE__` — nickname, bio, avatar and the newest ~30 note cards (title, cover, publish time, likes) — using the full Chrome document header group, no login required. Pasting a logged-in `Cookie` (optional) unlocks per-note detail pages: full text, image sets and MP4 video, fetched only for genuinely new notes, throttled with jitter and capped per refresh (failed details fall back to the card and are retried from a pending queue next cycle).
- **Kuaishou, cookie-recommended**: Subscribe to `kuaishou.com/profile/<id>` (www / live / `v.kuaishou.com` share links all resolve). The web API is captcha-gated for anonymous traffic and its signed REST endpoints rely on an in-page JS signer that cannot be reproduced faithfully offline — so the parser runs the same GraphQL operations the web client sends, with a pasted login cookie as the supported credential, and surfaces the captcha as a distinct, localisable error that explains both the cookie path and the RSSHub browser-engine route alternative.
- **Anti-ban posture shared by both**: strictly serial per-site legs, jittered inter-feed pacing, first-page-only reads, read-only requests, hard host allow-lists (session cookies only ever travel in an explicit header to their own site), and upstream risk-control states surfaced as coded, localised per-feed errors instead of silent retries.

### 4. Embedded Local RSSHub Engine & Route Marketplace ([#6](https://github.com/AmintaCCCP/papr/pull/6), [#7](https://github.com/AmintaCCCP/papr/pull/7))
- **Self-Contained Local Runtime**: Bundles a slim Node 24 + RSSHub engine running locally on `127.0.0.1`. Spawns on-demand and idles out after 10 minutes of inactivity, completely bypassing blocked or rate-limited public instances.
- **In-App Route Marketplace**: Browse an offline catalog of 4,000+ routes across 2,000+ namespaces with search, category filtering, and popularity sorting. Features automated parameter forms with pre-filled examples and environment variable configuration.
- **Browser Engine Flavor**: Two engine builds ship from CI — `standard` (Node + RSSHub) and `browser` (`scripts/build-engine.sh --browser`), which additionally bundles RSSHub's own `patchright` runtime plus a headless Chromium. Installing the browser flavor (Settings → Subscriptions → RSSHub local engine, from file or URL) unlocks the marketplace's `Needs browser` routes: the engine pins `PLAYWRIGHT_BROWSERS_PATH` at its bundled Chromium, so anti-bot routes run headlessly on-device with no system Chrome required.
- **Catch-All & Query Handling**: Fully supports greedy/wildcard path parameters (`{:path*}`) and strips query strings/fragments during route matching. Cleans up orphan engine processes on exit and provides clear feedback for uninstalled routes.

### 5. Hardened Keyless Translation Engines & UI Concurrency Fixes ([#2](https://github.com/AmintaCCCP/papr/pull/2), [#4](https://github.com/AmintaCCCP/papr/pull/4))- **Repaired Free Translation Endpoints**:
  - Bing: Migrated to Edge's keyless `translatetext` API, resolving broken token endpoint 404 failures.
  - Google: Switched to Chromium's batch form endpoint (`clients5.google.com`), supporting aligned multi-segment payloads without awkward newline splits.
- **Scoped Viewport Translation**: Scoped list-view auto-translation strictly to the currently opened article instead of queuing all visible list rows, eliminating severe Google 429 rate-limiting.
- **Race Condition Fix**: Eliminated the React Query refetch race condition when switching translation engines in Settings, and added descriptive error messaging per engine.

### 6. Dialog Robustness: IME-Safe Escape & Nested Modals
- **IME Candidate Escape No Longer Closes Dialogs**: Every dialog, popover and the command palette guards Escape with `isComposing` — cancelling a CJK/IME candidate window no longer tears down the dialog the user is typing in (the same guard `Enter` submits already had).
- **Nested-Modal Escape Scoping**: Inner modals (RSSHub env editor, uninstall confirms, parser credential dialogs) own Escape while open; the outer dialog no longer closes both layers at once.

---

Papr is two front-ends over **one local database**:

- **A desktop reader** — fast, native, offline-first. No account, no cloud.
- **`papr`, an agent-facing CLI** — so an autonomous agent (Claude Code, Codex,
  OpenCode…) can read, search and triage your feeds straight from the shell.
  [Jump to it ↓](#papr--your-feeds-handed-to-your-agent)

Both read the same database through the shared `papr-core` crate, so the app and
the CLI can never drift apart.

---

## The desktop app

- **Feeds & folders** — subscribe, organize, and import/export OPML.
- **Smart views** — All, Unread, Starred, and Read Later, with live counts.
- **Tags & rules** — color-coded tags and rules that tag new articles automatically.
- **Full-text** — fetch and clean the complete article when a feed ships only a summary.
- **AI** — summaries, ask-the-article Q&A, and digests. Bring your own API key.
- **Audio** — a built-in player that follows you from article to article.
- **FreshRSS sync** — keep read state in step with a FreshRSS server.
- **Local-first** — everything stays on your machine. No account, no cloud.
- **Localized** — English, Japanese, and Simplified Chinese.

### Install

| Platform | How |
| --- | --- |
| **macOS** | `brew install --cask l0ng-ai/papr/papr` — or grab the `.dmg` |
| **Windows** | Download the `.msi` installer |
| **Linux** | Download the `.AppImage` or `.deb` |

All packages live on the **[latest release](https://github.com/l0ng-ai/papr/releases/latest)**.
The macOS builds are Developer ID signed and notarized.

---

## `papr` — your feeds, handed to your agent

This is what makes Papr different. `papr` is a command-line companion built **for
autonomous agents** to drive over the shell. Point your agent at it and it can
work your feeds with no GUI:

- **Read** — `feeds`, `list`, `read`, full-text `search`
- **Triage** — `mark` read/star/later, `extract` full text, `refresh`
- **Manage** — subscriptions, folders, tags, rules, highlights, OPML
- **Sync** — FreshRSS / Miniflux

Run bare `papr` and it prints your unread dashboard *plus the next useful
commands*, so the agent orients with zero manual. Output is
[TOON](https://toonformat.dev) — ~40% fewer tokens than JSON — with definitive
counts and structured exit codes.

```console
$ papr
unread: 206   starred: 17   later: 0   feeds: 15
articles[10]{id,feed,title,flags,date}:
  3664,V2EX,[Java] 使用 kkRepo 搭建 Maven 私服,unread.star,"2026-06-25"
  ...
help[4]: Run `papr read <id>` to read an article's full text, ...
```

### Hand it to your agent — one line

The bundled **[`papr-rss` skill](skills/papr-rss/SKILL.md)** loads *on demand*
when an agent recognizes a feed-related task, so it costs nothing until you use
it. Install it with [`skills`](https://github.com/vercel-labs/skills):

```sh
npx skills add https://github.com/l0ng-ai/papr/tree/main/skills/papr-rss
```

Want the agent *proactively* aware of your feeds every conversation? `papr setup`
wires up an ambient SessionStart hook (Claude Code, Codex, OpenCode).

### Install the CLI

| Platform | How |
| --- | --- |
| **macOS / Linux** | `brew install l0ng-ai/papr/papr-cli` |
| **Windows** | Download `papr-x86_64-pc-windows-msvc.zip`, unzip, drop `papr.exe` on your `PATH` |
| **Any** | Prebuilt `papr-<target>.tar.gz` from the [latest release](https://github.com/l0ng-ai/papr/releases/latest), or `cargo build --release -p papr-cli` |

> **Full command reference, agent setup, and install options → [docs/cli.md](docs/cli.md)**
