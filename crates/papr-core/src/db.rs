//! SQLite data layer. One file holds feeds, articles, FTS5 index and settings.
//! All SQL lives here; commands call typed functions, never raw SQL.

use crate::error::{AppError, AppResult};
use crate::ingestion::rsshub;
use crate::models::*;
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use rusqlite_migration::{M, Migrations};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

/// Append-only schema migrations. Never edit a shipped migration — add a new one.
/// How many leading characters of an article's `body_text` form the list
/// "snippet". This single source of truth is shared by the list query (which
/// derives `ArticleSummary.snippet`) and the preview-translation command (which
/// translates the same slice), so the translated snippet always corresponds to
/// the original the list would otherwise show.
pub const PREVIEW_SNIPPET_CHARS: usize = 280;

static MIGRATIONS: LazyLock<Migrations> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(
            r#"
        CREATE TABLE folders (
            id        INTEGER PRIMARY KEY,
            name      TEXT NOT NULL,
            position  INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE feeds (
            id              INTEGER PRIMARY KEY,
            feed_url        TEXT NOT NULL UNIQUE,
            site_url        TEXT,
            title           TEXT NOT NULL,
            description     TEXT,
            favicon_url     TEXT,
            folder_id       INTEGER REFERENCES folders(id) ON DELETE SET NULL,
            source_type     TEXT NOT NULL DEFAULT 'rss',
            etag            TEXT,
            last_modified   TEXT,
            last_fetched_at TEXT,
            fetch_error     TEXT,
            created_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE articles (
            id            INTEGER PRIMARY KEY,
            feed_id       INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
            guid          TEXT NOT NULL,
            url           TEXT,
            title         TEXT NOT NULL,
            author        TEXT,
            summary       TEXT,
            content_html  TEXT,
            extracted_html TEXT,
            body_text     TEXT NOT NULL DEFAULT '',
            image_url     TEXT,
            ai_summary    TEXT,
            published_at  TEXT,
            fetched_at    TEXT NOT NULL DEFAULT (datetime('now')),
            is_read       INTEGER NOT NULL DEFAULT 0,
            is_starred    INTEGER NOT NULL DEFAULT 0,
            read_later    INTEGER NOT NULL DEFAULT 0,
            UNIQUE(feed_id, guid)
        );

        CREATE INDEX idx_articles_feed      ON articles(feed_id);
        CREATE INDEX idx_articles_published ON articles(published_at DESC);
        CREATE INDEX idx_articles_unread    ON articles(is_read) WHERE is_read = 0;

        CREATE TABLE enclosures (
            id         INTEGER PRIMARY KEY,
            article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
            url        TEXT NOT NULL,
            mime_type  TEXT,
            length     INTEGER
        );
        CREATE INDEX idx_enclosures_article ON enclosures(article_id);

        CREATE VIRTUAL TABLE articles_fts USING fts5(
            title, body, tokenize = 'porter unicode61'
        );

        -- Keep the FTS index in sync on delete; inserts are handled in code so
        -- that read-state updates do not trigger needless re-indexing.
        CREATE TRIGGER articles_fts_ad AFTER DELETE ON articles BEGIN
            DELETE FROM articles_fts WHERE rowid = old.id;
        END;

        CREATE TABLE settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
        ),
        // v2 — placeholder. An earlier sqlite-vec semantic-search schema was
        // removed; this keeps the version count aligned for databases that
        // already applied it. Search is keyword-only (FTS5).
        M::up("-- semantic search removed; search is FTS5 keyword-only"),
        // v3 — sync support: a remote item id per article plus a small queue
        // of local read/starred changes still to push to the sync server.
        M::up(
            r#"
            ALTER TABLE articles ADD COLUMN remote_id TEXT;
            CREATE TABLE sync_queue (
                article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
                field      TEXT NOT NULL,
                value      INTEGER NOT NULL,
                PRIMARY KEY (article_id, field)
            );
            "#,
        ),
        // v4 — article tags: a flat label set plus an article↔tag join table.
        M::up(
            r#"
            CREATE TABLE tags (
                id        INTEGER PRIMARY KEY,
                name      TEXT NOT NULL UNIQUE,
                color     TEXT NOT NULL DEFAULT 'clay',
                position  INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE article_tags (
                article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
                tag_id     INTEGER NOT NULL REFERENCES tags(id)     ON DELETE CASCADE,
                PRIMARY KEY (article_id, tag_id)
            );
            CREATE INDEX idx_article_tags_tag ON article_tags(tag_id);
            "#,
        ),
        // v5 — filter rules: keyword matches applied to incoming articles to
        // auto-skip noise, or auto mark-read / star them, at ingestion time.
        M::up(
            r#"
            CREATE TABLE rules (
                id         INTEGER PRIMARY KEY,
                name       TEXT NOT NULL,
                enabled    INTEGER NOT NULL DEFAULT 1,
                feed_id    INTEGER REFERENCES feeds(id) ON DELETE CASCADE,
                field      TEXT NOT NULL DEFAULT 'title',
                query      TEXT NOT NULL,
                action     TEXT NOT NULL DEFAULT 'skip',
                position   INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            "#,
        ),
        // v6 — index over the effective article date the list sorts by,
        // COALESCE(published_at, fetched_at), so a dateless entry sorts by
        // when it was fetched instead of sinking below every dated article.
        // (Superseded by v12, which rebuilds this index over a `datetime()`-
        // normalised expression — see that migration for why.)
        M::up(
            "CREATE INDEX idx_articles_sort
             ON articles(COALESCE(published_at, fetched_at) DESC, id DESC);",
        ),
        // v7 — every date ordering now sorts on the effective date and uses
        // idx_articles_sort, so the original published_at-only index is dead
        // weight on each insert. Drop it.
        M::up("DROP INDEX idx_articles_published;"),
        // v8 — partial indexes mirroring idx_articles_unread for the other
        // two smart-view flags, so the Starred / Read-later sidebar counts
        // and list queries use a tiny index instead of a full table scan.
        M::up(
            "CREATE INDEX idx_articles_starred
                 ON articles(is_starred) WHERE is_starred = 1;
             CREATE INDEX idx_articles_readlater
                 ON articles(read_later) WHERE read_later = 1;",
        ),
        // v9 — index the article URL. FreshRSS reconciliation matches remote
        // items to local articles by URL (up to ~1000 lookups per sync) and
        // the dedup check tests URL existence per inserted article; both
        // full-scanned the table without this.
        M::up("CREATE INDEX idx_articles_url ON articles(url);"),
        // v10 — email-newsletter sources (feature F5). A newsletter is a
        // normal `feeds` row (source_type = 'newsletter') so it lists,
        // searches and retains like an RSS feed; this side-table holds the
        // IMAP connection details, keyed 1:1 by feed_id and cascade-deleted
        // with the feed. The app-password is stored in plaintext, the same
        // way FreshRSS sync credentials live in the `settings` table — the
        // database never leaves the user's machine.
        M::up(
            r#"
            CREATE TABLE newsletter_sources (
                feed_id   INTEGER PRIMARY KEY REFERENCES feeds(id) ON DELETE CASCADE,
                host      TEXT NOT NULL,
                port      INTEGER NOT NULL DEFAULT 993,
                username  TEXT NOT NULL,
                password  TEXT NOT NULL,
                folder    TEXT NOT NULL DEFAULT 'INBOX'
            );
            "#,
        ),
        // v11 — highlights / annotations layer (feature F7). Each highlight
        // pins a span of an article's rendered plain text. `text_offset` is
        // the character offset of the quote, and `prefix` / `suffix` carry a
        // short context window for robust re-anchoring when the rendered text
        // shifts (e.g. after full-text extraction replaces a feed snippet).
        // `note` is an optional user annotation; `color` is a palette key.
        M::up(
            r#"
            CREATE TABLE highlights (
                id          INTEGER PRIMARY KEY,
                article_id  INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
                quote       TEXT NOT NULL,
                prefix      TEXT NOT NULL DEFAULT '',
                suffix      TEXT NOT NULL DEFAULT '',
                text_offset INTEGER NOT NULL DEFAULT 0,
                color       TEXT NOT NULL DEFAULT 'yellow',
                note        TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX idx_highlights_article ON highlights(article_id);
            "#,
        ),
        // v12 — rebuild the article-sort index over the *normalised* effective
        // date. `published_at` is RFC 3339 (`2024-01-15T10:30:00+00:00`, the
        // `T`-separated form `to_rfc3339` writes) while `fetched_at` uses
        // SQLite's space-separated form (`2024-01-15 10:30:00`). The old index
        // (v6) ordered on the *raw* `COALESCE(published_at, fetched_at)`, so a
        // string `<` compared the two formats byte-for-byte — and the `T`
        // (0x54) sorts after a space (0x20), making a dated article look up to
        // a day newer than a same-instant dateless one. A list mixing both
        // kinds of rows then came out subtly out of chronological order.
        //
        // Wrapping the effective date in `datetime()` parses both formats to
        // one canonical representation. The ORDER BY clauses are wrapped to
        // match (see `list_articles` / `digest_source` / `preview_rule`); an
        // index on the raw column can't serve a `datetime()`-wrapped sort, so
        // the index expression must be wrapped identically for the planner to
        // keep using it (verified with EXPLAIN QUERY PLAN — no temp B-tree).
        M::up(
            "DROP INDEX idx_articles_sort;
             CREATE INDEX idx_articles_sort
                 ON articles(datetime(COALESCE(published_at, fetched_at)) DESC,
                             id DESC);",
        ),
        // v13 — mark feeds whose title the user has set by hand. A refresh
        // pulls the feed document's own `<title>` and `update_feed_meta`
        // `COALESCE`s it over the stored one, which silently reverted a
        // manual rename on the very next poll. This flag lets `update_feed_meta`
        // leave a user-named feed's title alone while still refreshing every
        // other piece of feed metadata.
        M::up("ALTER TABLE feeds ADD COLUMN custom_title INTEGER NOT NULL DEFAULT 0;"),
        // v14 — cache a translated copy of the article body. `translated_lang`
        // records the target language the cache was produced for, so a later
        // change to the translation-target setting is detected as a cache miss.
        M::up(
            "ALTER TABLE articles ADD COLUMN translated_html TEXT;
             ALTER TABLE articles ADD COLUMN translated_lang TEXT;",
        ),
        // v15 — per-feed refresh interval (minutes). NULL follows the global
        // `refresh_interval_min` setting; the 525_600 sentinel means "never".
        M::up("ALTER TABLE feeds ADD COLUMN refresh_interval_min INTEGER;"),
        // v16 — per-feed auto-translate. 0 (the default) shows the original
        // text; 1 translates an article into the configured target language the
        // moment it is opened.
        M::up("ALTER TABLE feeds ADD COLUMN auto_translate INTEGER NOT NULL DEFAULT 0;"),
        // v17 — independent list-pane translation cache. Preview translations
        // are keyed by article, target language and engine so switching language
        // or engine never overwrites another cached preview. A source title/body
        // update invalidates the derived preview text.
        M::up(
            r#"
            CREATE TABLE article_preview_translations (
                article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
                lang       TEXT NOT NULL,
                engine     TEXT NOT NULL,
                title      TEXT NOT NULL,
                snippet    TEXT NOT NULL,
                PRIMARY KEY(article_id, lang, engine)
            );
            CREATE TRIGGER article_preview_translations_au
            AFTER UPDATE OF title, body_text ON articles BEGIN
                DELETE FROM article_preview_translations WHERE article_id = new.id;
            END;
            "#,
        ),
        // v18 — retention tombstones. A full-archive feed (Hugo's `index.xml`
        // ships the site's entire history) keeps every past item in the feed
        // document forever, so a read article that retention purges is re-fetched
        // on the very next refresh and re-inserted as brand-new *unread* —
        // resurfacing the whole archive the user just cleared, every day the
        // daily cleanup runs (issue #98). `cleanup_old_articles` records each
        // purged article's (feed_id, guid) here and `upsert_article` drops a
        // matching re-fetch, so a retention-purged read article stays gone.
        // Cascade-deletes with the feed.
        M::up(
            r#"
            CREATE TABLE article_tombstones (
                feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                guid    TEXT NOT NULL,
                PRIMARY KEY (feed_id, guid)
            );
            "#,
        ),
        // v19 — per-feed open mode (issue #110): 'reader', 'extracted' or
        // 'web'. NULL (the default) keeps today's behaviour — reader view,
        // honouring the global auto-extract preference.
        M::up("ALTER TABLE feeds ADD COLUMN open_mode TEXT;"),
        // v20/v21 were used by development builds sharing the desktop data
        // directory. Keep their additive columns and version slots so those
        // databases remain readable without resetting or discarding data.
        M::up("ALTER TABLE feeds ADD COLUMN custom_favicon INTEGER NOT NULL DEFAULT 0;"),
        M::up("ALTER TABLE feeds ADD COLUMN position INTEGER NOT NULL DEFAULT 0;"),
        // v22 — installed RSSHub route registry (the route marketplace). One
        // row per route the user installed from the bundled catalog, keyed by
        // the route's template path (`/bilibili/user/video/:id`). `enabled`
        // gates refresh of its subscriptions; `env_json` is the route's
        // environment variables (`{"NCM_COOKIES": "..."}`) merged into the
        // local engine's process environment at spawn. Subscriptions are
        // ordinary feeds rows with `source_type = 'rsshub'`.
        M::up(
            r#"
            CREATE TABLE rsshub_routes (
                route_path   TEXT PRIMARY KEY,
                enabled      INTEGER NOT NULL DEFAULT 1,
                env_json     TEXT NOT NULL DEFAULT '{}',
                installed_at TEXT NOT NULL
            );
            "#,
        ),
        // v23 — cached translated article title, produced alongside the body
        // translation (`translated_html` / `translated_lang`). Same language
        // key as the body: one `ai_translate` run fills both, so `NULL` simply
        // means the row predates title translation (the original title shows).
        M::up("ALTER TABLE articles ADD COLUMN translated_title TEXT;"),
        // v24 — advanced rules. Each rule gains a stored When-document
        // (`conditions`: JSON groups of ANDed conditions, ORed across groups)
        // and a `rewrite_rules` list for the `rewrite` action (literal
        // from→to replacements). The legacy keyword columns stay: rows with a
        // NULL document synthesise one from `field` + `query` at read time,
        // and writes keep those columns filled as a flat, human-readable
        // summary for `sqlite3` inspection (values comma-joined, so a value
        // containing a comma is ambiguous there — informational only). The
        // JSON document is the single source of truth for matching.
        M::up(
            "ALTER TABLE rules ADD COLUMN conditions TEXT;
             ALTER TABLE rules ADD COLUMN rewrite_rules TEXT;",
        ),
        // v25 — global AI chat sessions, message history, and AI briefings.
        M::up(
            r#"
            CREATE TABLE ai_chat_sessions (
                id         TEXT PRIMARY KEY,
                title      TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE ai_chat_messages (
                id            INTEGER PRIMARY KEY,
                session_id    TEXT NOT NULL REFERENCES ai_chat_sessions(id) ON DELETE CASCADE,
                role          TEXT NOT NULL,
                content       TEXT NOT NULL DEFAULT '',
                contexts_json TEXT,
                tools_json    TEXT,
                created_at    TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX idx_ai_chat_messages_session ON ai_chat_messages(session_id, id);
            CREATE TABLE ai_briefings (
                id            INTEGER PRIMARY KEY,
                title         TEXT NOT NULL,
                scope_label   TEXT NOT NULL DEFAULT '',
                days          INTEGER NOT NULL DEFAULT 1,
                article_count INTEGER NOT NULL DEFAULT 0,
                content       TEXT NOT NULL,
                created_at    TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX idx_ai_briefings_created ON ai_briefings(created_at DESC, id DESC);
            "#,
        ),
    ])
});

/// Register Papr's custom SQL scalar functions on a freshly opened connection.
///
/// SQLite's built-in `LOWER()` only case-folds ASCII (it has no Unicode
/// awareness without the ICU extension, which the bundled build omits). Rust's
/// `str::to_lowercase()` is fully Unicode-aware. Anywhere a query needs to
/// match the case-folding the Rust code does — notably `preview_rule`, which
/// must agree with `rule_matches`'s `to_lowercase()` so the rule preview counts
/// exactly the articles live ingestion would act on — `unicode_lower` provides
/// it. SQLite scalar functions are per-connection, so this runs for every
/// connection (the writer and each pooled reader).
fn register_functions(conn: &Connection) -> AppResult<()> {
    conn.create_scalar_function(
        "unicode_lower",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            // A NULL argument folds to NULL; callers `COALESCE` beforehand, but
            // staying total here keeps the function safe to use bare.
            let value: Option<String> = ctx.get(0)?;
            Ok(value.map(|s| s.to_lowercase()))
        },
    )?;
    // `regexp(pattern, text)` backs the rules' `regex` operator on the SQL
    // side (`preview_rule` / `apply_rule_to_existing`). Compilation goes
    // through the same memoised `RuleCondition::compile` the ingestion matcher
    // uses, so a pattern means exactly the same thing in both places; an
    // invalid pattern yields false (never matches) rather than an error — the
    // matcher treats it the same way, and save-time validation rejects it.
    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            // NULL in → false out, both for the pattern and the text: an
            // unusable input "never matches", mirroring how an invalid
            // pattern behaves below.
            let pattern: Option<String> = ctx.get(0)?;
            let text: Option<String> = ctx.get(1)?;
            let (Some(pattern), Some(text)) = (pattern, text) else {
                return Ok(false);
            };
            Ok(RuleCondition::compile(&pattern).is_some_and(|re| re.is_match(&text)))
        },
    )?;
    Ok(())
}

/// Open the writer connection: run migrations and set the write-side pragmas.
/// WAL mode is persisted in the database header, so reader connections opened
/// afterwards inherit it automatically.
pub fn open(path: &Path) -> AppResult<Connection> {
    let mut conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    MIGRATIONS.to_latest(&mut conn)?;
    register_functions(&conn)?;
    Ok(conn)
}

/// Open a read-only connection for the UI query pool. Under WAL these run
/// concurrently with the writer, so interface reads never block on a
/// background refresh. `query_only` is a safety net against an accidental
/// write on a pooled reader. Must be called after `open` has migrated.
pub fn open_reader(path: &Path) -> AppResult<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "query_only", true)?;
    register_functions(&conn)?;
    Ok(conn)
}

// ─────────────────────────── folders ───────────────────────────

pub fn list_folders(conn: &Connection) -> AppResult<Vec<Folder>> {
    let mut stmt =
        conn.prepare("SELECT id, name, position FROM folders ORDER BY position, name")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Folder {
                id: r.get(0)?,
                name: r.get(1)?,
                position: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Create a folder, or return the existing one when a folder with the same
/// name (case-insensitively) is already present.
///
/// `folders.name` carries no `UNIQUE` constraint, so without this guard two
/// folders named "Tech" — or "Tech" and "tech" — could coexist, leaving the
/// sidebar with confusing near-duplicates. Idempotent-on-name is also exactly
/// what `folder_id_by_name` (and so OPML import) wants: a feed nested under a
/// folder whose name already exists must land in *that* folder rather than a
/// freshly created twin. This mirrors `create_tag`'s case-insensitive dedup.
pub fn create_folder(conn: &Connection, name: &str) -> AppResult<i64> {
    // Trim before the dedup lookup and the insert: a name carrying surrounding
    // whitespace (a pasted OPML `<outline text=" Tech ">`, an accidental
    // trailing space) is a different string from its trimmed twin, so the
    // `COLLATE NOCASE` lookup below would miss the existing folder and spawn
    // the near-duplicate the dedup exists to prevent. Normalising here — the
    // one chokepoint every caller (UI prompt, OPML import) funnels through —
    // keeps the invariant independent of any caller-side trimming.
    let name = name.trim();
    // Reject an empty/whitespace-only name at the same chokepoint. The
    // `PromptDialog` guards the interactive path, but `import_opml` reaches
    // this through `folder_id_by_name` with no such guard: an OPML folder
    // outline labelled with only whitespace (or an empty `text`/`title`
    // attribute) would otherwise insert a blank-named folder into the sidebar,
    // indistinguishable from a glitch and impossible to tell apart from any
    // other blank folder. Mirrors the guard `rename_feed` already applies.
    if name.is_empty() {
        return Err(AppError::code("emptyFolderName"));
    }
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM folders WHERE name = ?1 COLLATE NOCASE",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO folders(name, position) VALUES (?1, (SELECT COALESCE(MAX(position),0)+1 FROM folders))",
        params![name],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Rename a folder, rejecting a name that collides with a *different* folder.
///
/// `create_folder` collapses same-name folders, so a rename onto an existing
/// folder's name (or a case variant of it) would otherwise recreate exactly
/// the near-duplicate that dedup prevents — leaving the two functions
/// inconsistent. Match case-insensitively and return the localisable
/// `folderNameExists` code. Renaming a folder to its own name (or a case
/// change of it) is allowed. Mirrors `rename_tag`.
pub fn rename_folder(conn: &Connection, id: i64, name: &str) -> AppResult<()> {
    // Trim so the collision check and the stored value match what `create_folder`
    // would produce — otherwise a rename to `" Tech "` slips past the clash test
    // against an existing `"Tech"` and recreates the near-duplicate.
    let name = name.trim();
    // Reject an empty/whitespace-only name, the same guard `create_folder` and
    // `rename_feed` apply: a rename to a blank string would leave the folder
    // unlabelled in the sidebar with no recovery path short of renaming it
    // again to something valid.
    if name.is_empty() {
        return Err(AppError::code("emptyFolderName"));
    }
    let clash: Option<i64> = conn
        .query_row(
            "SELECT id FROM folders WHERE name = ?1 COLLATE NOCASE AND id != ?2",
            params![name, id],
            |r| r.get(0),
        )
        .optional()?;
    if clash.is_some() {
        return Err(AppError::code("folderNameExists"));
    }
    conn.execute(
        "UPDATE folders SET name = ?2 WHERE id = ?1",
        params![id, name],
    )?;
    Ok(())
}

pub fn delete_folder(conn: &Connection, id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM folders WHERE id = ?1", params![id])?;
    Ok(())
}

// ─────────────────────────── feeds ───────────────────────────

pub fn find_feed_by_url(conn: &Connection, url: &str) -> AppResult<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM feeds WHERE feed_url = ?1",
            params![url],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn insert_feed(
    conn: &Connection,
    feed_url: &str,
    site_url: Option<&str>,
    title: &str,
    description: Option<&str>,
    source_type: SourceType,
    folder_id: Option<i64>,
) -> AppResult<i64> {
    conn.execute(
        "INSERT INTO feeds(feed_url, site_url, title, description, source_type, folder_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            feed_url,
            site_url,
            title,
            description,
            source_type.as_str(),
            folder_id
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Promote a feed's `source_type` once its real kind is known from the parsed
/// document — but only when it is still the generic `'rss'`.
///
/// `add_feed` classifies a feed precisely (it has the parsed feed in hand and
/// runs `parse::refine_source_type`), but `import_opml` can only call
/// `parse::detect_source_type`, which inspects the URL alone and so cannot see
/// that a feed is a podcast (audio enclosures) or a Mastodon timeline. An
/// OPML-imported podcast therefore stays mislabelled `'rss'` forever, losing
/// its source badge and podcast-specific UI. The refresh loop calls this on
/// every successful fetch to correct such feeds from their first poll onward.
///
/// The `WHERE source_type = 'rss'` guard makes this strictly a promotion: a
/// feed already classified (youtube / bluesky / podcast / mastodon / reddit /
/// newsletter) is never touched, so a re-poll cannot demote or churn the type.
pub fn refine_feed_source_type(
    conn: &Connection,
    id: i64,
    source_type: SourceType,
) -> AppResult<()> {
    if source_type == SourceType::Rss {
        return Ok(());
    }
    conn.execute(
        "UPDATE feeds SET source_type = ?2
         WHERE id = ?1 AND source_type = 'rss'",
        params![id, source_type.as_str()],
    )?;
    Ok(())
}

/// The feed columns every feed-row query reads, in `map_feed_row` order —
/// shared by the whole-library list and the single-feed lookup so the two can
/// never drift.
const FEED_COLUMNS: &str = "\
    SELECT f.id, f.feed_url, f.site_url, f.title, f.description, f.favicon_url, \
           f.folder_id, f.source_type, f.last_fetched_at, f.fetch_error, \
           (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id AND a.is_read = 0), \
           f.refresh_interval_min, f.auto_translate, f.open_mode \
    FROM feeds f";

pub fn list_feeds(conn: &Connection) -> AppResult<Vec<Feed>> {
    let mut stmt = conn.prepare(&format!("{FEED_COLUMNS} ORDER BY f.title COLLATE NOCASE"))?;
    let rows = stmt
        .query_map([], map_feed_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One feed by id, or `None` when the id is unknown (deleted concurrently).
pub fn feed_by_id(conn: &Connection, id: i64) -> AppResult<Option<Feed>> {
    let mut stmt = conn.prepare(&format!("{FEED_COLUMNS} WHERE f.id = ?1"))?;
    stmt.query_row(params![id], map_feed_row)
        .optional()
        .map_err(Into::into)
}

/// A feed's current `feed_url`. The refresh-completion guard reads this under
/// the same write lock it would apply its outcome through: a feed re-pointed
/// mid-refresh (the edit-feed dialog) must not receive the old URL's result.
pub fn feed_url_by_id(conn: &Connection, id: i64) -> AppResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT feed_url FROM feeds WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?)
}

fn map_feed_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Feed> {
    Ok(Feed {
        id: r.get(0)?,
        feed_url: r.get(1)?,
        site_url: r.get(2)?,
        title: r.get(3)?,
        description: r.get(4)?,
        favicon_url: r.get(5)?,
        folder_id: r.get(6)?,
        source_type: r.get(7)?,
        last_fetched_at: r.get(8)?,
        fetch_error: r.get(9)?,
        unread_count: r.get(10)?,
        refresh_interval_min: r.get(11)?,
        auto_translate: r.get::<_, i64>(12)? != 0,
        open_mode: r.get(13)?,
    })
}

/// A feed the scheduler should fetch: `(id, feed_url, etag, last_modified)` —
/// the last two are the stored revalidators for a conditional GET.
pub type FeedToRefresh = (i64, String, Option<String>, Option<String>);

/// The `IN (…)` operand list naming every built-in parser's `source_type` —
/// the parser leg's query filter (`refresh::poll_parsers`). Mirrors the
/// registry in `ingestion::parsers`; a cross-check test pins the two
/// together so a new parser cannot be half-registered.
pub const PARSER_SOURCE_TYPES_IN: &str = "('x', 'xiaohongshu', 'kuaishou')";

/// The `NOT IN (…)` operand list every plain-HTTP query filters with:
/// newsletters (IMAP leg), the built-in parser source types (parser leg) and
/// the engine-run rsshub sources (engine leg). None of these feed_urls is a
/// plain feed document a conditional GET could parse.
pub const NON_HTTP_SOURCE_TYPES: &str = "('newsletter', 'x', 'xiaohongshu', 'kuaishou', 'rsshub')";

/// All feeds that need an HTTP fetch. Newsletter, parser and rsshub sources
/// are excluded — newsletters are polled over IMAP, parser feeds are fetched
/// by the parser leg (see `refresh::poll_parsers`) and rsshub routes run
/// through the local engine (see `refresh::poll_rsshub`); none of their
/// feed_urls is a plain feed document a conditional GET could parse.
pub fn feeds_to_refresh(conn: &Connection) -> AppResult<Vec<FeedToRefresh>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, feed_url, etag, last_modified FROM feeds\n         WHERE source_type NOT IN {NON_HTTP_SOURCE_TYPES}"
    ))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The "never auto-refresh" sentinel (minutes ≈ one year). The Settings panel
/// writes it as the global interval when auto-refresh is switched off, and a
/// per-feed interval can carry it to opt one feed out of automatic refresh.
pub const REFRESH_OFF_MINUTES: i64 = 525_600;

/// Non-parser feeds (newsletters excluded) that are *due* for a fetch: their effective interval —
/// the per-feed `refresh_interval_min`, or `global_min` when unset — has
/// elapsed since `last_fetched_at` (a never-fetched feed is always due). Feeds
/// whose effective interval is the "off" sentinel are excluded entirely. Used
/// by the background scheduler; the manual refresh still fetches every feed via
/// `feeds_to_refresh`.
pub fn feeds_due_for_refresh(conn: &Connection, global_min: i64) -> AppResult<Vec<FeedToRefresh>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, feed_url, etag, last_modified FROM feeds\n         WHERE source_type NOT IN {NON_HTTP_SOURCE_TYPES}\n           AND COALESCE(refresh_interval_min, ?1) < ?2\n           AND ( last_fetched_at IS NULL\n                 OR (julianday('now') - julianday(last_fetched_at)) * 1440.0\n                    >= COALESCE(refresh_interval_min, ?1) )"
    ))?;
    let rows = stmt
        .query_map(params![global_min, REFRESH_OFF_MINUTES], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Non-parser feeds for a single feed id — the per-feed manual refresh. An
/// empty result means the id is unknown or names a source polled elsewhere
/// (a newsletter over IMAP, an X timeline via the parser leg, or an rsshub
/// route via the engine leg).
pub fn feeds_to_refresh_for_feed(conn: &Connection, feed_id: i64) -> AppResult<Vec<FeedToRefresh>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, feed_url, etag, last_modified FROM feeds
         WHERE source_type NOT IN {NON_HTTP_SOURCE_TYPES} AND id = ?1"
    ))?;
    let rows = stmt
        .query_map(params![feed_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Non-parser feeds in a folder — the per-folder manual refresh.
pub fn feeds_to_refresh_in_folder(
    conn: &Connection,
    folder_id: i64,
) -> AppResult<Vec<FeedToRefresh>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, feed_url, etag, last_modified FROM feeds
         WHERE source_type NOT IN {NON_HTTP_SOURCE_TYPES} AND folder_id = ?1"
    ))?;
    let rows = stmt
        .query_map(params![folder_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Set (or clear) a feed's per-feed refresh interval. `None` reverts the feed
/// to the global interval; `Some(REFRESH_OFF_MINUTES)` opts it out entirely.
pub fn set_feed_refresh_interval(
    conn: &Connection,
    id: i64,
    minutes: Option<i64>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET refresh_interval_min = ?2 WHERE id = ?1",
        params![id, minutes],
    )?;
    Ok(())
}

/// Toggle a feed's per-feed auto-translate flag. When on, opening an article
/// from this feed starts a translation into the configured target language.
pub fn set_feed_auto_translate(conn: &Connection, id: i64, enabled: bool) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET auto_translate = ?2 WHERE id = ?1",
        params![id, enabled as i64],
    )?;
    Ok(())
}

/// Set (or clear) a feed's per-feed open mode: `"reader"`, `"extracted"` or
/// `"web"`. `None` reverts the feed to the default behaviour.
pub fn set_feed_open_mode(conn: &Connection, id: i64, mode: Option<&str>) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET open_mode = ?2 WHERE id = ?1",
        params![id, mode],
    )?;
    Ok(())
}

/// Refresh a feed's metadata from its parsed document. A `None` *or empty*
/// field leaves the stored value untouched. The feed-supplied `title` is
/// applied only when the user has *not* renamed the feed by hand
/// (`custom_title = 0`); otherwise `update_feed_meta` would revert a manual
/// rename on the next poll.
///
/// Empty strings are treated exactly like `None` (`NULLIF(?, '')`): `feed-rs`
/// parses a `<title></title>` element as `Some("")`, and the scheduler's
/// refresh path passes the parsed title straight through. Without the
/// `NULLIF` guard, a feed that momentarily serves an empty `<title>` would
/// overwrite a perfectly good feed name with a blank string in the sidebar —
/// `COALESCE` only skips a SQL `NULL`, not an empty string. `add_feed`
/// already filters empty titles on the subscribe path; this makes the
/// periodic-refresh path just as safe, and applies the same protection to
/// the other metadata columns.
pub fn update_feed_meta(
    conn: &Connection,
    id: i64,
    title: Option<&str>,
    site_url: Option<&str>,
    description: Option<&str>,
    favicon_url: Option<&str>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET
            title       = CASE WHEN custom_title = 1 THEN title
                               ELSE COALESCE(NULLIF(?2, ''), title) END,
            site_url    = COALESCE(NULLIF(?3, ''), site_url),
            description = COALESCE(NULLIF(?4, ''), description),
            favicon_url = COALESCE(NULLIF(?5, ''), favicon_url)
         WHERE id = ?1",
        params![id, title, site_url, description, favicon_url],
    )?;
    Ok(())
}

pub fn set_feed_fetch_state(
    conn: &Connection,
    id: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
    error: Option<&str>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET etag = ?2, last_modified = ?3, fetch_error = ?4,
                          last_fetched_at = datetime('now')
         WHERE id = ?1",
        params![id, etag, last_modified, error],
    )?;
    Ok(())
}

/// Record a successful fetch that produced no changes (304 Not Modified).
pub fn touch_feed(conn: &Connection, id: i64) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET last_fetched_at = datetime('now'), fetch_error = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// A single feed's `last_fetched_at` timestamp, if it has ever been fetched.
pub fn feed_last_fetched(conn: &Connection, id: i64) -> AppResult<Option<String>> {
    Ok(conn.query_row(
        "SELECT last_fetched_at FROM feeds WHERE id = ?1",
        params![id],
        |r| r.get::<_, Option<String>>(0),
    )?)
}

/// Record a failed fetch, keeping the previous content untouched.
pub fn set_feed_error(conn: &Connection, id: i64, error: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET last_fetched_at = datetime('now'), fetch_error = ?2 WHERE id = ?1",
        params![id, error],
    )?;
    Ok(())
}

pub fn delete_feed(conn: &Connection, id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM feeds WHERE id = ?1", params![id])?;
    Ok(())
}

/// Feeds for OPML export as `(title, feed_url, folder)` tuples. Newsletter,
/// parser and rsshub sources are excluded: OPML is an RSS-subscription
/// interchange format. A newsletter's `feed_url` is a synthetic `imap://…`
/// string, a parser feed's `x.com/<user>` URL is not a feed document, and an
/// `rsshub://` short link only resolves inside papr (the local engine) —
/// exporting any of the three would emit an `<outline>` any reader (Papr's
/// own `import_opml` included) would treat as an RSS feed and then fail to
/// HTTP-fetch forever, with the credentials not even carried.
pub fn feeds_for_export(conn: &Connection) -> AppResult<Vec<(String, String, Option<String>)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT f.title, f.feed_url, fo.name
         FROM feeds f LEFT JOIN folders fo ON fo.id = f.folder_id
         WHERE f.source_type NOT IN {NON_HTTP_SOURCE_TYPES}
         ORDER BY fo.name, f.title"
    ))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Feed URLs the sync layer should mirror onto the server. Excludes
/// `newsletter`, parser and rsshub sources (those have no upstream feed URL
/// the server could subscribe to), matching the OPML-export filter.
pub fn feed_urls_for_sync(conn: &Connection) -> AppResult<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT feed_url FROM feeds
         WHERE source_type NOT IN {NON_HTTP_SOURCE_TYPES} AND feed_url <> ''"
    ))?;
    let rows = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Find a folder by name, creating it if absent. Used during OPML import.
/// Resolve a folder name to its id, creating the folder when absent. Used by
/// OPML import to attach imported feeds to their folders. `create_folder` is
/// itself case-insensitively idempotent, so an OPML folder whose name matches
/// an existing folder (in any case) reuses that folder instead of spawning a
/// near-duplicate.
pub fn folder_id_by_name(conn: &Connection, name: &str) -> AppResult<i64> {
    create_folder(conn, name)
}

pub fn move_feed(conn: &Connection, id: i64, folder_id: Option<i64>) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET folder_id = ?2 WHERE id = ?1",
        params![id, folder_id],
    )?;
    Ok(())
}

/// The folder a feed currently sits in, if any. Used by sync to tell an
/// already-filed feed (leave it) from an unfiled one (adopt the server folder).
pub fn feed_folder_id(conn: &Connection, id: i64) -> AppResult<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT folder_id FROM feeds WHERE id = ?1",
            params![id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

/// Set a feed's display title to a user-chosen value. `custom_title` is also
/// raised so a later refresh's `update_feed_meta` does not revert the rename
/// back to the feed document's own `<title>`.
pub fn rename_feed(conn: &Connection, id: i64, title: &str) -> AppResult<()> {
    // Reject an empty/whitespace-only title at the chokepoint. A rename also
    // sets `custom_title = 1`, which makes `update_feed_meta`
    // never again overwrite the title from the feed document — so an empty
    // title would leave the feed *permanently* blank in the sidebar with no
    // recovery path, not even a refresh. The frontend `PromptDialog` guards
    // against this, but the backend command is the real chokepoint (other IPC
    // callers exist), so enforce it here the way `rename_tag` already does.
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::code("emptyFeedTitle"));
    }
    conn.execute(
        "UPDATE feeds SET title = ?2, custom_title = 1 WHERE id = ?1",
        params![id, title],
    )?;
    Ok(())
}

/// Point an existing subscription at a new feed document. The feed row keeps
/// its id — folders, articles, read state and every per-feed setting survive
/// the move — while the metadata follows the new document: the source type is
/// re-classified from the new URL + parsed feed, and a feed the user has not
/// renamed takes the new document's title. The stored conditional-GET
/// revalidators are dropped: the old ETag belongs to the old URL's content,
/// so the next poll must be a full GET.
pub fn edit_feed_url(
    conn: &Connection,
    id: i64,
    url: &str,
    source_type: crate::models::SourceType,
    title: &str,
) -> AppResult<()> {
    conn.execute(
        "UPDATE feeds SET feed_url = ?2, source_type = ?3,
                          title = CASE WHEN custom_title = 1 THEN title
                                       ELSE COALESCE(NULLIF(?4, ''), title) END,
                          etag = NULL, last_modified = NULL
         WHERE id = ?1",
        params![id, url, source_type.as_str(), title],
    )?;
    Ok(())
}

// ─────────────────────────── newsletter sources ───────────────────────────

/// One configured email-newsletter source: the backing feed plus its IMAP
/// connection details. Mirrors the `commands::NewsletterSource` payload.
pub struct NewsletterSourceRow {
    pub feed_id: i64,
    pub title: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub folder: String,
}

/// Insert a newsletter source: a `feeds` row (source_type = 'newsletter') plus
/// its IMAP credentials in `newsletter_sources`. Both land in one transaction
/// so a failure cannot leave a feed with no credentials. Returns the feed id.
pub fn insert_newsletter_source(
    conn: &Connection,
    feed_url: &str,
    title: &str,
    cfg: &crate::ingestion::newsletter::NewsletterConfig,
) -> AppResult<i64> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO feeds(feed_url, title, source_type) VALUES (?1, ?2, 'newsletter')",
        params![feed_url, title],
    )?;
    let feed_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO newsletter_sources(feed_id, host, port, username, password, folder)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            feed_id,
            cfg.host,
            cfg.port,
            cfg.username,
            cfg.password,
            cfg.folder
        ],
    )?;
    tx.commit()?;
    Ok(feed_id)
}

/// Every configured newsletter source (without the password) for the UI list.
pub fn list_newsletter_sources(conn: &Connection) -> AppResult<Vec<NewsletterSourceRow>> {
    let mut stmt = conn.prepare(
        "SELECT n.feed_id, f.title, n.host, n.port, n.username, n.folder
         FROM newsletter_sources n JOIN feeds f ON f.id = n.feed_id
         ORDER BY f.title COLLATE NOCASE",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(NewsletterSourceRow {
                feed_id: r.get(0)?,
                title: r.get(1)?,
                host: r.get(2)?,
                port: r.get::<_, i64>(3)? as u16,
                username: r.get(4)?,
                folder: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `(feed_id, IMAP config)` for every newsletter source — the work list the
/// refresh scheduler polls each cycle. Returning a `NewsletterConfig` directly
/// spares the caller a field-by-field rebuild.
pub fn newsletter_sources_to_poll(
    conn: &Connection,
) -> AppResult<Vec<(i64, crate::ingestion::newsletter::NewsletterConfig)>> {
    use crate::ingestion::newsletter::NewsletterConfig;
    let mut stmt = conn.prepare(
        "SELECT feed_id, host, port, username, password, folder FROM newsletter_sources",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                NewsletterConfig {
                    host: r.get::<_, String>(1)?,
                    port: r.get::<_, i64>(2)? as u16,
                    username: r.get::<_, String>(3)?,
                    password: r.get::<_, String>(4)?,
                    folder: r.get::<_, String>(5)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `(feed_id, IMAP config)` for newsletter sources that are *due* to be polled,
/// applying the same per-feed/global interval logic as `feeds_due_for_refresh`.
/// Used by the background scheduler; the manual refresh polls every mailbox.
pub fn newsletter_sources_due_to_poll(
    conn: &Connection,
    global_min: i64,
) -> AppResult<Vec<(i64, crate::ingestion::newsletter::NewsletterConfig)>> {
    use crate::ingestion::newsletter::NewsletterConfig;
    let mut stmt = conn.prepare(
        "SELECT s.feed_id, s.host, s.port, s.username, s.password, s.folder
         FROM newsletter_sources s JOIN feeds f ON f.id = s.feed_id
         WHERE COALESCE(f.refresh_interval_min, ?1) < ?2
           AND ( f.last_fetched_at IS NULL
                 OR (julianday('now') - julianday(f.last_fetched_at)) * 1440.0
                    >= COALESCE(f.refresh_interval_min, ?1) )",
    )?;
    let rows = stmt
        .query_map(params![global_min, REFRESH_OFF_MINUTES], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                NewsletterConfig {
                    host: r.get::<_, String>(1)?,
                    port: r.get::<_, i64>(2)? as u16,
                    username: r.get::<_, String>(3)?,
                    password: r.get::<_, String>(4)?,
                    folder: r.get::<_, String>(5)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `(feed_id, IMAP config)` for a single newsletter source — the per-feed
/// manual refresh. Empty when the feed id is not a newsletter source.
pub fn newsletter_sources_for_feed(
    conn: &Connection,
    feed_id: i64,
) -> AppResult<Vec<(i64, crate::ingestion::newsletter::NewsletterConfig)>> {
    use crate::ingestion::newsletter::NewsletterConfig;
    let mut stmt = conn.prepare(
        "SELECT feed_id, host, port, username, password, folder
         FROM newsletter_sources WHERE feed_id = ?1",
    )?;
    let rows = stmt
        .query_map(params![feed_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                NewsletterConfig {
                    host: r.get::<_, String>(1)?,
                    port: r.get::<_, i64>(2)? as u16,
                    username: r.get::<_, String>(3)?,
                    password: r.get::<_, String>(4)?,
                    folder: r.get::<_, String>(5)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `(feed_id, IMAP config)` for the newsletter sources in a folder — the
/// per-folder manual refresh.
pub fn newsletter_sources_in_folder(
    conn: &Connection,
    folder_id: i64,
) -> AppResult<Vec<(i64, crate::ingestion::newsletter::NewsletterConfig)>> {
    use crate::ingestion::newsletter::NewsletterConfig;
    let mut stmt = conn.prepare(
        "SELECT s.feed_id, s.host, s.port, s.username, s.password, s.folder
         FROM newsletter_sources s JOIN feeds f ON f.id = s.feed_id
         WHERE f.folder_id = ?1",
    )?;
    let rows = stmt
        .query_map(params![folder_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                NewsletterConfig {
                    host: r.get::<_, String>(1)?,
                    port: r.get::<_, i64>(2)? as u16,
                    username: r.get::<_, String>(3)?,
                    password: r.get::<_, String>(4)?,
                    folder: r.get::<_, String>(5)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Remove a newsletter source. Deleting the `feeds` row cascades to both
/// `newsletter_sources` and the source's articles.
pub fn delete_newsletter_source(conn: &Connection, feed_id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM feeds WHERE id = ?1", params![feed_id])?;
    Ok(())
}

/// The `source_type` of the feed an article belongs to, or `None` for an
/// unknown article id. Lets callers treat parser-served content differently —
/// most importantly, refuse to web-extract it (see `extract_fulltext`).
pub fn article_feed_source_type(conn: &Connection, article_id: i64) -> AppResult<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT f.source_type
         FROM articles a JOIN feeds f ON f.id = a.feed_id
         WHERE a.id = ?1",
    )?;
    let mut rows = stmt.query(params![article_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Re-render an existing parser article in place, returning whether anything
/// changed.
///
/// Parser feeds generate their body from papr's own renderer, so the stored
/// markup can go stale when the renderer changes — and a plain re-fetch cannot
/// fix it, because the same `(feed_id, guid)` is skipped as a duplicate. This
/// updates the renderer-owned columns for a row that already exists and clears
/// `extracted_html`: for a parser feed that column can only hold a previous web
/// extraction of the site's SPA shell, which is exactly what must not win over
/// the rendered card. Read state, stars, read-later, tags, highlights and the
/// AI summary are deliberately left alone. A non-empty incoming enclosure
/// list replaces the stored one, so a card-to-fulltext upgrade carries its
/// video media along; an empty list never clears what is stored.
/// Outcome of refreshing an existing parser-owned article row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserRefreshOutcome {
    /// The row existed and at least one column changed.
    Updated,
    /// The row existed and already matched (or was protected by the downgrade guard).
    Unchanged,
    /// No row matched `(feed_id, guid)` (e.g. skipped by dedup or retention tombstone).
    NotFound,
}

pub fn refresh_parser_article(conn: &Connection, feed_id: i64, a: &NewArticle) -> AppResult<bool> {
    Ok(matches!(
        refresh_parser_article_with_status(conn, feed_id, a)?,
        ParserRefreshOutcome::Updated
    ))
}

pub fn refresh_parser_article_with_status(
    conn: &Connection,
    feed_id: i64,
    a: &NewArticle,
) -> AppResult<ParserRefreshOutcome> {
    /// The stored renderer-owned columns of one parser article row.
    type StoredRow = (
        i64,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        String,
        Option<String>,
    );
    let existing: Option<StoredRow> = conn
        .query_row(
            "SELECT id, content_html, extracted_html, body_text, url, title, summary
             FROM articles WHERE feed_id = ?1 AND guid = ?2",
            params![feed_id, a.guid],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        id,
        content_html,
        extracted_html,
        body_text,
        stored_url,
        stored_title,
        stored_summary,
    )) = existing
    else {
        return Ok(ParserRefreshOutcome::NotFound);
    };
    // Idempotent: a refresh usually re-renders the same tweet to the same HTML,
    // and rewriting identical bytes would churn WAL pages and re-index FTS.
    // An empty incoming body carries no information (the kuaishou live-api
    // ships no captions): keep what is stored rather than blanking it — the
    // same contract as the COALESCE on url/published_at below. The equality
    // check compares the effective body so an always-empty incoming body
    // does not read as a change on every refresh.
    // Downgrade guard, X-article specific: a row whose stored card carries the
    // full article body must never be rewritten by an incoming card that only
    // renders the bare title link — that is exactly what a cycle whose
    // TweetDetail fetch failed produces. The stored body survives until a
    // fetch succeeds again (then the incoming markup carries the body
    // container and the normal diff path applies). Matching the class
    // ATTRIBUTE (not any substring) so prose that merely mentions the marker
    // — an article whose title contains "x-article-body" — cannot impersonate
    // a body container.
    if content_html
        .as_deref()
        .is_some_and(|h| h.contains(r#"class="x-article-body""#))
        && !a
            .content_html
            .as_deref()
            .is_some_and(|h| h.contains(r#"class="x-article-body""#))
    {
        return Ok(ParserRefreshOutcome::Unchanged);
    }
    let effective_body = if a.body_text.is_empty() {
        body_text.clone()
    } else {
        a.body_text.clone()
    };
    let rendered_differs = content_html.as_deref() != a.content_html.as_deref()
        || body_text != effective_body
        // Title and summary are rewritten by the UPDATE below but nothing
        // else forces the early-return path open when only they change —
        // compare them too (e.g. a kuaishou work card whose label follows a
        // payload change while the markup stays identical).
        || stored_title != a.title
        || stored_summary != a.summary;
    let stale_extraction = extracted_html.is_some();
    // A URL the stored row lacks counts as a change too: the permalink
    // backfill must land even when the body is byte-identical (an anonymous
    // card row whose detail upgrade re-renders to the same markup).
    let url_backfill = a.url.is_some() && stored_url.as_deref() != a.url.as_deref();
    if !rendered_differs && !stale_extraction && !url_backfill {
        return Ok(ParserRefreshOutcome::Unchanged);
    }
    // When the body itself changed — including when clearing `extracted_html`
    // switches the effective body source back to `content_html` — every
    // rendering derived from the OLD body is stale too. A title change clears
    // the title translation specifically; URL-only backfills (with no
    // effective-body change) keep the caches.
    let title_changed = stored_title != a.title;
    let prev_effective_html = extracted_html
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or(content_html.as_deref());
    let body_changed = content_html.as_deref() != a.content_html.as_deref()
        || prev_effective_html != a.content_html.as_deref()
        || body_text != effective_body
        || stored_summary != a.summary;

    let tx = conn.unchecked_transaction()?;
    // `url` follows when the incoming article carries one: an anonymous card
    // row stored without a permalink gains the real explore URL when its
    // detail is fetched later. `COALESCE` keeps an absent incoming URL from
    // clearing a stored one. The derived caches follow `body_changed` and
    // `title_changed` — `CASE WHEN` on bound flags, so a URL-only backfill
    // keeps them.
    tx.execute(
        "UPDATE articles
            SET title = ?2, summary = ?3, content_html = ?4, body_text = ?5,
                image_url = COALESCE(?6, image_url),
                published_at = COALESCE(?7, published_at),
                url = COALESCE(?8, url),
                extracted_html = NULL,
                translated_html = CASE WHEN ?9 THEN NULL ELSE translated_html END,
                translated_lang = CASE WHEN ?9 THEN NULL ELSE translated_lang END,
                translated_title = CASE WHEN ?10 THEN NULL ELSE translated_title END,
                ai_summary = CASE WHEN ?9 THEN NULL ELSE ai_summary END
          WHERE id = ?1",
        params![
            id,
            a.title,
            a.summary,
            a.content_html,
            effective_body,
            a.image_url,
            a.published_at,
            a.url,
            body_changed,
            title_changed
        ],
    )?;
    // Enclosures must follow the markup: the xiaohongshu pending-retry path
    // upgrades a card row (no enclosure) to a full-text video note (mp4), and
    // leaving the old list in place would make `get_article` return media-less
    // content. Only an incoming non-empty list rewrites — an empty one never
    // destroys what is stored.
    if !a.enclosures.is_empty() {
        tx.execute("DELETE FROM enclosures WHERE article_id = ?1", params![id])?;
        for e in &a.enclosures {
            tx.execute(
                "INSERT INTO enclosures(article_id, url, mime_type, length) VALUES (?1,?2,?3,?4)",
                params![id, e.url, e.mime_type, e.length],
            )?;
        }
    }
    tx.execute(
        "UPDATE articles_fts SET title = ?2, body = ?3 WHERE rowid = ?1",
        params![id, a.title, effective_body],
    )?;
    tx.commit()?;
    Ok(ParserRefreshOutcome::Updated)
}

/// One parser feed handed to the refresh leg: the row id, the parser that
/// owns it (`source_type`) and its canonical URL.
pub type ParserFeed = (i64, String, String);

/// The subset of `guids` already stored for `feed_id` — the "is this item
/// new" check the xiaohongshu leg uses to spend note-detail fetches (cookie
/// mode) only on genuinely new notes.
pub fn filter_existing_guids(
    conn: &Connection,
    feed_id: i64,
    guids: &[String],
) -> AppResult<Vec<String>> {
    if guids.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT guid FROM articles WHERE feed_id = ?1")?;
    let stored: std::collections::HashSet<String> = stmt
        .query_map(params![feed_id], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    Ok(guids
        .iter()
        .filter(|g| stored.contains(*g))
        .cloned()
        .collect())
}

/// `(feed_id, source_type, feed_url)` for every parser feed, the shape the
/// parser leg of `refresh_core` needs — `source_type` names the parser that
/// owns the feed and the feed_url carries its handle / profile id. No
/// revalidators exist: the upstream APIs send no conditional-GET headers.
pub fn parser_feeds_to_refresh(conn: &Connection) -> AppResult<Vec<ParserFeed>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, source_type, feed_url FROM feeds WHERE source_type IN {PARSER_SOURCE_TYPES_IN}"
    ))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Parser feeds that are *due*, applying the same per-feed/global interval
/// logic as [`feeds_due_for_refresh`]. Used by the background scheduler; the
/// manual refresh polls every parser feed via [`parser_feeds_to_refresh`].
pub fn parser_feeds_due_for_refresh(
    conn: &Connection,
    global_min: i64,
) -> AppResult<Vec<ParserFeed>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, source_type, feed_url FROM feeds WHERE source_type IN {PARSER_SOURCE_TYPES_IN}
         AND COALESCE(refresh_interval_min, ?1) < ?2
         AND ( last_fetched_at IS NULL
               OR (julianday('now') - julianday(last_fetched_at)) * 1440.0
                  >= COALESCE(refresh_interval_min, ?1) )"
    ))?;
    let rows = stmt
        .query_map(params![global_min, REFRESH_OFF_MINUTES], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `(feed_id, source_type, feed_url)` for one parser feed by id — the
/// per-feed manual refresh. Empty when the id is not a parser feed.
pub fn parser_feeds_for_feed(conn: &Connection, feed_id: i64) -> AppResult<Vec<ParserFeed>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, source_type, feed_url FROM feeds WHERE source_type IN {PARSER_SOURCE_TYPES_IN} AND id = ?1"
    ))?;
    let rows = stmt
        .query_map(params![feed_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `(feed_id, source_type, feed_url)` for the parser feeds in a folder —
/// the per-folder manual refresh.
pub fn parser_feeds_in_folder(conn: &Connection, folder_id: i64) -> AppResult<Vec<ParserFeed>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, source_type, feed_url FROM feeds WHERE source_type IN {PARSER_SOURCE_TYPES_IN} AND folder_id = ?1"
    ))?;
    let rows = stmt
        .query_map(params![folder_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ─────────────────────────── rsshub routes ───────────────────────────

/// One installed RSSHub route (a row of `rsshub_routes`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RsshubRouteRow {
    /// The route's *template* path, e.g. `/bilibili/user/video/:id`.
    pub route_path: String,
    /// When disabled, its subscriptions are skipped by the engine refresh leg.
    pub enabled: bool,
    /// The route's environment variables as a JSON object of strings.
    pub env_json: String,
    pub installed_at: String,
}

/// Install (or re-install) a route template. Enabling state and environment
/// variables survive a re-install — the marketplace's "install" on an
/// already-installed route is a no-op for configuration.
pub fn rsshub_route_upsert(conn: &Connection, route_path: &str) -> AppResult<()> {
    conn.execute(
        "INSERT INTO rsshub_routes(route_path, enabled, env_json, installed_at)
         VALUES (?1, 1, '{}', datetime('now'))
         ON CONFLICT(route_path) DO UPDATE SET installed_at = excluded.installed_at",
        params![route_path],
    )?;
    Ok(())
}

fn rsshub_route_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RsshubRouteRow> {
    Ok(RsshubRouteRow {
        route_path: r.get(0)?,
        enabled: r.get::<_, i64>(1)? != 0,
        env_json: r.get(2)?,
        installed_at: r.get(3)?,
    })
}

/// Every installed route, oldest install first (the env-merge order and the
/// marketplace's "installed" list both want this).
pub fn rsshub_routes_list(conn: &Connection) -> AppResult<Vec<RsshubRouteRow>> {
    let mut stmt = conn.prepare(
        "SELECT route_path, enabled, env_json, installed_at
         FROM rsshub_routes ORDER BY installed_at, route_path",
    )?;
    let rows = stmt
        .query_map([], rsshub_route_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One installed route by template path.
pub fn rsshub_route_get(conn: &Connection, route_path: &str) -> AppResult<Option<RsshubRouteRow>> {
    Ok(conn
        .query_row(
            "SELECT route_path, enabled, env_json, installed_at
             FROM rsshub_routes WHERE route_path = ?1",
            params![route_path],
            rsshub_route_row,
        )
        .optional()?)
}

/// Toggle a route. Disabled routes stop refreshing their subscriptions but
/// keep their environment variables.
pub fn rsshub_route_set_enabled(
    conn: &Connection,
    route_path: &str,
    enabled: bool,
) -> AppResult<()> {
    conn.execute(
        "UPDATE rsshub_routes SET enabled = ?2 WHERE route_path = ?1",
        params![route_path, enabled as i64],
    )?;
    Ok(())
}

/// Replace a route's environment variables (`env_json` is a JSON object of
/// strings; the caller validates).
pub fn rsshub_route_set_env(conn: &Connection, route_path: &str, env_json: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE rsshub_routes SET env_json = ?2 WHERE route_path = ?1",
        params![route_path, env_json],
    )?;
    Ok(())
}

/// Uninstall a route. Subscribed feeds (their `rsshub://` URLs) are left
/// alone — the UI warns before calling this.
pub fn rsshub_route_delete(conn: &Connection, route_path: &str) -> AppResult<()> {
    conn.execute(
        "DELETE FROM rsshub_routes WHERE route_path = ?1",
        params![route_path],
    )?;
    Ok(())
}

/// The merged environment variables of every *enabled* route.
///
/// Each row's `env_json` is an object of string variables; rows merge in
/// install order with later installs winning on name collision (deterministic
/// because `rsshub_routes_list` orders by `installed_at`). Malformed JSON is
/// skipped defensively — one corrupted row must not break engine start for
/// every route; the marketplace UI rewrites `env_json` on the next save.
/// Entries a child process could never carry are dropped for the same
/// reason — `rsshub::valid_env_key` / `valid_env_value` re-apply the save
/// path's limits here, because older rows may predate them (an over-long
/// value past the ~128 KiB `execve` E2BIG threshold would otherwise fail
/// the spawn and stall every RSSHub subscription).
pub fn rsshub_enabled_env(conn: &Connection) -> BTreeMap<String, String> {
    let mut merged = BTreeMap::new();
    for row in rsshub_routes_list(conn).unwrap_or_default() {
        if !row.enabled {
            continue;
        }
        let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(&row.env_json)
        else {
            continue;
        };
        for (k, v) in map {
            if let Some(s) = v.as_str() {
                if !rsshub::valid_env_key(&k) || !rsshub::valid_env_value(s) {
                    continue;
                }
                merged.insert(k, s.to_string());
            }
        }
    }
    merged
}

/// `(feed_id, feed_url, etag, last_modified)` for every rsshub feed —
/// the shape the engine leg of `refresh_core` needs. Unlike parser feeds the
/// engine serves real conditional-GET headers, so the revalidators travel
/// along ([`FeedToRefresh`]).
pub fn rsshub_feeds_to_refresh(conn: &Connection) -> AppResult<Vec<FeedToRefresh>> {
    let mut stmt = conn.prepare(
        "SELECT id, feed_url, etag, last_modified FROM feeds WHERE source_type = 'rsshub'",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// rsshub feeds that are *due*, applying the same per-feed/global interval
/// logic as [`feeds_due_for_refresh`]. Used by the background scheduler; the
/// manual refresh polls every rsshub feed via [`rsshub_feeds_to_refresh`].
pub fn rsshub_feeds_due_for_refresh(
    conn: &Connection,
    global_min: i64,
) -> AppResult<Vec<FeedToRefresh>> {
    let mut stmt = conn.prepare(
        "SELECT id, feed_url, etag, last_modified FROM feeds WHERE source_type = 'rsshub'
         AND COALESCE(refresh_interval_min, ?1) < ?2
         AND ( last_fetched_at IS NULL
               OR (julianday('now') - julianday(last_fetched_at)) * 1440.0
                  >= COALESCE(refresh_interval_min, ?1) )",
    )?;
    let rows = stmt
        .query_map(params![global_min, REFRESH_OFF_MINUTES], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// rsshub feeds for a single feed id — the per-feed manual refresh. Empty
/// when the id is not an rsshub feed.
pub fn rsshub_feeds_for_feed(conn: &Connection, feed_id: i64) -> AppResult<Vec<FeedToRefresh>> {
    let mut stmt = conn.prepare(
        "SELECT id, feed_url, etag, last_modified FROM feeds
         WHERE source_type = 'rsshub' AND id = ?1",
    )?;
    let rows = stmt
        .query_map(params![feed_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// rsshub feeds in a folder — the per-folder manual refresh.
pub fn rsshub_feeds_in_folder(conn: &Connection, folder_id: i64) -> AppResult<Vec<FeedToRefresh>> {
    let mut stmt = conn.prepare(
        "SELECT id, feed_url, etag, last_modified FROM feeds
         WHERE source_type = 'rsshub' AND folder_id = ?1",
    )?;
    let rows = stmt
        .query_map(params![folder_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ─────────────────────────── articles ───────────────────────────

/// A parsed article ready for insertion.
#[derive(Debug, Clone)]
pub struct NewArticle {
    pub guid: String,
    pub url: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub content_html: Option<String>,
    pub body_text: String,
    pub image_url: Option<String>,
    pub published_at: Option<String>,
    pub enclosures: Vec<Enclosure>,
}

/// True if `rule` (scoped to `feed_id`) matches the incoming article `a`.
///
/// The When-document is a group list: every condition of **any one** group
/// must match (AND within, OR across). An empty or unparsable document matches
/// nothing — such rows can't be saved through the API, so only hand-edited
/// databases can hold them, and staying inert beats matching everything.
fn rule_matches(rule: &Rule, feed_id: i64, a: &NewArticle) -> bool {
    if rule.feed_id.is_some_and(|fid| fid != feed_id) {
        return false;
    }
    if rule.conditions.is_empty() {
        return false;
    }
    // An empty group doesn't participate — `all()` over no conditions is
    // vacuously true, which would turn a stray empty group (only reachable
    // via a hand-edited row; save-time validation rejects it) into "matches
    // everything". Same policy as `rule_match_where`, which skips empty
    // groups, so preview / backfill counts stay identical to ingestion.
    rule.conditions
        .iter()
        .filter(|group| !group.is_empty())
        .any(|group| group.iter().all(|c| condition_matches(c, a)))
}

/// True if one condition holds for article `a`. Each field checks each of its
/// text sources *independently* — a needle must lie wholly within one of them.
/// Concatenating the sources would let a needle straddle a boundary (a title
/// ending in "machine" + a body starting with "learning" matching "machine
/// learning"), so live ingestion would act on articles the SQL preview never
/// counted. Case handling mirrors `condition_where`: the four substring /
/// equality operators fold case (Unicode-aware `to_lowercase()`, matching the
/// SQL side's `unicode_lower()`), while `regex` sees the raw text.
fn condition_matches(c: &RuleCondition, a: &NewArticle) -> bool {
    // A whitespace-only needle matches nothing, exactly like the SQL side's
    // `condition_where` (which emits a literal `0` for one). Save-time
    // validation rejects such values; this keeps hand-edited rows symmetric.
    if c.value.trim().is_empty() {
        return false;
    }
    let author = a.author.as_deref().unwrap_or("");
    let url = a.url.as_deref().unwrap_or("");
    let sources: &[&str] = match c.field.as_str() {
        "title" => &[a.title.as_str()],
        "author" => &[author],
        "content" => &[a.body_text.as_str()],
        "url" => &[url],
        "any" => &[a.title.as_str(), author, a.body_text.as_str()],
        // Unknown field (hand-edited row): never matches.
        _ => return false,
    };
    match c.operator.as_str() {
        "contains" => {
            let needle = c.value.to_lowercase();
            // A needle with an embedded NUL stays inert: SQLite's LIKE treats
            // its pattern as NUL-terminated, so the SQL twin could silently
            // widen `%x\0y%` into `%` (match everything). Rust's `contains`
            // keeps NULs literal — inert on both sides is the only way they
            // can agree.
            !needle.is_empty()
                && !needle.contains('\0')
                && sources.iter().any(|h| h.to_lowercase().contains(&needle))
        }
        // Negations quantify over *all* sources: "no source contains" /
        // "no source equals", not "some source doesn't".
        "not_contains" => {
            let needle = c.value.to_lowercase();
            !needle.is_empty()
                && !needle.contains('\0')
                && sources.iter().all(|h| !h.to_lowercase().contains(&needle))
        }
        "eq" => {
            let needle = c.value.to_lowercase();
            sources.iter().any(|h| h.to_lowercase() == needle)
        }
        "not_eq" => {
            let needle = c.value.to_lowercase();
            sources.iter().all(|h| h.to_lowercase() != needle)
        }
        // Regex runs on the raw text — lowercasing the haystack would break
        // patterns like `[A-Z]`, and lowercasing the pattern would rewrite
        // classes like `\D`. Case-insensitivity is the pattern's `(?i)` flag.
        "regex" => RuleCondition::compile(&c.value)
            .is_some_and(|re| sources.iter().any(|h| re.is_match(h))),
        // Unknown operator (hand-edited row): never matches.
        _ => false,
    }
}

/// The rule effects [`article_precheck`] collects for one article: the read /
/// starred state the insert should start with, and the literal replacements
/// (from every matching `rewrite` rule, applied in rule order) to run on the
/// article before it lands.
#[derive(Debug, Default, Clone)]
pub struct RuleEffects {
    pub read: bool,
    pub starred: bool,
    pub rewrites: Vec<RuleRewrite>,
}

/// Insert an article if it is new (by feed_id + guid). Returns `true` only when
/// a genuinely **new and unread** article was inserted — callers tally this as
/// the count of fresh articles surfaced to the user (refresh toast, "new
/// articles" notification, `add_newsletter_source`'s `unread_count`).
///
/// An article inserted but pre-marked read by a `read` rule returns `false`:
/// the row landed, but it never shows up as unread, so counting it would
/// inflate the "N new articles" figure and disagree with the sidebar's unread
/// count (the same overcount `add_feed` guards against).
///
/// When `dedup` is on, an article whose URL already exists (in any feed) is
/// skipped — collapsing the same story pushed by multiple feeds. Enabled
/// `rules` are evaluated first: a `skip` match drops the article entirely,
/// `read` / `star` matches pre-set the article's state on insert, and matching
/// `rewrite` rules replace literal text in the article before it lands (rules
/// match the *original* text — rewrites compose in rule order and never
/// influence which rules fire). Open-time actions (`translate` …) are no-ops
/// here — the reader resolves them per article via [`rule_actions_for_article`].
pub fn upsert_article(
    conn: &Connection,
    feed_id: i64,
    a: &NewArticle,
    dedup: bool,
    rules: &[Rule],
) -> AppResult<bool> {
    let Some(effects) = article_precheck(conn, feed_id, a, dedup, rules)? else {
        return Ok(false);
    };
    // The article row, its FTS index entry, and its enclosures must land
    // together — a partial insert leaves an unsearchable or enclosure-less
    // article. Wrap them in a transaction so a mid-loop failure rolls back.
    let tx = conn.unchecked_transaction()?;
    let inserted = if effects.rewrites.is_empty() {
        insert_article_row(&tx, feed_id, a, effects.read, effects.starred)?
    } else {
        let mut rewritten = a.clone();
        apply_rewrites(&mut rewritten, &effects.rewrites);
        insert_article_row(&tx, feed_id, &rewritten, effects.read, effects.starred)?
    };
    tx.commit()?;
    Ok(inserted)
}

/// Apply literal from→to replacements to an article's user-visible text:
/// title, summary, HTML body and plain-text body. Only the text is touched —
/// URLs and guids stay stable so dedup / tombstones keep working. Case-
/// sensitive on purpose: it matches SQLite's `REPLACE()` used by the
/// existing-article backfill, so a rewrite means the same thing in both paths.
pub fn apply_rewrites(a: &mut NewArticle, rewrites: &[RuleRewrite]) {
    for rw in rewrites {
        if rw.from.is_empty() {
            // Rejected at save time; a hand-edited empty `from` must not
            // sprinkle `to` between every character (`str::replace` would).
            continue;
        }
        if rw.from == rw.to {
            // A no-op pair: skipping it here keeps the ingestion path in
            // lockstep with the backfill, where a no-op pair must not count
            // a row as changed or invalidate its derivative caches.
            continue;
        }
        a.title = a.title.replace(&rw.from, &rw.to);
        a.summary = a.summary.take().map(|s| s.replace(&rw.from, &rw.to));
        a.content_html = a.content_html.take().map(|s| s.replace(&rw.from, &rw.to));
        a.body_text = a.body_text.replace(&rw.from, &rw.to);
    }
}

/// The decision half of [`upsert_article`]: dedup, retention tombstones and
/// filter rules, all pure reads. `None` = skip the article outright (a `skip`
/// rule, a tombstoned or deduplicated guid); `Some(RuleEffects)` carries the
/// rule-tinted start state and the rewrites for the insert half. Callers that
/// already hold a transaction run this first, then [`insert_article_row`]
/// inside it.
pub fn article_precheck(
    conn: &Connection,
    feed_id: i64,
    a: &NewArticle,
    dedup: bool,
    rules: &[Rule],
) -> AppResult<Option<RuleEffects>> {
    if dedup {
        if let Some(url) = a.url.as_deref().filter(|u| !u.is_empty()) {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM articles WHERE url = ?1)",
                params![url],
                |r| r.get(0),
            )?;
            if exists {
                return Ok(None);
            }
        }
    }
    // A previously retention-purged article (same feed + guid) must not be
    // re-ingested as fresh unread. A full-archive feed keeps every past item in
    // its document, so without this the daily cleanup and the next refresh would
    // ping-pong the whole read history back as new, every day (issue #98).
    // Checked regardless of the `dedup` flag — the tombstone, not URL dedup, is
    // what closes the loop.
    let tombstoned: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM article_tombstones WHERE feed_id = ?1 AND guid = ?2)",
        params![feed_id, a.guid],
        |r| r.get(0),
    )?;
    if tombstoned {
        return Ok(None);
    }
    // Apply filter rules: skip wins outright; read / star tint the new row;
    // every matching rewrite rule contributes its replacements. Open-time
    // actions (`translate` …) land in the catch-all: they are resolved when
    // the reader opens the article (`rule_actions_for_article`), not here.
    let mut effects = RuleEffects::default();
    for rule in rules {
        if !rule_matches(rule, feed_id, a) {
            continue;
        }
        match rule.action.as_str() {
            "skip" => return Ok(None),
            "read" => effects.read = true,
            "star" => effects.starred = true,
            "rewrite" => effects.rewrites.extend(rule.rewrites.iter().cloned()),
            _ => {}
        }
    }
    Ok(Some(effects))
}

/// The write half of [`upsert_article`]: the article row, its FTS index entry
/// and its enclosures. Must run inside a caller-owned transaction (rusqlite
/// has no nested BEGIN, so this never opens one of its own) — `upsert_article`
/// is the standalone, transaction-wrapping entry point.
pub fn insert_article_row(
    tx: &Connection,
    feed_id: i64,
    a: &NewArticle,
    start_read: bool,
    start_starred: bool,
) -> AppResult<bool> {
    let n = tx.execute(
        "INSERT INTO articles
            (feed_id, guid, url, title, author, summary, content_html, body_text,
             image_url, published_at, is_read, is_starred)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(feed_id, guid) DO NOTHING",
        params![
            feed_id,
            a.guid,
            a.url,
            a.title,
            a.author,
            a.summary,
            a.content_html,
            a.body_text,
            a.image_url,
            a.published_at,
            start_read,
            start_starred
        ],
    )?;
    if n == 0 {
        return Ok(false);
    }
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO articles_fts(rowid, title, body) VALUES (?1, ?2, ?3)",
        params![id, a.title, a.body_text],
    )?;
    for e in &a.enclosures {
        tx.execute(
            "INSERT INTO enclosures(article_id, url, mime_type, length) VALUES (?1,?2,?3,?4)",
            params![id, e.url, e.mime_type, e.length],
        )?;
    }
    // A row inserted but pre-marked read by a `read` rule is not "new" from
    // the user's point of view — report it as not-inserted so it is excluded
    // from new-article tallies.
    Ok(!start_read)
}

/// Build and run the article-list query for the given sidebar selection.
/// WHERE clauses + bind values selecting the articles for `query` under the
/// unread filter — shared by `list_articles` and `article_index` so the two
/// never drift in *which* rows they consider (a drift would put a located
/// article at an index the list doesn't agree with). The returned clauses
/// assume the `articles a JOIN feeds f` aliases used by both callers.
fn article_filter(query: &ArticleQuery, unread_only: bool) -> (Vec<String>, Vec<Value>) {
    let mut where_clauses: Vec<String> = vec!["1=1".into()];
    let mut binds: Vec<Value> = Vec::new();

    match query {
        ArticleQuery::All => {}
        ArticleQuery::Unread => where_clauses.push("a.is_read = 0".into()),
        ArticleQuery::Starred => where_clauses.push("a.is_starred = 1".into()),
        ArticleQuery::ReadLater => where_clauses.push("a.read_later = 1".into()),
        ArticleQuery::Feed(id) => {
            where_clauses.push("a.feed_id = ?".into());
            binds.push(Value::Integer(*id));
        }
        ArticleQuery::Folder(id) => {
            where_clauses.push("f.folder_id = ?".into());
            binds.push(Value::Integer(*id));
        }
        ArticleQuery::Tag(id) => {
            where_clauses
                .push("a.id IN (SELECT article_id FROM article_tags WHERE tag_id = ?)".into());
            binds.push(Value::Integer(*id));
        }
    }
    if unread_only && !matches!(query, ArticleQuery::Unread) {
        where_clauses.push("a.is_read = 0".into());
    }
    (where_clauses, binds)
}

/// The non-search ORDER BY clause shared by `list_articles` and
/// `article_index`. See `list_articles` for why the effective date is wrapped
/// in `datetime(COALESCE(...))`.
fn article_order(oldest_first: bool) -> &'static str {
    if oldest_first {
        "datetime(COALESCE(a.published_at, a.fetched_at)) ASC, a.id ASC"
    } else {
        "datetime(COALESCE(a.published_at, a.fetched_at)) DESC, a.id DESC"
    }
}

/// 0-based position of `article_id` within the list `query` would produce under
/// the same unread filter and sort — or `None` when the article isn't in that
/// list (filtered out, or not in this feed/folder/tag). Lets the frontend page
/// the virtual list up to a specific article (e.g. one opened from search that
/// lives far below the first loaded page) and scroll it into view.
pub fn article_index(
    conn: &Connection,
    query: &ArticleQuery,
    unread_only: bool,
    oldest_first: bool,
    article_id: i64,
) -> AppResult<Option<i64>> {
    let (where_clauses, mut binds) = article_filter(query, unread_only);
    let sql = format!(
        "SELECT pos FROM (
             SELECT a.id AS aid,
                    ROW_NUMBER() OVER (ORDER BY {order}) - 1 AS pos
             FROM articles a JOIN feeds f ON f.id = a.feed_id
             WHERE {where_sql}
         ) WHERE aid = ?",
        order = article_order(oldest_first),
        where_sql = where_clauses.join(" AND "),
    );
    binds.push(Value::Integer(article_id));
    let pos = conn
        .prepare(&sql)?
        .query_row(params_from_iter(binds), |r| r.get::<_, i64>(0))
        .optional()?;
    Ok(pos)
}

pub fn list_articles(
    conn: &Connection,
    query: &ArticleQuery,
    unread_only: bool,
    search: Option<&str>,
    oldest_first: bool,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ArticleSummary>> {
    let (mut where_clauses, mut binds) = article_filter(query, unread_only);

    let searching = search.map(|s| !s.trim().is_empty()).unwrap_or(false);
    let mut sql = format!(
        "SELECT a.id, a.feed_id, f.title, f.source_type, a.title, a.author,
                substr(a.body_text,1,{snippet_len}), a.image_url, a.url, a.published_at,
                a.is_read, a.is_starred, a.read_later
         FROM articles a JOIN feeds f ON f.id = a.feed_id ",
        snippet_len = PREVIEW_SNIPPET_CHARS,
    );
    if searching {
        sql.push_str("JOIN articles_fts fts ON fts.rowid = a.id ");
        where_clauses.push("articles_fts MATCH ?".into());
        // Explicit search: every typed word must match (AND), so results
        // narrow as the user adds terms.
        binds.push(Value::Text(fts_query(search.unwrap(), false)));
    }
    sql.push_str("WHERE ");
    sql.push_str(&where_clauses.join(" AND "));
    // Sort by the effective date — COALESCE(published_at, fetched_at) — so
    // an article with no feed-supplied date orders by when it arrived rather
    // than sinking to the bottom. The two columns are stored in different
    // textual formats (`published_at` RFC 3339 with a `T`; `fetched_at` the
    // space form), so a raw string compare mis-orders a list mixing both;
    // `datetime()` normalises each side to a single comparable form. Backed
    // by `idx_articles_sort`, an expression index over the same wrapped
    // expression (v12) — the planner uses it for both directions, no sort.
    if searching {
        sql.push_str(" ORDER BY fts.rank ");
    } else {
        sql.push_str(" ORDER BY ");
        sql.push_str(article_order(oldest_first));
        sql.push(' ');
    }
    sql.push_str("LIMIT ? OFFSET ?");
    binds.push(Value::Integer(limit));
    binds.push(Value::Integer(offset));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(binds), |r| {
            Ok(ArticleSummary {
                id: r.get(0)?,
                feed_id: r.get(1)?,
                feed_title: r.get(2)?,
                source_type: r.get(3)?,
                title: r.get(4)?,
                author: r.get(5)?,
                snippet: r.get(6)?,
                image_url: r.get(7)?,
                url: r.get(8)?,
                published_at: r.get(9)?,
                is_read: r.get(10)?,
                is_starred: r.get(11)?,
                read_later: r.get(12)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Scan stored articles that have no thumbnail but whose feed or extracted
/// body HTML embeds an image, returning the `(id, image_url)` pairs to adopt.
/// Reads only — paired with `apply_card_images` so the caller can run this
/// heavy parse on a reader connection and the quick writes under the writer
/// lock. One-time: feeds ingested after the body-image fallback shipped
/// already store this at parse.
pub fn card_image_backfill_scan(conn: &Connection) -> AppResult<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, content_html, extracted_html FROM articles
         WHERE (image_url IS NULL OR trim(image_url) = '')
           AND (
                (content_html IS NOT NULL AND content_html <> '')
                OR (extracted_html IS NOT NULL AND extracted_html <> '')
           )",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, content_html, extracted_html) = row?;
        let img = content_html
            .as_deref()
            .and_then(crate::sanitize::first_image)
            .or_else(|| {
                extracted_html
                    .as_deref()
                    .and_then(crate::sanitize::first_image)
            });
        if let Some(img) = img {
            out.push((id, img));
        }
    }
    Ok(out)
}

/// Persist the `(id, image_url)` pairs found by `card_image_backfill_scan`,
/// in a single transaction.
pub fn apply_card_images(conn: &Connection, updates: &[(i64, String)]) -> AppResult<()> {
    let tx = conn.unchecked_transaction()?;
    for (id, img) in updates {
        tx.execute(
            "UPDATE articles SET image_url = ?2 WHERE id = ?1",
            params![id, img],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Turn raw user text into a safe FTS5 MATCH expression (each term
/// prefix-matched). `or_join` selects how multiple terms combine: `false`
/// joins them with an implicit AND (every term must match — explicit search,
/// where adding words narrows results); `true` joins them with `OR` (any term
/// may match — recall-oriented retrieval, e.g. RAG over a natural-language
/// question, where AND-ing every word would match nothing).
///
/// Punctuation *splits* a word into separate terms rather than being deleted
/// inside it: the `unicode61` tokenizer indexes `rust-lang` / `node.js` as the
/// two tokens `rust`+`lang` and `node`+`js`, so collapsing the query side to
/// `rustlang` / `nodejs` would match nothing the index actually holds.
fn fts_query(input: &str, or_join: bool) -> String {
    let terms: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\"*"))
        .collect();
    if terms.is_empty() {
        "\"\"".into()
    } else if or_join {
        terms.join(" OR ")
    } else {
        terms.join(" ")
    }
}

/// Retrieve up to `limit` articles relevant to a natural-language `question`,
/// for use as RAG context. Uses OR-joined FTS terms so a multi-word question
/// still matches articles that contain *some* of its keywords — an AND join
/// (as explicit search uses) would require every word to appear and so return
/// nothing for a real question. Returns `(id, title, feed_title)` ordered by
/// FTS relevance. An all-stopword / punctuation-only question yields no rows.
pub fn search_articles_for_rag(
    conn: &Connection,
    question: &str,
    limit: i64,
) -> AppResult<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.title, f.title
         FROM articles a
         JOIN feeds f ON f.id = a.feed_id
         JOIN articles_fts fts ON fts.rowid = a.id
         WHERE articles_fts MATCH ?1
         ORDER BY fts.rank
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![fts_query(question, true), limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Recent articles as `(title, feed_title, text)` for building an AI digest.
pub fn digest_source(conn: &Connection, limit: i64) -> AppResult<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT a.title, f.title, substr(a.body_text, 1, 600)
         FROM articles a JOIN feeds f ON f.id = a.feed_id
         ORDER BY datetime(COALESCE(a.published_at, a.fetched_at)) DESC, a.id DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_article(conn: &Connection, id: i64) -> AppResult<ArticleDetail> {
    let mut detail = conn.query_row(
        "SELECT a.id, a.feed_id, f.title, f.source_type, a.title, a.author, a.url,
                a.content_html, a.extracted_html, a.image_url, a.published_at,
                a.is_read, a.is_starred, a.read_later, a.ai_summary,
                a.translated_html, a.translated_lang, a.translated_title
         FROM articles a JOIN feeds f ON f.id = a.feed_id WHERE a.id = ?1",
        params![id],
        |r| {
            Ok(ArticleDetail {
                id: r.get(0)?,
                feed_id: r.get(1)?,
                feed_title: r.get(2)?,
                source_type: r.get(3)?,
                title: r.get(4)?,
                author: r.get(5)?,
                url: r.get(6)?,
                content_html: r.get(7)?,
                extracted_html: r.get(8)?,
                image_url: r.get(9)?,
                published_at: r.get(10)?,
                is_read: r.get(11)?,
                is_starred: r.get(12)?,
                read_later: r.get(13)?,
                ai_summary: r.get(14)?,
                translated_html: r.get(15)?,
                translated_lang: r.get(16)?,
                translated_title: r.get(17)?,
                enclosures: Vec::new(),
                tags: Vec::new(),
            })
        },
    )?;
    let mut stmt =
        conn.prepare("SELECT url, mime_type, length FROM enclosures WHERE article_id = ?1")?;
    detail.enclosures = stmt
        .query_map(params![id], |r| {
            Ok(Enclosure {
                url: r.get(0)?,
                mime_type: r.get(1)?,
                length: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    detail.tags = tags_for_article(conn, id)?;
    Ok(detail)
}

/// `(title, plain_text)` for building an AI prompt. Prefers the extracted
/// full text when the user has run extraction, so a summary / answer covers
/// the whole article rather than the (often truncated) feed body.
pub fn article_preview_text(conn: &Connection, id: i64) -> AppResult<(String, String)> {
    conn.query_row(
        "SELECT title, body_text FROM articles WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(Into::into)
}

pub fn article_text(conn: &Connection, id: i64) -> AppResult<(String, String)> {
    let (title, body, extracted): (String, String, Option<String>) = conn.query_row(
        "SELECT title, body_text, extracted_html FROM articles WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let text = match extracted {
        Some(html) if !html.trim().is_empty() => crate::sanitize::html_to_text(&html),
        _ => body,
    };
    Ok((title, text))
}

pub fn set_read(conn: &Connection, id: i64, read: bool) -> AppResult<()> {
    conn.execute(
        "UPDATE articles SET is_read = ?2 WHERE id = ?1",
        params![id, read],
    )?;
    Ok(())
}

pub fn set_starred(conn: &Connection, id: i64, starred: bool) -> AppResult<()> {
    conn.execute(
        "UPDATE articles SET is_starred = ?2 WHERE id = ?1",
        params![id, starred],
    )?;
    Ok(())
}

pub fn set_read_later(conn: &Connection, id: i64, v: bool) -> AppResult<()> {
    conn.execute(
        "UPDATE articles SET read_later = ?2 WHERE id = ?1",
        params![id, v],
    )?;
    Ok(())
}

/// Store the extracted full-text HTML and re-index the article's FTS body
/// with it, so search covers the whole article rather than just the short
/// summary the feed shipped.
pub fn set_extracted_html(
    conn: &Connection,
    id: i64,
    html: &str,
    image_url: Option<&str>,
) -> AppResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE articles
            SET extracted_html = ?2,
                image_url = CASE
                    WHEN ?3 IS NOT NULL AND (image_url IS NULL OR trim(image_url) = '')
                    THEN ?3
                    ELSE image_url
                END
         WHERE id = ?1",
        params![id, html, image_url],
    )?;
    tx.execute(
        "UPDATE articles_fts SET body = ?2 WHERE rowid = ?1",
        params![id, crate::sanitize::html_to_text(html)],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn set_ai_summary(conn: &Connection, id: i64, summary: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE articles SET ai_summary = ?2 WHERE id = ?1",
        params![id, summary],
    )?;
    Ok(())
}

/// Return a cached list-preview translation for this exact target language and
/// engine, if one exists.
pub fn get_preview_translation(
    conn: &Connection,
    id: i64,
    lang: &str,
    engine: &str,
) -> AppResult<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT title, snippet
               FROM article_preview_translations
              WHERE article_id = ?1 AND lang = ?2 AND engine = ?3",
            params![id, lang, engine],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Cache a list-preview translation independently from full-body translations.
pub fn set_preview_translation(
    conn: &Connection,
    id: i64,
    title: &str,
    snippet: &str,
    lang: &str,
    engine: &str,
) -> AppResult<()> {
    conn.execute(
        "INSERT INTO article_preview_translations(article_id, lang, engine, title, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(article_id, lang, engine) DO UPDATE SET
             title = excluded.title,
             snippet = excluded.snippet",
        params![id, lang, engine, title.trim(), snippet.trim()],
    )?;
    Ok(())
}

/// Cache an article's translated body (and title, when one was produced) for
/// the target language. `title` is `None` when the article had no title to
/// translate or the row predates title translation — the reader then keeps
/// showing the original title instead of a blank one.
pub fn set_translation(
    conn: &Connection,
    id: i64,
    html: &str,
    lang: &str,
    title: Option<&str>,
) -> AppResult<()> {
    conn.execute(
        "UPDATE articles SET translated_html = ?2, translated_lang = ?3,
                              translated_title = ?4 WHERE id = ?1",
        params![id, html, lang, title],
    )?;
    Ok(())
}

/// Mark every article matching the current sidebar selection as read. When
/// `enqueue_sync` is set, the read change is also queued for the sync server
/// — otherwise a bulk mark-all-read never reaches FreshRSS and the next pull
/// silently reverts it.
pub fn mark_all_read(
    conn: &Connection,
    query: &ArticleQuery,
    enqueue_sync: bool,
) -> AppResult<usize> {
    // WHERE fragment selecting the articles in the current view, plus an
    // optional bound id (feed / folder / tag). `pred` is a fixed literal.
    let (pred, id): (&str, Option<i64>) = match query {
        ArticleQuery::All | ArticleQuery::Unread => ("1", None),
        ArticleQuery::Starred => ("is_starred = 1", None),
        ArticleQuery::ReadLater => ("read_later = 1", None),
        ArticleQuery::Feed(id) => ("feed_id = ?1", Some(*id)),
        ArticleQuery::Folder(id) => (
            "feed_id IN (SELECT id FROM feeds WHERE folder_id = ?1)",
            Some(*id),
        ),
        ArticleQuery::Tag(id) => (
            "id IN (SELECT article_id FROM article_tags WHERE tag_id = ?1)",
            Some(*id),
        ),
    };
    let bind: Vec<&dyn rusqlite::ToSql> = id.iter().map(|v| v as &dyn rusqlite::ToSql).collect();

    // Queue + flip together: the sync-queue rows and the is_read change must
    // commit atomically, or a mid-way failure leaves the queue claiming a
    // read state the articles never reached. Queue *before* flipping so the
    // `is_read = 0` filter still matches; the SELECT's WHERE also
    // disambiguates the ON CONFLICT clause.
    //
    // Articles are queued regardless of whether they already carry a
    // `remote_id`: freshly fetched items have none until a pull matches them
    // by URL, and "mark all read" is most often run right after a refresh on
    // exactly those items. `take_sync_queue` defers any entry whose article
    // still lacks a remote id, so the change pushes on the sync after the id
    // is assigned — mirroring the single-article `enqueue_sync` path. The old
    // `remote_id IS NOT NULL` filter here silently dropped those changes.
    let tx = conn.unchecked_transaction()?;
    if enqueue_sync {
        tx.execute(
            &format!(
                "INSERT INTO sync_queue(article_id, field, value)
                 SELECT id, 'read', 1 FROM articles
                 WHERE {pred} AND is_read = 0
                 ON CONFLICT(article_id, field) DO UPDATE SET value = 1"
            ),
            bind.as_slice(),
        )?;
    }
    let n = tx.execute(
        &format!("UPDATE articles SET is_read = 1 WHERE {pred} AND is_read = 0"),
        bind.as_slice(),
    )?;
    tx.commit()?;
    Ok(n)
}

/// Whether a FreshRSS server is currently linked (a non-empty URL is stored).
pub fn is_freshrss_connected(conn: &Connection) -> bool {
    get_setting(conn, "freshrss_url")
        .ok()
        .flatten()
        .map(|u| !u.trim().is_empty())
        .unwrap_or(false)
}

// ─────────────────────────── tags ───────────────────────────

/// Palette keys cycled through as new tags are created.
const TAG_COLORS: &[&str] = &[
    "clay", "amber", "pine", "teal", "indigo", "violet", "rose", "slate",
];

/// Every tag, ordered for the sidebar, with a live article count.
pub fn list_tags(conn: &Connection) -> AppResult<Vec<Tag>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.color, t.position,
                (SELECT COUNT(*) FROM article_tags at WHERE at.tag_id = t.id)
         FROM tags t ORDER BY t.position, t.name COLLATE NOCASE",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
                color: r.get(2)?,
                position: r.get(3)?,
                article_count: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Create a tag, auto-assigning the next palette colour and list position.
///
/// Idempotent on name: `tags.name` is `UNIQUE`, so a plain `INSERT` of an
/// existing name would fail the constraint. Both call sites — the sidebar's
/// "new tag" prompt and the reader's create-and-attach picker — treat a name
/// the user typed, which may already exist (the picker even lists every tag
/// right above the input). Matching is case-insensitive, consistent with how
/// `list_tags` orders names, so "Rust" and "rust" resolve to one tag rather
/// than silently diverging. An existing name returns that tag's id instead of
/// erroring.
pub fn create_tag(conn: &Connection, name: &str) -> AppResult<i64> {
    // Trim before the dedup lookup and the insert: a name with surrounding
    // whitespace is a distinct string from its trimmed twin, so the
    // `COLLATE NOCASE` lookup would miss the existing tag and spawn a
    // near-duplicate. Normalise at this one chokepoint so the invariant holds
    // regardless of caller-side trimming. Mirrors `create_folder`.
    let name = name.trim();
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    // Position the new tag at the end of the list. `MAX(position)+1` — not
    // `COUNT(*)` — is required: deleting a tag from the middle leaves a gap,
    // so a fresh `COUNT(*)` would collide with an existing tag's position and
    // the new tag would not sort last (only the name tiebreaker would save
    // it). The colour cycles off the same index so the palette stays varied.
    let next: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM tags",
        [],
        |r| r.get(0),
    )?;
    let color = TAG_COLORS[(next as usize) % TAG_COLORS.len()];
    conn.execute(
        "INSERT INTO tags(name, color, position) VALUES (?1, ?2, ?3)",
        params![name, color, next],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Rename a tag, rejecting a name that collides with a *different* existing
/// tag.
///
/// `tags.name` is `UNIQUE` (case-sensitively). Without this guard a rename to a
/// name that exactly matches another tag would fail the constraint and surface
/// the raw SQLite message to the user; and a rename to a *case variant* of
/// another tag ("rust" → "Rust") would succeed and create the near-duplicate
/// that `create_tag` deliberately collapses — leaving the two functions
/// inconsistent. Match case-insensitively, the same basis `create_tag` and
/// `list_tags`'s ordering use, and return a localisable `tagNameExists` code.
/// Renaming a tag to its own current name (or a case change of it) is allowed.
pub fn rename_tag(conn: &Connection, id: i64, name: &str) -> AppResult<()> {
    // Trim so the collision check and the stored value match what `create_tag`
    // would produce — otherwise a rename to a whitespace-padded variant slips
    // past the clash test and recreates the near-duplicate.
    let name = name.trim();
    let clash: Option<i64> = conn
        .query_row(
            "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE AND id != ?2",
            params![name, id],
            |r| r.get(0),
        )
        .optional()?;
    if clash.is_some() {
        return Err(AppError::code("tagNameExists"));
    }
    conn.execute("UPDATE tags SET name = ?2 WHERE id = ?1", params![id, name])?;
    Ok(())
}

pub fn set_tag_color(conn: &Connection, id: i64, color: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE tags SET color = ?2 WHERE id = ?1",
        params![id, color],
    )?;
    Ok(())
}

/// Persist a new tag ordering — `ids` listed in the desired display order.
/// The per-row updates run in one transaction so a mid-loop failure can't
/// leave the tag list in a half-reordered state.
pub fn reorder_tags(conn: &Connection, ids: &[i64]) -> AppResult<()> {
    let tx = conn.unchecked_transaction()?;
    for (pos, id) in ids.iter().enumerate() {
        tx.execute(
            "UPDATE tags SET position = ?2 WHERE id = ?1",
            params![id, pos as i64],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn delete_tag(conn: &Connection, id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM tags WHERE id = ?1", params![id])?;
    Ok(())
}

/// Attach (`on = true`) or detach a tag from one article.
pub fn set_article_tag(conn: &Connection, article_id: i64, tag_id: i64, on: bool) -> AppResult<()> {
    if on {
        conn.execute(
            "INSERT INTO article_tags(article_id, tag_id) VALUES (?1, ?2)
             ON CONFLICT DO NOTHING",
            params![article_id, tag_id],
        )?;
    } else {
        conn.execute(
            "DELETE FROM article_tags WHERE article_id = ?1 AND tag_id = ?2",
            params![article_id, tag_id],
        )?;
    }
    Ok(())
}

/// Tags attached to one article (article_count left at 0 — unused per-article).
pub fn tags_for_article(conn: &Connection, article_id: i64) -> AppResult<Vec<Tag>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.color, t.position
         FROM tags t JOIN article_tags at ON at.tag_id = t.id
         WHERE at.article_id = ?1 ORDER BY t.position, t.name COLLATE NOCASE",
    )?;
    let rows = stmt
        .query_map(params![article_id], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
                color: r.get(2)?,
                position: r.get(3)?,
                article_count: 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ─────────────────────────── filter rules ───────────────────────────

fn row_to_rule(r: &rusqlite::Row) -> rusqlite::Result<Rule> {
    let field: String = r.get(4)?;
    let query: String = r.get(5)?;
    // A SQL NULL document means a pre-v24 keyword rule; a non-NULL value that
    // fails to parse means the document was corrupted (hand-edited, partial
    // write) and must NOT be resurrected from the `field`/`query` summary —
    // those columns are derived from the document, so rebuilding from them
    // would silently change what the rule matches (a corrupted "title
    // contains rust AND author eq bob" whose summary is `any / rust,bob`
    // would start skipping every Rust article again). Corrupt → inert until
    // the rule is re-saved; matching treats an empty document as no-match.
    let conditions = match r.get::<_, Option<String>>(8)? {
        None => query
            .split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(|term| {
                vec![RuleCondition {
                    field: field.clone(),
                    operator: "contains".to_string(),
                    value: term.to_string(),
                }]
            })
            .collect(),
        Some(doc) => serde_json::from_str(&doc).unwrap_or_else(|e| {
            log::warn!(
                "rule {}: conditions JSON is corrupt ({e}); the rule matches nothing until it is re-saved",
                r.get::<_, i64>(0).unwrap_or_default()
            );
            Vec::new()
        }),
    };
    let rewrites = parse_rewrites(r.get(9)?);
    Ok(Rule {
        id: r.get(0)?,
        name: r.get(1)?,
        enabled: r.get(2)?,
        feed_id: r.get(3)?,
        conditions,
        action: r.get(6)?,
        rewrites,
        position: r.get(7)?,
    })
}

/// Parse a stored rewrite list (JSON `Vec<RuleRewrite>`); anything malformed
/// degrades to "no rewrites" like a corrupt conditions document.
fn parse_rewrites(json: Option<String>) -> Vec<RuleRewrite> {
    json.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

const RULE_COLS: &str =
    "id, name, enabled, feed_id, field, query, action, position, conditions, rewrite_rules";

/// Flatten a When-document into the legacy `field`/`query` summary columns:
/// the first condition's field (or `any` when fields are mixed) and every
/// condition value comma-joined. Purely informational — matching only reads
/// the JSON document, and a value containing a comma is ambiguous here —
/// but it keeps `sqlite3` inspection of the `rules` table readable.
fn keyword_summary(conditions: &[Vec<RuleCondition>]) -> (String, String) {
    let flat: Vec<&RuleCondition> = conditions.iter().flatten().collect();
    let Some(first) = flat.first() else {
        return ("title".to_string(), String::new());
    };
    let field = if flat.iter().all(|c| c.field == first.field) {
        first.field.clone()
    } else {
        "any".to_string()
    };
    let query = flat
        .iter()
        .map(|c| c.value.as_str())
        .collect::<Vec<_>>()
        .join(",");
    (field, query)
}

/// Every rule, enabled or not, ordered for the settings list.
pub fn list_rules(conn: &Connection) -> AppResult<Vec<Rule>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RULE_COLS} FROM rules ORDER BY position, id"
    ))?;
    let rows = stmt
        .query_map([], row_to_rule)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Only the enabled rules — the set evaluated against incoming articles.
pub fn active_rules(conn: &Connection) -> AppResult<Vec<Rule>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RULE_COLS} FROM rules WHERE enabled = 1 ORDER BY position, id"
    ))?;
    let rows = stmt
        .query_map([], row_to_rule)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn create_rule(
    conn: &Connection,
    name: &str,
    feed_id: Option<i64>,
    conditions: &[Vec<RuleCondition>],
    action: &str,
    rewrites: &[RuleRewrite],
) -> AppResult<i64> {
    // Position the new rule at the end. `MAX(position)+1` — not `COUNT(*)` —
    // is required: deleting a rule from the middle leaves a gap, so a fresh
    // `COUNT(*)` would collide with an existing rule's position and the new
    // rule would not sort last (`ORDER BY position, id` would then slot it
    // before any later-positioned rule). Rule order is semantically load-
    // bearing — `active_rules` evaluates in this order and a `skip` match
    // short-circuits — so a stale position can change which action fires.
    let next: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM rules",
        [],
        |r| r.get(0),
    )?;
    let (field, query) = keyword_summary(conditions);
    conn.execute(
        "INSERT INTO rules(name, feed_id, field, query, action, position, conditions, rewrite_rules)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            name,
            feed_id,
            field,
            query,
            action,
            next,
            serde_json::to_string(conditions).unwrap_or_default(),
            serde_json::to_string(rewrites).unwrap_or_default(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

#[allow(clippy::too_many_arguments)]
pub fn update_rule(
    conn: &Connection,
    id: i64,
    name: &str,
    enabled: bool,
    feed_id: Option<i64>,
    conditions: &[Vec<RuleCondition>],
    action: &str,
    rewrites: &[RuleRewrite],
) -> AppResult<()> {
    let (field, query) = keyword_summary(conditions);
    conn.execute(
        "UPDATE rules SET name = ?2, enabled = ?3, feed_id = ?4,
                          field = ?5, query = ?6, action = ?7,
                          conditions = ?8, rewrite_rules = ?9
         WHERE id = ?1",
        params![
            id,
            name,
            enabled,
            feed_id,
            field,
            query,
            action,
            serde_json::to_string(conditions).unwrap_or_default(),
            serde_json::to_string(rewrites).unwrap_or_default(),
        ],
    )?;
    Ok(())
}

pub fn delete_rule(conn: &Connection, id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM rules WHERE id = ?1", params![id])?;
    Ok(())
}

/// Build the `WHERE` fragment (and its bind values) that selects the articles
/// a rule's When-document matches, optionally scoped to one feed. Columns are
/// *unaliased* so the fragment slots straight into a bare `SELECT … FROM
/// articles`, `UPDATE articles` or `DELETE FROM articles`. Returns `None` when
/// the document holds no usable group (a no-op rule), so callers can
/// short-circuit.
///
/// `preview_rule` (count + samples) and `apply_rule_to_existing` (act) share
/// this builder so the number the preview shows is exactly the set the apply
/// touches. Semantics mirror `condition_matches` line for line: `unicode_lower`
/// (not SQLite's ASCII-only `LOWER`) reproduces the Unicode-aware
/// `to_lowercase()` the ingestion matcher folds with — otherwise a needle like
/// `café` would be counted here but its `CAFÉ` articles missed, diverging the
/// preview/apply from live ingestion; `regexp(?, col)` shares the memoised
/// compiler, and negations AND their NOT-tests across all of a field's
/// columns exactly as the matcher quantifies over all sources.
fn rule_match_where(
    conditions: &[Vec<RuleCondition>],
    feed_id: Option<i64>,
) -> Option<(String, Vec<Value>)> {
    let mut binds: Vec<Value> = Vec::new();
    let groups: Vec<String> = conditions
        .iter()
        .filter(|g| !g.is_empty())
        .map(|group| {
            let parts: Vec<String> = group
                .iter()
                .map(|c| condition_where(c, &mut binds))
                .collect();
            format!("({})", parts.join(" AND "))
        })
        .collect();
    if groups.is_empty() {
        return None;
    }
    let mut where_sql = format!("({})", groups.join(" OR "));
    if let Some(fid) = feed_id {
        where_sql.push_str(" AND feed_id = ?");
        binds.push(Value::Integer(fid));
    }
    Some((where_sql, binds))
}

/// The SQL for one condition, pushing its bind values onto `binds`. Unknown
/// fields/operators and empty needles produce a literal `0` — the SQL twin of
/// `condition_matches` returning false, keeping hand-edited rows inert on both
/// sides. Within one condition the field's columns combine by the operator's
/// quantifier: positive operators (`contains`, `eq`, `regex`) OR across them
/// ("some column matches"), negations (`not_contains`, `not_eq`) AND across
/// them ("no column matches").
fn condition_where(c: &RuleCondition, binds: &mut Vec<Value>) -> String {
    if c.value.trim().is_empty() {
        return "0".to_string();
    }
    let cols: &[&str] = match c.field.as_str() {
        "title" => &["title"],
        "author" => &["author"],
        "content" => &["body_text"],
        "url" => &["url"],
        "any" => &["title", "author", "body_text"],
        _ => return "0".to_string(),
    };
    let mut parts: Vec<String> = Vec::new();
    match c.operator.as_str() {
        "contains" | "not_contains" => {
            // SQLite may truncate a LIKE pattern at an embedded NUL, which
            // would silently widen the match; keep such needles inert here
            // exactly as `condition_matches` does (eq / not_eq / regex don't
            // use LIKE and are unaffected).
            if c.value.contains('\0') {
                return "0".to_string();
            }
            let escaped = c.value.to_lowercase();
            let escaped = escaped
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            for col in cols {
                binds.push(Value::Text(format!("%{escaped}%")));
                let neg = if c.operator == "not_contains" {
                    "NOT "
                } else {
                    ""
                };
                parts.push(format!(
                    "unicode_lower(COALESCE({col},'')) {neg}LIKE ? ESCAPE '\\'"
                ));
            }
        }
        "eq" | "not_eq" => {
            for col in cols {
                binds.push(Value::Text(c.value.to_lowercase()));
                let neg = if c.operator == "not_eq" { "!=" } else { "=" };
                parts.push(format!("unicode_lower(COALESCE({col},'')) {neg} ?"));
            }
        }
        "regex" => {
            for col in cols {
                binds.push(Value::Text(c.value.clone()));
                parts.push(format!("regexp(?, COALESCE({col},''))"));
            }
        }
        _ => return "0".to_string(),
    }
    let joiner = if c.operator.starts_with("not_") {
        " AND "
    } else {
        " OR "
    };
    if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        format!("({})", parts.join(joiner))
    }
}

/// Preview how a draft rule would behave: the number of *already-stored*
/// articles its When-document matches, plus a handful of recent sample titles.
/// Lets the user sanity-check a rule before saving it.
pub fn preview_rule(
    conn: &Connection,
    feed_id: Option<i64>,
    conditions: &[Vec<RuleCondition>],
) -> AppResult<(i64, Vec<String>)> {
    let Some((where_sql, binds)) = rule_match_where(conditions, feed_id) else {
        return Ok((0, Vec::new()));
    };
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM articles WHERE {where_sql}"),
        params_from_iter(binds.iter().cloned()),
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(&format!(
        "SELECT title FROM articles WHERE {where_sql}
         ORDER BY datetime(COALESCE(published_at, fetched_at)) DESC, id DESC
         LIMIT 5"
    ))?;
    let samples = stmt
        .query_map(params_from_iter(binds), |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok((count, samples))
}

/// The open-time rule actions (`translate` / `summarize` / `fulltext` /
/// `open_original`) the reader should honour for one article: the union over
/// every *enabled* rule whose When-document matches the stored row, in rule
/// order. This is the read-side twin of [`article_precheck`] — open-time
/// actions never touch ingestion (they would either cost network / LLM calls
/// for every landed article or make no sense at all), so they are resolved
/// here, the moment a matching article is opened. Evaluating the live rules
/// against the row — through the same [`rule_match_where`] builder the
/// preview and the backfill use — means editing a rule takes effect for every
/// matching article, old or new, with no backfill and no per-article state.
///
/// Like the preview and the backfill, matching runs against the *stored* row,
/// i.e. the text after any `rewrite` rules have applied at ingestion — the
/// stored twin of ingestion's match-the-original-text guarantee. A rewrite
/// that added or removed a later rule's needle therefore shifts what the
/// open-time rule sees, exactly as it already shifts what the preview shows.
pub fn rule_actions_for_article(conn: &Connection, article_id: i64) -> AppResult<Vec<String>> {
    let mut actions: Vec<String> = Vec::new();
    for rule in active_rules(conn)? {
        if !RULE_OPEN_ACTIONS.contains(&rule.action.as_str()) {
            // Ingestion-time action: by the time a row exists it has already
            // acted (or dropped the row), so there is nothing to re-derive.
            continue;
        }
        let Some((where_sql, binds)) = rule_match_where(&rule.conditions, rule.feed_id) else {
            continue;
        };
        let hits: bool = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM articles WHERE ({where_sql}) AND id = ?)"),
            params_from_iter(binds.iter().cloned().chain([Value::Integer(article_id)])),
            |r| r.get(0),
        )?;
        if hits && !actions.contains(&rule.action) {
            actions.push(rule.action);
        }
    }
    Ok(actions)
}

/// Apply a saved rule's action to the articles already in the store that it
/// matches — the one-time backfill run when a rule is created or edited so it
/// affects the existing backlog, not only articles fetched afterwards. Returns
/// the number of articles acted on.
///
/// `skip` *deletes* its matches — the stored-article equivalent of dropping the
/// article at ingestion. FK `ON DELETE CASCADE` (enclosures, highlights, tags)
/// and the `articles_fts_ad` trigger keep dependent rows and the FTS index in
/// sync, the same path retention cleanup relies on. `read` / `star` set the
/// matching flag and skip rows that already carry it, so the returned count is
/// the number of rows actually changed — and so does `rewrite`, whose
/// changed-row set is tracked explicitly (`INSTR` containment, the same
/// literal containment `REPLACE()` applies). `rewrite` replaces literal from→to
/// pairs in the matched articles' text (title / summary / HTML body / plain
/// body), the stored twin of [`apply_rewrites`] at ingestion; no UPDATE
/// trigger keeps `articles_fts` fresh, so the matched rows' FTS entries are
/// rebuilt from the updated columns afterwards. The open-time actions
/// (`translate` / `summarize` / `fulltext` / `open_original`) fall to the
/// catch-all and backfill nothing: they are reader behaviours resolved at
/// open time ([`rule_actions_for_article`]), which by design covers the
/// existing backlog the moment the rules change.
pub fn apply_rule_to_existing(
    conn: &Connection,
    feed_id: Option<i64>,
    conditions: &[Vec<RuleCondition>],
    action: &str,
    rewrites: &[RuleRewrite],
) -> AppResult<usize> {
    let Some((where_sql, binds)) = rule_match_where(conditions, feed_id) else {
        return Ok(0);
    };
    match action {
        "skip" => {
            let sql = format!("DELETE FROM articles WHERE {where_sql}");
            Ok(conn.execute(&sql, params_from_iter(binds))?)
        }
        "read" | "star" => {
            let flag = if action == "read" {
                "is_read"
            } else {
                "is_starred"
            };
            let sql = format!("UPDATE articles SET {flag} = 1 WHERE ({where_sql}) AND {flag} = 0");
            Ok(conn.execute(&sql, params_from_iter(binds))?)
        }
        "rewrite" => {
            // Snapshot the matching ids first: the UPDATE statements below
            // re-evaluate their WHERE per statement, and an earlier rewrite
            // pair could add or remove a later pair's needle from a row —
            // each pair must act on exactly the set the rule matched (the
            // stored twin of ingestion, which matches once and then applies
            // every rewrite). Temp tables hold the snapshot and the subset
            // actually changed; everything commits or rolls back together.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TEMP TABLE rule_hits (id INTEGER PRIMARY KEY);
                 CREATE TEMP TABLE rule_changed (id INTEGER PRIMARY KEY);",
            )?;
            tx.execute(
                &format!("INSERT INTO rule_hits(id) SELECT id FROM articles WHERE {where_sql}"),
                params_from_iter(binds.iter().cloned()),
            )?;
            // The literal-containment predicate for one rewrite needle,
            // evaluated against an `articles` row bound to the alias `a`.
            let needle_hit = "INSTR(a.title, ?1) > 0 \
                        OR INSTR(COALESCE(a.summary, ''), ?1) > 0 \
                        OR INSTR(COALESCE(a.content_html, ''), ?1) > 0 \
                        OR INSTR(COALESCE(a.body_text, ''), ?1) > 0";
            for rw in rewrites {
                if rw.from.is_empty() {
                    continue;
                }
                if rw.from == rw.to {
                    // A no-op pair: skipping it keeps the backfill in
                    // lockstep with the ingestion path and keeps a
                    // needle-hit on a from==to pair from counting a row as
                    // changed or invalidating its caches.
                    continue;
                }
                // Record which rows this pair will actually change — `INSTR`
                // is the same literal, case-sensitive containment `REPLACE()`
                // applies — so derived caches are only rebuilt where the
                // original text really changed and the returned count means
                // "rows changed", like the read / star branches.
                tx.execute(
                    // Explicitly joined/qualified: bare column names would
                    // resolve against `rule_hits`, which holds only ids.
                    &format!(
                        "INSERT OR IGNORE INTO rule_changed(id)
                         SELECT h.id FROM rule_hits h
                         JOIN articles a ON a.id = h.id
                         WHERE {needle_hit}"
                    ),
                    params![rw.from],
                )?;
                // Literal replacement on the same four text fields
                // `apply_rewrites` touches at ingestion. SQLite's `REPLACE()`
                // is byte-literal and case-sensitive — never a regex — which
                // is exactly why rewrite pairs are literal by design.
                // `REPLACE(NULL, …)` yields NULL, so nullable text columns
                // keep their NULL when the row is untouched by the needle —
                // the stored twin of `apply_rewrites`, where `None` stays
                // `None`. The containment guard keeps untouched rows from
                // being updated at all — an UPDATE that lists title /
                // body_text fires the v17 preview-translation trigger for
                // every row it touches, changed or not.
                tx.execute(
                    &format!(
                        "UPDATE articles SET
                            title        = REPLACE(title, ?1, ?2),
                            summary      = REPLACE(summary, ?1, ?2),
                            content_html = REPLACE(content_html, ?1, ?2),
                            body_text    = REPLACE(body_text, ?1, ?2)
                         WHERE id IN (SELECT id FROM rule_hits)
                           AND id IN (SELECT a.id FROM rule_hits h
                                      JOIN articles a ON a.id = h.id
                                      WHERE {needle_hit})"
                    ),
                    params![rw.from, rw.to],
                )?;
            }
            // No UPDATE trigger maintains articles_fts, so the changed rows'
            // entries need rebuilding — but the body column only where the
            // indexed body *is* the feed text: rows with `extracted_html`
            // were indexed from the extracted full text (`set_extracted_html`
            // writes `html_to_text(extracted_html)`), which rewrites never
            // touch, so their indexed body must survive the title rewrite.
            tx.execute(
                "UPDATE articles_fts SET
                    title = (SELECT title     FROM articles WHERE id = articles_fts.rowid),
                    body  = (SELECT body_text FROM articles WHERE id = articles_fts.rowid)
                 WHERE rowid IN (SELECT id FROM rule_changed)
                   AND rowid IN (SELECT id FROM articles WHERE extracted_html IS NULL)",
                [],
            )?;
            tx.execute(
                "UPDATE articles_fts SET
                    title = (SELECT title FROM articles WHERE id = articles_fts.rowid)
                 WHERE rowid IN (SELECT id FROM rule_changed)
                   AND rowid IN (SELECT id FROM articles WHERE extracted_html IS NOT NULL)",
                [],
            )?;
            // The cached translation and AI summary are derivatives of the
            // original text: drop them so the reader never shows a rewritten
            // original beside a pre-rewrite rendering. (The v17
            // `article_preview_translations_au` trigger already cleared the
            // list-preview cache when title / body_text were updated above.)
            tx.execute(
                "UPDATE articles SET
                    translated_html = NULL, translated_lang = NULL,
                    translated_title = NULL, ai_summary = NULL
                 WHERE id IN (SELECT id FROM rule_changed)",
                [],
            )?;
            let changed: i64 =
                tx.query_row("SELECT COUNT(*) FROM rule_changed", [], |r| r.get(0))?;
            tx.execute_batch("DROP TABLE rule_changed; DROP TABLE rule_hits;")?;
            tx.commit()?;
            Ok(changed as usize)
        }
        _ => Ok(0),
    }
}

/// (total unread, starred, read-later) counts for the sidebar smart folders.
pub fn smart_counts(conn: &Connection) -> AppResult<(i64, i64, i64)> {
    Ok(conn.query_row(
        "SELECT
            (SELECT COUNT(*) FROM articles WHERE is_read = 0),
            (SELECT COUNT(*) FROM articles WHERE is_starred = 1),
            (SELECT COUNT(*) FROM articles WHERE read_later = 1)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

// ─────────────────────────── highlights ───────────────────────────

const HIGHLIGHT_COLS: &str =
    "id, article_id, quote, prefix, suffix, text_offset, color, note, created_at";

fn row_to_highlight(r: &rusqlite::Row) -> rusqlite::Result<Highlight> {
    Ok(Highlight {
        id: r.get(0)?,
        article_id: r.get(1)?,
        quote: r.get(2)?,
        prefix: r.get(3)?,
        suffix: r.get(4)?,
        text_offset: r.get(5)?,
        color: r.get(6)?,
        note: r.get(7)?,
        created_at: r.get(8)?,
    })
}

/// The fields needed to create a highlight — everything in [`Highlight`]
/// except the database-assigned `id` and `created_at`. Grouping the anchor
/// fields (which are all `&str` and otherwise trivially swappable) into one
/// named value keeps `insert_highlight` calls unambiguous.
pub struct NewHighlight<'a> {
    pub article_id: i64,
    pub quote: &'a str,
    pub prefix: &'a str,
    pub suffix: &'a str,
    pub text_offset: i64,
    pub color: &'a str,
    pub note: &'a str,
}

/// Insert a highlight and return its new id.
pub fn insert_highlight(conn: &Connection, h: &NewHighlight) -> AppResult<i64> {
    conn.execute(
        "INSERT INTO highlights(article_id, quote, prefix, suffix, text_offset, color, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            h.article_id,
            h.quote,
            h.prefix,
            h.suffix,
            h.text_offset,
            h.color,
            h.note
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// All highlights for one article, oldest first (their reading order).
pub fn list_highlights(conn: &Connection, article_id: i64) -> AppResult<Vec<Highlight>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {HIGHLIGHT_COLS} FROM highlights
         WHERE article_id = ?1 ORDER BY text_offset, id"
    ))?;
    let rows = stmt
        .query_map(params![article_id], row_to_highlight)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every highlight across all articles — used by the Highlights browser.
pub fn list_all_highlights(conn: &Connection) -> AppResult<Vec<Highlight>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {HIGHLIGHT_COLS} FROM highlights ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt
        .query_map([], row_to_highlight)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Fetch one highlight by id, if it exists.
#[allow(dead_code)] // exercised by the db tests; kept as a complete CRUD API.
pub fn get_highlight(conn: &Connection, id: i64) -> AppResult<Option<Highlight>> {
    Ok(conn
        .query_row(
            &format!("SELECT {HIGHLIGHT_COLS} FROM highlights WHERE id = ?1"),
            params![id],
            row_to_highlight,
        )
        .optional()?)
}

/// Replace a highlight's note text (an empty string clears it).
pub fn update_highlight_note(conn: &Connection, id: i64, note: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE highlights SET note = ?2 WHERE id = ?1",
        params![id, note],
    )?;
    Ok(())
}

/// Change a highlight's colour (a palette key).
pub fn set_highlight_color(conn: &Connection, id: i64, color: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE highlights SET color = ?2 WHERE id = ?1",
        params![id, color],
    )?;
    Ok(())
}

pub fn delete_highlight(conn: &Connection, id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM highlights WHERE id = ?1", params![id])?;
    Ok(())
}

// ─────────────────────────── settings ───────────────────────────

pub fn get_setting(conn: &Connection, key: &str) -> AppResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> AppResult<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Atomically switch AI profiles, preserving legacy and explicitly empty values.
/// Supplying a model and URL also updates the selected profile in one commit.
pub fn configure_ai_provider(
    conn: &Connection,
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
) -> AppResult<(String, String)> {
    if !matches!(provider, "anthropic" | "openai" | "deepseek") {
        return Err(AppError::other("invalid AI provider"));
    }
    let tx = conn.unchecked_transaction()?;
    let previous = get_setting(&tx, "ai_provider")?.unwrap_or_default();
    let previous = match previous.as_str() {
        "openai" => "openai",
        "deepseek" => "deepseek",
        _ => "anthropic",
    };
    for key in ["ai_model", "ai_base_url"] {
        let value = get_setting(&tx, key)?.unwrap_or_default();
        set_setting(&tx, &format!("{key}_{previous}"), &value)?;
    }
    let model = model
        .map(str::to_owned)
        .unwrap_or(get_setting(&tx, &format!("ai_model_{provider}"))?.unwrap_or_default());
    let base_url = base_url
        .map(str::to_owned)
        .unwrap_or(get_setting(&tx, &format!("ai_base_url_{provider}"))?.unwrap_or_default());
    for (key, value) in [
        ("ai_model", model.as_str()),
        ("ai_base_url", base_url.as_str()),
    ] {
        set_setting(&tx, key, value)?;
        set_setting(&tx, &format!("{key}_{provider}"), value)?;
    }
    set_setting(&tx, "ai_provider", provider)?;
    tx.commit()?;
    Ok((model, base_url))
}

/// Read a setting and parse it as `T`, falling back to `default` when the key
/// is missing, unreadable, or fails to parse.
pub fn setting_parsed<T: std::str::FromStr>(conn: &Connection, key: &str, default: T) -> T {
    get_setting(conn, key)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Read a setting as a boolean flag — `"1"` and `"true"` are true, anything
/// else (including a missing key) falls back to `default`.
pub fn setting_flag(conn: &Connection, key: &str, default: bool) -> bool {
    get_setting(conn, key)
        .ok()
        .flatten()
        .map(|v| v == "1" || v == "true")
        .unwrap_or(default)
}

// ─────────────────────────── storage ───────────────────────────

/// `(database bytes, article count, feed count)` for the storage panel.
pub fn storage_stats(conn: &Connection) -> AppResult<(i64, i64, i64)> {
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let articles: i64 = conn.query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))?;
    let feeds: i64 = conn.query_row("SELECT COUNT(*) FROM feeds", [], |r| r.get(0))?;
    Ok((page_count * page_size, articles, feeds))
}

/// Delete read articles older than `days`, keeping starred / read-later ones.
/// Returns the number removed. Age is the effective date —
/// COALESCE(published_at, fetched_at) — so a dateless article is retained by
/// fetch age rather than living forever (fetched_at is never NULL).
///
/// The two timestamp columns are stored in different textual formats:
/// `published_at` is RFC 3339 (`2024-01-15T10:30:00+00:00`, written by
/// `to_rfc3339`) while `fetched_at` and `datetime('now', …)` use SQLite's
/// space-separated form (`2024-01-15 10:30:00`). A raw string `<` mis-orders
/// them — the `T` byte sorts *after* a space, so a `published_at` value looks
/// almost a day newer than it is and same-day articles escape the cutoff.
/// Wrapping every side in `datetime()` parses both formats to the canonical
/// representation, so the comparison reflects the real instant.
pub fn cleanup_old_articles(conn: &Connection, days: i64) -> AppResult<usize> {
    // A retention window must be a positive number of days. A non-positive
    // value is meaningless and dangerous: `days = 0` builds the modifier
    // `'-0 days'`, so the cutoff `datetime('now', '-0 days')` collapses to
    // *now* and the DELETE purges **every** read article regardless of age;
    // a negative `days` builds a malformed `'--N days'` modifier that
    // `datetime()` evaluates to NULL, silently deleting nothing. Neither is a
    // real retention policy. The Settings UI only ever offers 30/90/180, but
    // this is the one chokepoint both that command and the background
    // scheduler funnel through — and the scheduler parses `days` from a
    // free-form settings string — so reject a non-positive value here rather
    // than trust every caller. Bail out as a no-op (0 removed).
    if days <= 0 {
        return Ok(0);
    }
    // Retention deletes only articles the user has not signalled they want to
    // keep. Starred and read-later are explicit "keep" flags; an article the
    // user has *highlighted* carries the same intent — the highlights table
    // cascade-deletes with the article (`ON DELETE CASCADE`), so purging a
    // highlighted-but-read article would silently destroy that hand-made
    // annotation layer (feature F7). Exempt any article with highlights.
    //
    // The purge condition — shared verbatim by the tombstone INSERT and the
    // DELETE so the two select exactly the same rows.
    const PURGE_WHERE: &str = "is_starred = 0 AND read_later = 0 AND is_read = 1
           AND NOT EXISTS (SELECT 1 FROM highlights WHERE article_id = articles.id)
           AND datetime(COALESCE(published_at, fetched_at)) < datetime('now', ?1)";
    let modifier = format!("-{days} days");

    // Tombstone every row about to be purged *before* deleting it, in one
    // transaction, so a full-archive feed can't re-insert it as fresh unread on
    // the next refresh (issue #98). `INSERT OR IGNORE` because a feed that
    // re-purges the same guid across cleanup runs already has its tombstone.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO article_tombstones(feed_id, guid)
             SELECT feed_id, guid FROM articles WHERE {PURGE_WHERE}"
        ),
        params![modifier],
    )?;
    let removed = tx.execute(
        &format!("DELETE FROM articles WHERE {PURGE_WHERE}"),
        params![modifier],
    )?;
    tx.commit()?;
    Ok(removed)
}

/// Reclaim free pages — must run outside any transaction.
pub fn vacuum(conn: &Connection) -> AppResult<()> {
    conn.execute_batch("VACUUM")?;
    Ok(())
}

/// Wipe all user content (feeds → articles cascade, folders). Settings are
/// kept. Both deletes commit together so a failure can't leave feeds wiped
/// but folders behind.
pub fn clear_all_data(conn: &Connection) -> AppResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM feeds", [])?;
    tx.execute("DELETE FROM folders", [])?;
    tx.commit()?;
    Ok(())
}

/// Clear every stored setting.
pub fn reset_settings(conn: &Connection) -> AppResult<()> {
    conn.execute("DELETE FROM settings", [])?;
    Ok(())
}

pub fn count_unread(conn: &Connection) -> AppResult<i64> {
    Ok(
        conn.query_row("SELECT COUNT(*) FROM articles WHERE is_read = 0", [], |r| {
            r.get(0)
        })?,
    )
}

/// Unread article count for a single feed — the same expression `list_feeds`
/// computes per row, used by `add_feed` so its returned `unread_count` matches
/// what the sidebar will show (rules that pre-mark an article read must not be
/// counted as unread).
pub fn count_feed_unread(conn: &Connection, feed_id: i64) -> AppResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM articles WHERE feed_id = ?1 AND is_read = 0",
        params![feed_id],
        |r| r.get(0),
    )?)
}

/// Timestamp of the most recent successful feed fetch, if any.
pub fn latest_fetch(conn: &Connection) -> AppResult<Option<String>> {
    Ok(
        conn.query_row("SELECT MAX(last_fetched_at) FROM feeds", [], |r| {
            r.get::<_, Option<String>>(0)
        })?,
    )
}

// ─────────────────────────── sync ───────────────────────────

/// Local article id for a given source URL — used to reconcile remote state.
pub fn article_id_by_url(conn: &Connection, url: &str) -> AppResult<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM articles WHERE url = ?1 LIMIT 1",
            params![url],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn set_remote_id(conn: &Connection, article_id: i64, remote_id: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE articles SET remote_id = ?2 WHERE id = ?1",
        params![article_id, remote_id],
    )?;
    Ok(())
}

/// Apply remote read/starred state to a local article.
pub fn set_sync_state(
    conn: &Connection,
    article_id: i64,
    read: bool,
    starred: bool,
) -> AppResult<()> {
    conn.execute(
        "UPDATE articles SET is_read = ?2, is_starred = ?3 WHERE id = ?1",
        params![article_id, read, starred],
    )?;
    Ok(())
}

/// Resolve a set of feed URLs to the ids of the local feeds that carry them,
/// skipping any URL the local DB doesn't track. Used to scope a sync
/// reconciliation to the feeds the server actually knows about.
pub fn feed_ids_by_urls(
    conn: &Connection,
    urls: &std::collections::HashSet<String>,
) -> AppResult<Vec<i64>> {
    let mut ids = Vec::with_capacity(urls.len());
    for url in urls {
        if let Some(id) = find_feed_by_url(conn, url)? {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Reconcile local read/starred state against the server's authoritative unread
/// + starred URL sets, for every article under one of `feed_ids`. An article is
/// marked read unless its URL is in `unread_urls`, and starred iff its URL is in
/// `starred_urls` — so the server's read state wins even for the long tail of
/// items a pull of only the recent unread set never enumerates (issue #96).
///
/// Articles carrying an un-pushed local edit (in `sync_queue`) are skipped, so a
/// pull never clobbers a change still waiting to be sent. Only rows whose state
/// actually changes are written. Returns the number of articles changed.
pub fn reconcile_sync_state(
    conn: &Connection,
    feed_ids: &[i64],
    unread_urls: &std::collections::HashSet<String>,
    starred_urls: &std::collections::HashSet<String>,
) -> AppResult<usize> {
    let pending: std::collections::HashSet<i64> =
        pending_sync_article_ids(conn)?.into_iter().collect();
    let tx = conn.unchecked_transaction()?;
    let mut changed: Vec<(i64, bool, bool)> = Vec::new();
    {
        let mut sel = tx.prepare(
            "SELECT id, url, is_read, is_starred FROM articles
             WHERE feed_id = ?1 AND url IS NOT NULL",
        )?;
        for &fid in feed_ids {
            let rows = sel.query_map(params![fid], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            })?;
            for row in rows {
                let (id, url, cur_read, cur_starred) = row?;
                if pending.contains(&id) {
                    continue;
                }
                let read = !unread_urls.contains(&url);
                let starred = starred_urls.contains(&url);
                if read != cur_read || starred != cur_starred {
                    changed.push((id, read, starred));
                }
            }
        }
    }
    for (id, read, starred) in &changed {
        tx.execute(
            "UPDATE articles SET is_read = ?2, is_starred = ?3 WHERE id = ?1",
            params![id, read, starred],
        )?;
    }
    tx.commit()?;
    Ok(changed.len())
}

/// Queue a local read/starred change to push on the next sync.
pub fn enqueue_sync(conn: &Connection, article_id: i64, field: &str, value: bool) -> AppResult<()> {
    conn.execute(
        "INSERT INTO sync_queue(article_id, field, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(article_id, field) DO UPDATE SET value = excluded.value",
        params![article_id, field, value],
    )?;
    Ok(())
}

/// Article ids that still carry un-pushed local changes — their state must not
/// be overwritten by a pull until the change has been sent.
pub fn pending_sync_article_ids(conn: &Connection) -> AppResult<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT DISTINCT article_id FROM sync_queue")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// One pushable change drained from the sync queue.
pub struct SyncEntry {
    pub article_id: i64,
    pub remote_id: String,
    pub field: String,
    pub value: bool,
}

/// Drain pushable queue entries. Only rows whose article already has a remote
/// id are returned and removed; the rest wait for a pull to assign one. The
/// caller MUST re-queue any entry whose push fails (see `requeue_sync`) so a
/// network blip never silently drops a local change.
pub fn take_sync_queue(conn: &Connection) -> AppResult<Vec<SyncEntry>> {
    let mut stmt = conn.prepare(
        "SELECT q.article_id, a.remote_id, q.field, q.value
         FROM sync_queue q JOIN articles a ON a.id = q.article_id
         WHERE a.remote_id IS NOT NULL",
    )?;
    let rows: Vec<SyncEntry> = stmt
        .query_map([], |r| {
            Ok(SyncEntry {
                article_id: r.get(0)?,
                remote_id: r.get(1)?,
                field: r.get(2)?,
                value: r.get::<_, i64>(3)? != 0,
            })
        })?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    conn.execute(
        "DELETE FROM sync_queue WHERE article_id IN
            (SELECT id FROM articles WHERE remote_id IS NOT NULL)",
        [],
    )?;
    Ok(rows)
}

/// Re-insert a queue entry whose push failed. Unlike `enqueue_sync` this does
/// not clobber a newer edit the user made on the same article during the sync.
pub fn requeue_sync(conn: &Connection, article_id: i64, field: &str, value: bool) -> AppResult<()> {
    conn.execute(
        "INSERT INTO sync_queue(article_id, field, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(article_id, field) DO NOTHING",
        params![article_id, field, value],
    )?;
    Ok(())
}

// ── Global AI chat sessions, briefings & read-only retrieval tools ───────────

pub fn list_ai_sessions(conn: &Connection, limit: i64) -> AppResult<Vec<AiChatSession>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, created_at, updated_at
         FROM ai_chat_sessions
         ORDER BY datetime(updated_at) DESC, rowid DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit.clamp(1, 200)], |r| {
            Ok(AiChatSession {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn create_ai_session(conn: &Connection, id: &str, title: &str) -> AppResult<AiChatSession> {
    conn.execute(
        "INSERT INTO ai_chat_sessions(id, title) VALUES (?1, ?2)",
        params![id, title],
    )?;
    let row = conn.query_row(
        "SELECT id, title, created_at, updated_at FROM ai_chat_sessions WHERE id = ?1",
        params![id],
        |r| {
            Ok(AiChatSession {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
            })
        },
    )?;
    Ok(row)
}

pub fn ensure_ai_session(conn: &Connection, id: &str, default_title: &str) -> AppResult<()> {
    conn.execute(
        "INSERT INTO ai_chat_sessions(id, title) VALUES (?1, ?2)
         ON CONFLICT(id) DO UPDATE SET
           title = CASE WHEN trim(title) = '' THEN excluded.title ELSE title END,
           updated_at = datetime('now')",
        params![id, default_title],
    )?;
    Ok(())
}

pub fn rename_ai_session(conn: &Connection, id: &str, title: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE ai_chat_sessions SET title = ?2, updated_at = datetime('now') WHERE id = ?1",
        params![id, title.trim()],
    )?;
    Ok(())
}

pub fn delete_ai_session(conn: &Connection, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM ai_chat_sessions WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn list_ai_messages(conn: &Connection, session_id: &str) -> AppResult<Vec<AiChatMessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, role, content, contexts_json, tools_json, created_at
         FROM ai_chat_messages
         WHERE session_id = ?1
         ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map(params![session_id], |r| {
            Ok(AiChatMessageRow {
                id: r.get(0)?,
                session_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                contexts_json: r.get(4)?,
                tools_json: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn append_ai_message(
    conn: &Connection,
    session_id: &str,
    role: &str,
    content: &str,
    contexts_json: Option<&str>,
    tools_json: Option<&str>,
) -> AppResult<i64> {
    conn.execute(
        "INSERT INTO ai_chat_messages(session_id, role, content, contexts_json, tools_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, role, content, contexts_json, tools_json],
    )?;
    let msg_id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE ai_chat_sessions SET updated_at = datetime('now') WHERE id = ?1",
        params![session_id],
    )?;
    Ok(msg_id)
}

/// Atomically ensure the session exists and append both the user turn and the
/// assistant reply inside a single SQLite transaction.
#[allow(clippy::too_many_arguments)]
pub fn save_ai_chat_turn(
    conn: &mut Connection,
    session_id: &str,
    default_title: &str,
    user_content: &str,
    contexts_json: Option<&str>,
    assistant_content: &str,
    tools_json: Option<&str>,
) -> AppResult<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO ai_chat_sessions(id, title) VALUES (?1, ?2)
         ON CONFLICT(id) DO UPDATE SET
           title = CASE WHEN trim(title) = '' THEN excluded.title ELSE title END,
           updated_at = datetime('now')",
        params![session_id, default_title],
    )?;
    tx.execute(
        "INSERT INTO ai_chat_messages(session_id, role, content, contexts_json, tools_json)
         VALUES (?1, 'user', ?2, ?3, NULL)",
        params![session_id, user_content, contexts_json],
    )?;
    tx.execute(
        "INSERT INTO ai_chat_messages(session_id, role, content, contexts_json, tools_json)
         VALUES (?1, 'assistant', ?2, NULL, ?3)",
        params![session_id, assistant_content, tools_json],
    )?;
    tx.execute(
        "UPDATE ai_chat_sessions SET updated_at = datetime('now') WHERE id = ?1",
        params![session_id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn list_ai_briefings(conn: &Connection, limit: i64) -> AppResult<Vec<AiBriefing>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, scope_label, days, article_count, content, created_at
         FROM ai_briefings
         ORDER BY datetime(created_at) DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit.clamp(1, 100)], |r| {
            Ok(AiBriefing {
                id: r.get(0)?,
                title: r.get(1)?,
                scope_label: r.get(2)?,
                days: r.get(3)?,
                article_count: r.get(4)?,
                content: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn save_ai_briefing(
    conn: &Connection,
    title: &str,
    scope_label: &str,
    days: i64,
    article_count: i64,
    content: &str,
) -> AppResult<AiBriefing> {
    conn.execute(
        "INSERT INTO ai_briefings(title, scope_label, days, article_count, content)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![title, scope_label, days, article_count, content],
    )?;
    let id = conn.last_insert_rowid();
    let row = conn.query_row(
        "SELECT id, title, scope_label, days, article_count, content, created_at
         FROM ai_briefings WHERE id = ?1",
        params![id],
        |r| {
            Ok(AiBriefing {
                id: r.get(0)?,
                title: r.get(1)?,
                scope_label: r.get(2)?,
                days: r.get(3)?,
                article_count: r.get(4)?,
                content: r.get(5)?,
                created_at: r.get(6)?,
            })
        },
    )?;
    Ok(row)
}

pub fn delete_ai_briefing(conn: &Connection, id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM ai_briefings WHERE id = ?1", params![id])?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn tool_search_articles(
    conn: &Connection,
    query: &str,
    feed_id: Option<i64>,
    folder_id: Option<i64>,
    tag_id: Option<i64>,
    unread_only: bool,
    starred_only: bool,
    read_later_only: bool,
    days: Option<i64>,
    limit: i64,
) -> AppResult<Vec<ToolArticleHit>> {
    let fts = fts_query(query, true);
    if query.trim().is_empty() || fts.trim_matches('"').trim().is_empty() {
        return tool_list_recent_articles(
            conn,
            feed_id,
            folder_id,
            tag_id,
            unread_only,
            starred_only,
            read_later_only,
            days,
            limit,
        );
    }
    let mut sql = String::from(
        "SELECT a.id, a.feed_id, f.title, a.title, a.author, a.url,
                COALESCE(a.published_at, a.fetched_at), a.is_read, a.is_starred,
                substr(a.body_text, 1, 360)
         FROM articles a
         JOIN feeds f ON f.id = a.feed_id
         JOIN articles_fts fts ON fts.rowid = a.id
         WHERE articles_fts MATCH ?",
    );
    let mut vals: Vec<Value> = vec![Value::Text(fts)];
    if let Some(fid) = feed_id {
        sql.push_str(" AND a.feed_id = ?");
        vals.push(Value::Integer(fid));
    }
    if let Some(foid) = folder_id {
        sql.push_str(" AND f.folder_id = ?");
        vals.push(Value::Integer(foid));
    }
    if let Some(tid) = tag_id {
        sql.push_str(" AND EXISTS (SELECT 1 FROM article_tags at WHERE at.article_id = a.id AND at.tag_id = ?)");
        vals.push(Value::Integer(tid));
    }
    if unread_only {
        sql.push_str(" AND a.is_read = 0");
    }
    if starred_only {
        sql.push_str(" AND a.is_starred = 1");
    }
    if read_later_only {
        sql.push_str(" AND a.read_later = 1");
    }
    if let Some(d) = days.filter(|&d| d > 0) {
        sql.push_str(" AND datetime(COALESCE(a.published_at, a.fetched_at)) >= datetime('now', ?)");
        vals.push(Value::Text(format!("-{d} days")));
    }
    sql.push_str(" ORDER BY fts.rank LIMIT ?");
    vals.push(Value::Integer(limit.clamp(1, 30)));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(vals), |r| {
            Ok(ToolArticleHit {
                id: r.get(0)?,
                feed_id: r.get(1)?,
                feed_title: r.get(2)?,
                title: r.get(3)?,
                author: r.get(4)?,
                url: r.get(5)?,
                published_at: r.get(6)?,
                is_read: r.get::<_, i64>(7)? != 0,
                is_starred: r.get::<_, i64>(8)? != 0,
                snippet: r.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
pub fn tool_list_recent_articles(
    conn: &Connection,
    feed_id: Option<i64>,
    folder_id: Option<i64>,
    tag_id: Option<i64>,
    unread_only: bool,
    starred_only: bool,
    read_later_only: bool,
    days: Option<i64>,
    limit: i64,
) -> AppResult<Vec<ToolArticleHit>> {
    let mut sql = String::from(
        "SELECT a.id, a.feed_id, f.title, a.title, a.author, a.url,
                COALESCE(a.published_at, a.fetched_at), a.is_read, a.is_starred,
                substr(a.body_text, 1, 360)
         FROM articles a
         JOIN feeds f ON f.id = a.feed_id
         WHERE 1 = 1",
    );
    let mut vals: Vec<Value> = Vec::new();
    if let Some(fid) = feed_id {
        sql.push_str(" AND a.feed_id = ?");
        vals.push(Value::Integer(fid));
    }
    if let Some(foid) = folder_id {
        sql.push_str(" AND f.folder_id = ?");
        vals.push(Value::Integer(foid));
    }
    if let Some(tid) = tag_id {
        sql.push_str(" AND EXISTS (SELECT 1 FROM article_tags at WHERE at.article_id = a.id AND at.tag_id = ?)");
        vals.push(Value::Integer(tid));
    }
    if unread_only {
        sql.push_str(" AND a.is_read = 0");
    }
    if starred_only {
        sql.push_str(" AND a.is_starred = 1");
    }
    if read_later_only {
        sql.push_str(" AND a.read_later = 1");
    }
    if let Some(d) = days.filter(|&d| d > 0) {
        sql.push_str(" AND datetime(COALESCE(a.published_at, a.fetched_at)) >= datetime('now', ?)");
        vals.push(Value::Text(format!("-{d} days")));
    }
    sql.push_str(" ORDER BY datetime(COALESCE(a.published_at, a.fetched_at)) DESC, a.id DESC LIMIT ?");
    vals.push(Value::Integer(limit.clamp(1, 40)));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(vals), |r| {
            Ok(ToolArticleHit {
                id: r.get(0)?,
                feed_id: r.get(1)?,
                feed_title: r.get(2)?,
                title: r.get(3)?,
                author: r.get(4)?,
                url: r.get(5)?,
                published_at: r.get(6)?,
                is_read: r.get::<_, i64>(7)? != 0,
                is_starred: r.get::<_, i64>(8)? != 0,
                snippet: r.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn tool_search_highlights(
    conn: &Connection,
    query: Option<&str>,
    limit: i64,
) -> AppResult<Vec<ToolHighlightHit>> {
    let q = query.map(str::trim).filter(|s| !s.is_empty());
    let (sql, vals): (&str, Vec<Value>) = if let Some(needle) = q {
        let lower_needle = needle.to_lowercase();
        (
            "SELECT h.id, h.article_id, a.title, f.title, h.quote, h.note, h.created_at
             FROM highlights h
             JOIN articles a ON a.id = h.article_id
             JOIN feeds f ON f.id = a.feed_id
             WHERE instr(unicode_lower(h.quote), ?1) > 0
                OR instr(unicode_lower(h.note), ?1) > 0
                OR instr(unicode_lower(a.title), ?1) > 0
             ORDER BY datetime(h.created_at) DESC, h.id DESC
             LIMIT ?2",
            vec![Value::Text(lower_needle), Value::Integer(limit.clamp(1, 30))],
        )
    } else {
        (
            "SELECT h.id, h.article_id, a.title, f.title, h.quote, h.note, h.created_at
             FROM highlights h
             JOIN articles a ON a.id = h.article_id
             JOIN feeds f ON f.id = a.feed_id
             ORDER BY datetime(h.created_at) DESC, h.id DESC
             LIMIT ?1",
            vec![Value::Integer(limit.clamp(1, 30))],
        )
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params_from_iter(vals), |r| {
            Ok(ToolHighlightHit {
                id: r.get(0)?,
                article_id: r.get(1)?,
                article_title: r.get(2)?,
                feed_title: r.get(3)?,
                quote: r.get(4)?,
                note: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// A fully-migrated in-memory connection for tests in other modules that
/// exercise settings-backed behaviour (parsers, sync, …). Test-only: the
/// migrations and function registration here mirror [`open`]'s setup.
#[cfg(test)]
pub(crate) fn test_conn() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    MIGRATIONS.to_latest(&mut conn).unwrap();
    register_functions(&conn).unwrap();
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_migrations_preserve_existing_databases() {
        for version in [0, 19, 20, 21, 22, 23, 24, 25, 26] {
            let path = std::env::temp_dir().join(format!(
                "papr-migrations-{}-{}-{}.db",
                std::process::id(),
                version,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut conn = Connection::open(&path).unwrap();
            MIGRATIONS.to_version(&mut conn, version.min(25)).unwrap();
            if version > 0 {
                conn.execute("INSERT INTO feeds(id,feed_url,title) VALUES(1,'https://example.com/rss','Preserved')", []).unwrap();
                conn.execute("INSERT INTO articles(feed_id,guid,title,is_starred) VALUES(1,'one','Article',1)", []).unwrap();
            }
            if version >= 20 {
                conn.execute("UPDATE feeds SET custom_favicon=1", [])
                    .unwrap();
            }
            if version >= 21 {
                conn.execute("UPDATE feeds SET position=42", []).unwrap();
            }
            if version == 26 {
                // A database from a *future* build (higher schema version than
                // this binary knows) must be refused, not silently migrated.
                conn.pragma_update(None, "user_version", 26).unwrap();
            }
            drop(conn);
            let result = open(&path);
            if version == 26 {
                assert!(result.is_err());
                let conn = Connection::open(&path).unwrap();
                assert_eq!(
                    conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                        .unwrap(),
                    26
                );
            } else {
                let conn = result.unwrap();
                assert_eq!(
                    conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                        .unwrap(),
                    25
                );
                if version > 0 {
                    let values: (String, i64, i64) = conn
                        .query_row(
                            "SELECT title,custom_favicon,position FROM feeds WHERE id=1",
                            [],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .unwrap();
                    assert_eq!(
                        values,
                        (
                            "Preserved".into(),
                            i64::from(version >= 20),
                            if version >= 21 { 42 } else { 0 }
                        )
                    );
                    assert_eq!(
                        conn.query_row(
                            "SELECT is_starred FROM articles WHERE guid='one'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )
                        .unwrap(),
                        1
                    );
                }
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn ai_profiles_preserve_legacy_values_and_explicit_clears() {
        let (conn, _) = test_db();
        set_setting(&conn, "ai_provider", "deepseek").unwrap();
        set_setting(&conn, "ai_model", "custom-model").unwrap();
        set_setting(&conn, "ai_base_url", "https://example.com/v1").unwrap();
        assert_eq!(
            configure_ai_provider(&conn, "openai", None, None).unwrap(),
            ("".into(), "".into())
        );
        configure_ai_provider(&conn, "openai", Some("gpt-custom"), Some("")).unwrap();
        assert_eq!(
            configure_ai_provider(&conn, "deepseek", None, None).unwrap(),
            ("custom-model".into(), "https://example.com/v1".into())
        );
        configure_ai_provider(&conn, "deepseek", Some(""), Some("")).unwrap();
        assert_eq!(
            configure_ai_provider(&conn, "openai", None, None)
                .unwrap()
                .0,
            "gpt-custom"
        );
        assert_eq!(
            configure_ai_provider(&conn, "deepseek", None, None).unwrap(),
            ("".into(), "".into())
        );
    }

    #[test]
    fn ai_profiles_roll_back_on_write_failure() {
        let (conn, _) = test_db();
        set_setting(&conn, "ai_model", "original").unwrap();
        conn.execute_batch("CREATE TRIGGER fail_provider BEFORE INSERT ON settings WHEN NEW.key = 'ai_provider' BEGIN SELECT RAISE(ABORT, 'write failure'); END;").unwrap();
        assert!(configure_ai_provider(&conn, "openai", Some("new"), Some("new-url")).is_err());
        assert_eq!(
            get_setting(&conn, "ai_model").unwrap().as_deref(),
            Some("original")
        );
        assert_eq!(get_setting(&conn, "ai_model_openai").unwrap(), None);
    }

    /// An in-memory database with all migrations applied and one feed +
    /// article inserted, so highlight FKs resolve. Returns `(conn, article_id)`.
    fn test_db() -> (Connection, i64) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        MIGRATIONS.to_latest(&mut conn).unwrap();
        // The production `open` / `open_reader` register custom SQL functions;
        // the in-memory test connection must too so `preview_rule`'s
        // `unicode_lower` resolves.
        register_functions(&conn).unwrap();
        let feed_id = insert_feed(
            &conn,
            "https://example.com/feed.xml",
            None,
            "Example Feed",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        let article = NewArticle {
            guid: "g1".into(),
            url: Some("https://example.com/a1".into()),
            title: "An Article".into(),
            author: None,
            summary: None,
            content_html: Some("<p>body</p>".into()),
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        upsert_article(&conn, feed_id, &article, false, &[]).unwrap();
        let article_id: i64 = conn
            .query_row("SELECT id FROM articles", [], |r| r.get(0))
            .unwrap();
        (conn, article_id)
    }

    #[test]
    fn due_logic_respects_per_feed_and_global_intervals() {
        let (conn, _) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        let global = 30;

        // Never fetched → always due, whatever the interval.
        assert!(
            feeds_due_for_refresh(&conn, global)
                .unwrap()
                .iter()
                .any(|(id, ..)| *id == feed_id)
        );

        // Just fetched → not due under the 30-minute global interval.
        set_feed_fetch_state(&conn, feed_id, None, None, None).unwrap();
        assert!(
            feeds_due_for_refresh(&conn, global).unwrap().is_empty(),
            "a just-fetched feed should not be due"
        );

        // Backdate the fetch 20 minutes: still under the 30-minute global…
        conn.execute(
            "UPDATE feeds SET last_fetched_at = datetime('now', '-20 minutes') WHERE id = ?1",
            params![feed_id],
        )
        .unwrap();
        assert!(feeds_due_for_refresh(&conn, global).unwrap().is_empty());

        // …but a 15-minute per-feed override makes it due.
        set_feed_refresh_interval(&conn, feed_id, Some(15)).unwrap();
        assert!(!feeds_due_for_refresh(&conn, global).unwrap().is_empty());

        // The "off" sentinel opts the feed out entirely, even when overdue.
        set_feed_refresh_interval(&conn, feed_id, Some(REFRESH_OFF_MINUTES)).unwrap();
        assert!(feeds_due_for_refresh(&conn, global).unwrap().is_empty());

        // Clearing the override returns the feed to the global interval: with
        // the 20-minute-old fetch and a 5-minute global, it is due again.
        set_feed_refresh_interval(&conn, feed_id, None).unwrap();
        assert!(!feeds_due_for_refresh(&conn, 5).unwrap().is_empty());
    }

    #[test]
    fn open_mode_round_trips_and_clears() {
        let (conn, _) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();

        // Fresh feed follows the default.
        assert_eq!(list_feeds(&conn).unwrap()[0].open_mode, None);

        set_feed_open_mode(&conn, feed_id, Some("web")).unwrap();
        assert_eq!(
            list_feeds(&conn).unwrap()[0].open_mode.as_deref(),
            Some("web")
        );

        // `None` reverts to the default.
        set_feed_open_mode(&conn, feed_id, None).unwrap();
        assert_eq!(list_feeds(&conn).unwrap()[0].open_mode, None);
    }

    #[test]
    fn preview_translation_cache_is_keyed_by_language_and_engine() {
        let (conn, id) = test_db();

        set_preview_translation(&conn, id, "标题", "摘要", "zh", "google").unwrap();
        set_preview_translation(&conn, id, "題名", "要約", "ja", "google").unwrap();
        set_preview_translation(&conn, id, "見出し", "抜粋", "ja", "deepl").unwrap();

        assert_eq!(
            get_preview_translation(&conn, id, "zh", "google").unwrap(),
            Some(("标题".into(), "摘要".into()))
        );
        assert_eq!(
            get_preview_translation(&conn, id, "ja", "google").unwrap(),
            Some(("題名".into(), "要約".into()))
        );
        assert_eq!(
            get_preview_translation(&conn, id, "ja", "deepl").unwrap(),
            Some(("見出し".into(), "抜粋".into()))
        );
        assert_eq!(
            get_preview_translation(&conn, id, "en", "google").unwrap(),
            None
        );
    }

    #[test]
    fn preview_translation_cache_is_cleared_when_source_preview_changes() {
        let (conn, id) = test_db();

        set_preview_translation(&conn, id, "标题", "摘要", "zh", "google").unwrap();
        conn.execute(
            "UPDATE articles SET title = ?2, body_text = ?3 WHERE id = ?1",
            params![id, "New title", "New body"],
        )
        .unwrap();

        assert_eq!(
            get_preview_translation(&conn, id, "zh", "google").unwrap(),
            None
        );
    }

    #[test]
    fn translation_round_trips_through_get_article() {
        let (conn, id) = test_db();
        // No translation cached on a fresh article.
        let before = get_article(&conn, id).unwrap();
        assert_eq!(before.translated_html, None);
        assert_eq!(before.translated_lang, None);
        assert_eq!(before.translated_title, None);

        set_translation(&conn, id, "<p>译文</p>", "zh", Some("译文标题")).unwrap();

        let after = get_article(&conn, id).unwrap();
        assert_eq!(after.translated_html.as_deref(), Some("<p>译文</p>"));
        assert_eq!(after.translated_lang.as_deref(), Some("zh"));
        assert_eq!(after.translated_title.as_deref(), Some("译文标题"));
    }

    #[test]
    fn set_translation_overwrites_a_previous_language() {
        let (conn, id) = test_db();
        set_translation(&conn, id, "<p>译文</p>", "zh", Some("标题")).unwrap();
        // A re-translation into another language carries its own title; a
        // `None` (no title produced) must clear the stale one, never leave a
        // title from the previous language behind.
        set_translation(&conn, id, "<p>translation</p>", "en", None).unwrap();
        let after = get_article(&conn, id).unwrap();
        assert_eq!(after.translated_html.as_deref(), Some("<p>translation</p>"));
        assert_eq!(after.translated_lang.as_deref(), Some("en"));
        assert_eq!(after.translated_title, None);
    }

    #[test]
    fn set_extracted_html_backfills_missing_image_url() {
        let (conn, id) = test_db();
        set_extracted_html(
            &conn,
            id,
            "<p>full text</p>",
            Some("https://ex.com/lead.jpg"),
        )
        .unwrap();

        let after = get_article(&conn, id).unwrap();
        assert_eq!(after.extracted_html.as_deref(), Some("<p>full text</p>"));
        assert_eq!(after.image_url.as_deref(), Some("https://ex.com/lead.jpg"));
    }

    #[test]
    fn set_extracted_html_keeps_existing_image_url() {
        let (conn, id) = test_db();
        conn.execute(
            "UPDATE articles SET image_url = ?2 WHERE id = ?1",
            params![id, "https://ex.com/feed.jpg"],
        )
        .unwrap();

        set_extracted_html(
            &conn,
            id,
            "<p>full text</p>",
            Some("https://ex.com/lead.jpg"),
        )
        .unwrap();

        let after = get_article(&conn, id).unwrap();
        assert_eq!(after.image_url.as_deref(), Some("https://ex.com/feed.jpg"));
    }

    #[test]
    fn card_image_backfill_scan_reads_extracted_html() {
        let (conn, id) = test_db();
        set_extracted_html(
            &conn,
            id,
            r#"<p>full text</p><img src="https://ex.com/from-extracted.jpg">"#,
            None,
        )
        .unwrap();

        let updates = card_image_backfill_scan(&conn).unwrap();
        assert_eq!(
            updates,
            vec![(id, "https://ex.com/from-extracted.jpg".into())]
        );
    }

    #[test]
    fn card_image_backfill_scan_includes_blank_image_url() {
        // A row whose `image_url` is blank (`''`, not NULL) must still be
        // scanned: `set_extracted_html`/`apply_card_images` treat NULL *or*
        // blank as missing, so a scan that only matched NULL would leave blank
        // rows permanently un-backfilled.
        let (conn, id) = test_db();
        conn.execute(
            "UPDATE articles
                SET image_url = '',
                    content_html = '<p>x</p><img src=\"https://ex.com/blank.jpg\">'
              WHERE id = ?1",
            [id],
        )
        .unwrap();

        let updates = card_image_backfill_scan(&conn).unwrap();
        assert_eq!(updates, vec![(id, "https://ex.com/blank.jpg".into())]);
    }

    /// Compact `NewHighlight` builder for the highlight tests.
    fn hl<'a>(
        article_id: i64,
        quote: &'a str,
        prefix: &'a str,
        suffix: &'a str,
        text_offset: i64,
        color: &'a str,
        note: &'a str,
    ) -> NewHighlight<'a> {
        NewHighlight {
            article_id,
            quote,
            prefix,
            suffix,
            text_offset,
            color,
            note,
        }
    }

    #[test]
    fn parser_article_refresh_updates_markup_clears_junk_and_keeps_user_state() {
        let conn = test_conn();
        let feed_id = insert_feed(
            &conn,
            "https://x.com/jack",
            Some("https://x.com/jack"),
            "jack",
            None,
            SourceType::X,
            None,
        )
        .unwrap();
        let mut article = NewArticle {
            guid: "800".into(),
            url: Some("https://x.com/jack/status/800".into()),
            title: "jack".into(),
            author: None,
            summary: Some("old summary".into()),
            content_html: Some("<div>old bare markup</div>".into()),
            body_text: "old body".into(),
            image_url: None,
            published_at: Some("2026-09-13T06:20:00+00:00".into()),
            enclosures: Vec::new(),
        };
        assert!(upsert_article(&conn, feed_id, &article, false, &[]).unwrap());
        let id: i64 = conn
            .query_row(
                "SELECT id FROM articles WHERE feed_id = ?1 AND guid = '800'",
                params![feed_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            article_feed_source_type(&conn, id).unwrap().as_deref(),
            Some("x")
        );

        // Unchanged markup is a no-op (a refresh re-renders the same tweet
        // thousands of times; it must not churn the WAL or re-index FTS).
        assert!(!refresh_parser_article(&conn, feed_id, &article).unwrap());

        // Simulate an article stored by an older version: a bare body, a web
        // extraction of x.com's SPA shell in `extracted_html`, and user state.
        conn.execute(
            "UPDATE articles SET is_read = 1, is_starred = 1, read_later = 1,
                    extracted_html = '<p>Log in or sign up for X</p>'
              WHERE id = ?1",
            params![id],
        )
        .unwrap();

        article.content_html = Some("<div class=\"x-tweet\">card</div>".into());
        article.body_text = "new body".into();
        article.summary = Some("new summary".into());
        assert!(refresh_parser_article(&conn, feed_id, &article).unwrap());

        let (content, extracted, body, is_read, starred, later, summary): (
            Option<String>,
            Option<String>,
            String,
            i64,
            i64,
            i64,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT content_html, extracted_html, body_text, is_read, is_starred,
                        read_later, summary
                   FROM articles WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        // Renderer-owned columns follow the new render.
        assert_eq!(
            content.as_deref(),
            Some("<div class=\"x-tweet\">card</div>")
        );
        assert_eq!(body, "new body");
        assert_eq!(summary.as_deref(), Some("new summary"));
        // The junk extraction is gone: it can only ever have been the site's
        // logged-out shell, and it must not win over the card.
        assert_eq!(extracted, None);
        // The user's own state survives a re-render.
        assert_eq!((is_read, starred, later), (1, 1, 1));

        // The search index follows the body it indexes.
        let indexed: String = conn
            .query_row(
                "SELECT body FROM articles_fts WHERE rowid = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexed, "new body");
    }

    #[test]
    fn refresh_parser_article_backfills_url_without_touching_the_body() {
        // The xiaohongshu pending-retry path can re-render a stored row to
        // byte-identical markup while carrying the permalink the anonymous
        // card was stored without — the URL backfill must land on its own,
        // not only when the body happens to change.
        let conn = test_conn();
        let feed_id = insert_feed(
            &conn,
            "https://www.xiaohongshu.com/user/profile/5e47b1ed0000000001006895",
            None,
            "给我开饭",
            None,
            SourceType::Xiaohongshu,
            None,
        )
        .unwrap();
        let article = NewArticle {
            guid: "xhs-5e47b1ed0000000001006895-1794462771000-bd93f046".into(),
            url: None,
            title: "这期偏韩系".into(),
            author: None,
            summary: Some("这期偏韩系".into()),
            content_html: Some("<div class=\"x-xhs-card\">card</div>".into()),
            body_text: "card".into(),
            image_url: None,
            published_at: Some("2026-11-12T05:52:51+00:00".into()),
            enclosures: Vec::new(),
        };
        assert!(upsert_article(&conn, feed_id, &article, false, &[]).unwrap());

        // Same body, same markup, but the upgraded article now knows its
        // permalink: the refresh must report a change and store the URL.
        let upgraded = NewArticle {
            url: Some(
                "https://www.xiaohongshu.com/explore/66f00a22000000001e01c0e4?xsec_token=ABz9token="
                    .into(),
            ),
            ..article.clone()
        };
        assert!(refresh_parser_article(&conn, feed_id, &upgraded).unwrap());
        let (url, content): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT url, content_html FROM articles WHERE feed_id = ?1 AND guid = ?2",
                params![feed_id, article.guid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            url.as_deref(),
            Some(
                "https://www.xiaohongshu.com/explore/66f00a22000000001e01c0e4?xsec_token=ABz9token="
            )
        );
        // The body was genuinely untouched.
        assert_eq!(
            content.as_deref(),
            Some("<div class=\"x-xhs-card\">card</div>")
        );

        // And the next refresh with the same article is a no-op again.
        assert!(!refresh_parser_article(&conn, feed_id, &upgraded).unwrap());
    }

    #[test]
    fn refresh_parser_article_never_downgrades_a_stored_article_body() {
        // A cycle whose X-article TweetDetail fetch failed re-renders the
        // bare title card. Rewriting would replace the stored full body with
        // that stub until a fetch succeeded again — the guard keeps the body.
        let conn = test_conn();
        let feed_id = insert_feed(
            &conn,
            "https://x.com/jack",
            None,
            "jack",
            None,
            SourceType::X,
            None,
        )
        .unwrap();
        let full = NewArticle {
            guid: "902".into(),
            url: Some("https://x.com/jack/status/902".into()),
            title: "jack".into(),
            author: None,
            summary: Some("标题\n\n正文".into()),
            content_html: Some(
                r#"<div class="x-tweet"><div class="x-tweet-body"><a class="x-article-title" href="https://x.com/jack/status/902">标题</a><div class="x-article-body"><p>正文</p></div></div></div>"#.into(),
            ),
            body_text: "标题\n\n正文".into(),
            image_url: Some("https://pbs.twimg.com/media/cover.jpg".into()),
            published_at: Some("2026-09-13T07:20:00+00:00".into()),
            enclosures: Vec::new(),
        };
        assert!(upsert_article(&conn, feed_id, &full, false, &[]).unwrap());

        // The degraded card (title link only) is refused.
        let degraded = NewArticle {
            content_html: Some(
                r#"<div class="x-tweet"><div class="x-tweet-body"><a href="https://x.com/jack/status/902">标题</a></div></div>"#.into(),
            ),
            ..full.clone()
        };
        assert!(!refresh_parser_article(&conn, feed_id, &degraded).unwrap());
        let stored: String = conn
            .query_row(
                "SELECT content_html FROM articles WHERE feed_id = ?1 AND guid = ?2",
                params![feed_id, full.guid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stored.contains("x-article-body"));

        // A genuinely new render that carries the body still upgrades (the
        // diff path applies, e.g. the article was edited on X).
        let edited = NewArticle {
            content_html: Some(
                r#"<div class="x-tweet"><div class="x-tweet-body"><a class="x-article-title" href="https://x.com/jack/status/902">标题</a><div class="x-article-body"><p>新正文</p></div></div></div>"#.into(),
            ),
            body_text: "标题\n\n新正文".into(),
            ..full.clone()
        };
        assert!(refresh_parser_article(&conn, feed_id, &edited).unwrap());
        let stored: String = conn
            .query_row(
                "SELECT content_html FROM articles WHERE feed_id = ?1 AND guid = ?2",
                params![feed_id, full.guid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stored.contains("新正文"));
    }

    #[test]
    fn refresh_parser_article_keeps_the_stored_body_when_incoming_is_empty() {
        // The kuaishou live-api ships no captions: its refresh results carry
        // an empty body_text, which must never blank the caption a
        // pre-migration row stored. An empty incoming body is "no
        // information", the same contract as the url/published_at COALESCE.
        let conn = test_conn();
        let feed_id = insert_feed(
            &conn,
            "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4",
            None,
            "创作者昵称",
            None,
            SourceType::Kuaishou,
            None,
        )
        .unwrap();
        let stored = NewArticle {
            guid: "3xphoto00001".into(),
            url: Some("https://www.kuaishou.com/short-video/3xphoto00001".into()),
            title: "第一条作品".into(),
            author: None,
            summary: Some("第一条作品".into()),
            content_html: Some("<p>旧文案</p>".into()),
            body_text: "旧文案".into(),
            image_url: None,
            published_at: Some("2026-11-12T05:52:51+00:00".into()),
            enclosures: Vec::new(),
        };
        assert!(upsert_article(&conn, feed_id, &stored, false, &[]).unwrap());

        // The new parse re-renders the row to a media card with no caption.
        let refreshed = NewArticle {
            title: "视频作品".into(),
            summary: Some("视频作品".into()),
            content_html: Some("<p><video controls preload=\"metadata\"></video></p>".into()),
            body_text: String::new(),
            ..stored.clone()
        };
        assert!(refresh_parser_article(&conn, feed_id, &refreshed).unwrap());
        let (body, fts_body): (String, String) = conn
            .query_row(
                "SELECT a.body_text, f.body FROM articles a
                 JOIN articles_fts f ON f.rowid = a.id
                 WHERE a.feed_id = ?1 AND a.guid = ?2",
                params![feed_id, stored.guid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(body, "旧文案");
        assert_eq!(fts_body, "旧文案");
        // Title/summary follow the new parse as intended.
        let title: String = conn
            .query_row(
                "SELECT title FROM articles WHERE feed_id = ?1 AND guid = ?2",
                params![feed_id, stored.guid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "视频作品");
    }

    #[test]
    fn refresh_parser_article_downgrade_guard_ignores_prose_mentions() {
        // The guard matches the class ATTRIBUTE: prose that merely mentions
        // the marker (an article titled "x-article-body") must not read as a
        // body container, or a title-card refresh could overwrite it.
        let conn = test_conn();
        let feed_id = insert_feed(
            &conn,
            "https://x.com/jack",
            None,
            "jack",
            None,
            SourceType::X,
            None,
        )
        .unwrap();
        let stored = NewArticle {
            guid: "910".into(),
            url: Some("https://x.com/jack/status/910".into()),
            title: "jack".into(),
            author: None,
            summary: Some("x-article-body".into()),
            content_html: Some(
                r#"<div class="x-tweet"><div class="x-tweet-body"><a href="https://x.com/jack/status/910">x-article-body</a></div></div>"#.into(),
            ),
            body_text: "x-article-body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(upsert_article(&conn, feed_id, &stored, false, &[]).unwrap());
        // Neither card carries a real `class="x-article-body"` container, so
        // the guard is inert: the changed markup rewrites as usual.
        let refresh = NewArticle {
            content_html: Some(
                r#"<div class="x-tweet"><div class="x-tweet-body"><a href="https://x.com/jack/status/910">新标题</a></div></div>"#.into(),
            ),
            ..stored.clone()
        };
        assert!(refresh_parser_article(&conn, feed_id, &refresh).unwrap());
        let content: String = conn
            .query_row(
                "SELECT content_html FROM articles WHERE feed_id = ?1 AND guid = ?2",
                params![feed_id, stored.guid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(content.contains("新标题"));
    }

    #[test]
    fn refresh_parser_article_clears_derived_caches_only_when_the_body_changed() {
        // A title-card row the user already summarized: the upgrade to the
        // full article body must drop the summary derived from the old card
        // — new text beside an old-card summary is exactly the staleness the
        // parser article refresh exists to avoid. A URL-only backfill (same
        // body) keeps the caches.
        let conn = test_conn();
        let feed_id = insert_feed(
            &conn,
            "https://x.com/jack",
            None,
            "jack",
            None,
            SourceType::X,
            None,
        )
        .unwrap();
        let stored = NewArticle {
            guid: "911".into(),
            url: None,
            title: "jack".into(),
            author: None,
            summary: Some("旧卡片".into()),
            content_html: Some(
                r#"<div class="x-tweet"><div class="x-tweet-body"><a href="https://x.com/jack/status/911">旧卡片</a></div></div>"#.into(),
            ),
            body_text: "旧卡片".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(upsert_article(&conn, feed_id, &stored, false, &[]).unwrap());
        conn.execute(
            "UPDATE articles SET ai_summary = '旧卡片摘要', translated_html = '<p>旧译文</p>',
                    translated_lang = 'zh', translated_title = '旧标题译文' WHERE feed_id = ?1",
            params![feed_id],
        )
        .unwrap();

        // URL-only backfill: the body is byte-identical, the caches stay.
        let backfill = NewArticle {
            url: Some("https://x.com/jack/status/911".into()),
            ..stored.clone()
        };
        assert!(refresh_parser_article(&conn, feed_id, &backfill).unwrap());
        let caches: (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT ai_summary, translated_html, translated_title FROM articles WHERE feed_id = ?1 AND guid = ?2",
                params![feed_id, stored.guid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(caches.0.as_deref(), Some("旧卡片摘要"));
        assert_eq!(caches.1.as_deref(), Some("<p>旧译文</p>"));
        assert_eq!(caches.2.as_deref(), Some("旧标题译文"));

        // The body itself changes (detail upgrade to the full article): the
        // derived caches are cleared.
        let upgraded = NewArticle {
            content_html: Some(
                r#"<div class="x-tweet"><div class="x-tweet-body"><a class="x-article-title" href="https://x.com/jack/status/911">标题</a><div class="x-article-body"><p>全文正文</p></div></div></div>"#.into(),
            ),
            body_text: "标题\n\n全文正文".into(),
            summary: Some("标题\n\n全文正文".into()),
            ..stored.clone()
        };
        assert!(refresh_parser_article(&conn, feed_id, &upgraded).unwrap());
        let caches: (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT ai_summary, translated_html, translated_lang FROM articles
                 WHERE feed_id = ?1 AND guid = ?2",
                params![feed_id, stored.guid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(caches.0, None);
        assert_eq!(caches.1, None);
        assert_eq!(caches.2, None);
    }

    #[test]
    fn refresh_parser_article_updates_the_title_even_when_only_it_changes() {
        // The kuaishou refresh body is empty, so the body comparison uses the
        // stored text — with identical markup and URL a title-only change
        // (e.g. the work-type label following a payload change) must still
        // trigger the update instead of early-returning and pinning the old
        // title forever.
        let conn = test_conn();
        let feed_id = insert_feed(
            &conn,
            "https://www.kuaishou.com/profile/3x4kuwfd24jzzf4",
            None,
            "创作者昵称",
            None,
            SourceType::Kuaishou,
            None,
        )
        .unwrap();
        let stored = NewArticle {
            guid: "3xphoto00042".into(),
            url: Some("https://www.kuaishou.com/short-video/3xphoto00042".into()),
            title: "作品".into(),
            author: None,
            summary: Some("作品".into()),
            content_html: Some("<p><video controls preload=\"metadata\"></video></p>".into()),
            body_text: "旧文案".into(),
            image_url: None,
            published_at: Some("2026-11-12T05:52:51+00:00".into()),
            enclosures: Vec::new(),
        };
        assert!(upsert_article(&conn, feed_id, &stored, false, &[]).unwrap());
        conn.execute(
            "UPDATE articles SET translated_title = '作品译文', translated_html = '<p>译文</p>' WHERE feed_id = ?1",
            params![feed_id],
        )
        .unwrap();

        // Same markup, same URL, empty body, but the title moved to the
        // work-type label: the refresh must report a change and store it.
        // A title change invalidates the title translation cache specifically,
        // while the unaffected body translation survives.
        let refreshed = NewArticle {
            title: "视频作品".into(),
            summary: Some("作品".into()),
            body_text: String::new(),
            ..stored.clone()
        };
        assert!(refresh_parser_article(&conn, feed_id, &refreshed).unwrap());
        let (title, summary, body, trans_title, trans_html): (String, Option<String>, String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT title, summary, body_text, translated_title, translated_html FROM articles WHERE feed_id = ?1 AND guid = ?2",
                params![feed_id, stored.guid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(title, "视频作品");
        assert_eq!(summary.as_deref(), Some("作品"));
        // The stored caption survives the body-preservation contract.
        assert_eq!(body, "旧文案");
        assert_eq!(trans_title, None);
        assert_eq!(trans_html.as_deref(), Some("<p>译文</p>"));

        // Idempotent: a second identical refresh reports no change.
        assert!(!refresh_parser_article(&conn, feed_id, &refreshed).unwrap());
    }

    #[test]
    fn refresh_parser_article_ignores_unknown_rows() {
        let conn = test_conn();
        let feed_id = insert_feed(
            &conn,
            "https://x.com/jack",
            None,
            "jack",
            None,
            SourceType::X,
            None,
        )
        .unwrap();
        let article = NewArticle {
            guid: "not-stored".into(),
            url: None,
            title: "jack".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: String::new(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(!refresh_parser_article(&conn, feed_id, &article).unwrap());
        assert!(article_feed_source_type(&conn, 424242).unwrap().is_none());
    }

    #[test]
    fn parser_feeds_are_excluded_from_http_refresh_and_selected_by_the_parser_queries() {
        let conn = test_conn();
        insert_feed(
            &conn,
            "https://example.com/feed.xml",
            None,
            "Rss Feed",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        insert_feed(
            &conn,
            "https://x.com/jack",
            None,
            "jack",
            None,
            SourceType::X,
            None,
        )
        .unwrap();

        // The plain-HTTP refresh set never sees the X feed: fetching
        // `x.com/jack` yields profile HTML, not a feed document.
        let http_urls: Vec<String> = feeds_to_refresh(&conn)
            .unwrap()
            .into_iter()
            .map(|(_, url, _, _)| url)
            .collect();
        assert_eq!(http_urls, vec!["https://example.com/feed.xml".to_string()]);

        // …and the due query agrees (a fresh feed is always due).
        let due = feeds_due_for_refresh(&conn, 30).unwrap();
        assert!(due.iter().all(|(_, url, _, _)| !url.contains("x.com")));

        // The parser leg selects exactly the X feed, in all four scopes.
        assert_eq!(
            parser_feeds_to_refresh(&conn).unwrap(),
            vec![(2, "x".to_string(), "https://x.com/jack".to_string())]
        );
        assert!(!parser_feeds_due_for_refresh(&conn, 30).unwrap().is_empty());
        assert_eq!(parser_feeds_for_feed(&conn, 2).unwrap().len(), 1);
        assert_eq!(parser_feeds_for_feed(&conn, 1).unwrap().len(), 0);
    }

    #[test]
    fn opml_export_and_sync_omit_parser_feeds_like_newsletters() {
        let conn = test_conn();
        insert_feed(
            &conn,
            "https://x.com/jack",
            None,
            "jack",
            None,
            SourceType::X,
            None,
        )
        .unwrap();
        // Neither downstream consumer may carry an `x.com/<user>` URL: any
        // other reader (and papr's own import) would treat it as an RSS feed
        // and fail on it forever.
        assert!(feeds_for_export(&conn).unwrap().is_empty());
        assert!(feed_urls_for_sync(&conn).unwrap().is_empty());
    }

    #[test]
    fn fresh_feed_has_no_last_fetched_until_touched() {
        let (conn, _) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        // A just-inserted feed has never been fetched.
        assert_eq!(feed_last_fetched(&conn, feed_id).unwrap(), None);
        // `touch_feed` (the same call `add_feed` makes after its initial
        // fetch) records the fetch time, so the feed no longer reads as
        // "never refreshed".
        touch_feed(&conn, feed_id).unwrap();
        assert!(feed_last_fetched(&conn, feed_id).unwrap().is_some());
    }

    #[test]
    fn refine_source_type_promotes_rss_but_never_demotes() {
        let (conn, _) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        let kind = |c: &Connection| -> String {
            c.query_row(
                "SELECT source_type FROM feeds WHERE id = ?1",
                params![feed_id],
                |r| r.get(0),
            )
            .unwrap()
        };
        // The test feed starts generic.
        assert_eq!(kind(&conn), "rss");

        // A no-op when the refined kind is still `Rss`.
        refine_feed_source_type(&conn, feed_id, SourceType::Rss).unwrap();
        assert_eq!(kind(&conn), "rss");

        // A genuine kind promotes the still-generic feed.
        refine_feed_source_type(&conn, feed_id, SourceType::Podcast).unwrap();
        assert_eq!(kind(&conn), "podcast");

        // Once classified, a later call must not churn the type — the
        // `WHERE source_type = 'rss'` guard makes this strictly a promotion.
        refine_feed_source_type(&conn, feed_id, SourceType::Mastodon).unwrap();
        assert_eq!(kind(&conn), "podcast");
    }

    #[test]
    fn opml_export_omits_newsletter_sources() {
        use crate::ingestion::newsletter::NewsletterConfig;
        let (conn, _) = test_db();
        // The RSS feed from `test_db` plus a newsletter source whose feed_url
        // is the synthetic, non-HTTP-fetchable `imap://` form.
        let cfg = NewsletterConfig {
            host: "imap.example.com".into(),
            port: 993,
            username: "me@example.com".into(),
            password: "secret".into(),
            folder: "Newsletters".into(),
        };
        insert_newsletter_source(
            &conn,
            "imap://me@example.com@imap.example.com:993/Newsletters",
            "My Newsletter",
            &cfg,
        )
        .unwrap();

        let exported = feeds_for_export(&conn).unwrap();
        // Only the real RSS feed is exportable — the newsletter is left out so
        // a re-import never resurrects it as a broken `imap://` RSS feed.
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].1, "https://example.com/feed.xml");
        assert!(
            !exported
                .iter()
                .any(|(_, url, _)| url.starts_with("imap://")),
            "no synthetic imap:// url should reach the OPML"
        );
    }

    #[test]
    fn insert_and_list_highlight() {
        let (conn, aid) = test_db();
        let id = insert_highlight(
            &conn,
            &hl(aid, "quoted text", "pre", "suf", 12, "yellow", ""),
        )
        .unwrap();
        let all = list_highlights(&conn, aid).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        assert_eq!(all[0].quote, "quoted text");
        assert_eq!(all[0].prefix, "pre");
        assert_eq!(all[0].suffix, "suf");
        assert_eq!(all[0].text_offset, 12);
        assert_eq!(all[0].color, "yellow");
        assert_eq!(all[0].note, "");
    }

    #[test]
    fn highlights_ordered_by_offset() {
        let (conn, aid) = test_db();
        insert_highlight(&conn, &hl(aid, "third", "", "", 90, "yellow", "")).unwrap();
        insert_highlight(&conn, &hl(aid, "first", "", "", 10, "yellow", "")).unwrap();
        insert_highlight(&conn, &hl(aid, "second", "", "", 50, "yellow", "")).unwrap();
        let quotes: Vec<String> = list_highlights(&conn, aid)
            .unwrap()
            .into_iter()
            .map(|h| h.quote)
            .collect();
        assert_eq!(quotes, ["first", "second", "third"]);
    }

    #[test]
    fn update_note_and_color() {
        let (conn, aid) = test_db();
        let id = insert_highlight(&conn, &hl(aid, "q", "", "", 0, "yellow", "")).unwrap();
        update_highlight_note(&conn, id, "a thought").unwrap();
        set_highlight_color(&conn, id, "green").unwrap();
        let h = get_highlight(&conn, id).unwrap().unwrap();
        assert_eq!(h.note, "a thought");
        assert_eq!(h.color, "green");
    }

    #[test]
    fn delete_highlight_removes_it() {
        let (conn, aid) = test_db();
        let id = insert_highlight(&conn, &hl(aid, "q", "", "", 0, "yellow", "")).unwrap();
        delete_highlight(&conn, id).unwrap();
        assert!(list_highlights(&conn, aid).unwrap().is_empty());
        assert!(get_highlight(&conn, id).unwrap().is_none());
    }

    #[test]
    fn highlights_cascade_on_article_delete() {
        let (conn, aid) = test_db();
        insert_highlight(&conn, &hl(aid, "q", "", "", 0, "yellow", "")).unwrap();
        conn.execute("DELETE FROM articles WHERE id = ?1", params![aid])
            .unwrap();
        assert!(list_highlights(&conn, aid).unwrap().is_empty());
    }

    #[test]
    fn list_all_highlights_spans_articles() {
        let (conn, aid) = test_db();
        insert_highlight(&conn, &hl(aid, "one", "", "", 0, "yellow", "")).unwrap();
        insert_highlight(&conn, &hl(aid, "two", "", "", 5, "green", "noted")).unwrap();
        assert_eq!(list_all_highlights(&conn).unwrap().len(), 2);
    }

    // ── FTS query building ───────────────────────────────────────────

    #[test]
    fn fts_query_and_joins_explicit_search_terms() {
        // Explicit search: every word required (implicit FTS5 AND).
        assert_eq!(fts_query("rust async", false), "\"rust\"* \"async\"*");
    }

    #[test]
    fn fts_query_or_joins_for_recall() {
        // RAG retrieval: any word may match.
        assert_eq!(
            fts_query("rust async runtime", true),
            "\"rust\"* OR \"async\"* OR \"runtime\"*"
        );
    }

    #[test]
    fn fts_query_strips_punctuation_and_handles_empty() {
        // Non-alphanumerics are dropped from each term; an all-punctuation
        // input collapses to a match-nothing expression in both modes.
        assert_eq!(fts_query("c++!", false), "\"c\"*");
        assert_eq!(fts_query("!!! ???", true), "\"\"");
        assert_eq!(fts_query("   ", false), "\"\"");
    }

    #[test]
    fn fts_query_splits_punctuation_into_separate_terms() {
        // Punctuation *inside* a word splits it into separate terms, matching
        // how the unicode61 tokenizer indexes the article text — collapsing
        // `rust-lang` to `rustlang` would match nothing the index holds.
        assert_eq!(fts_query("rust-lang", false), "\"rust\"* \"lang\"*");
        assert_eq!(fts_query("node.js", true), "\"node\"* OR \"js\"*");
        assert_eq!(
            fts_query("co-op runtime", false),
            "\"co\"* \"op\"* \"runtime\"*"
        );
    }

    /// Insert a second article with searchable text for the RAG tests.
    fn add_article(conn: &Connection, feed_id: i64, guid: &str, title: &str, body: &str) {
        let article = NewArticle {
            guid: guid.into(),
            url: Some(format!("https://example.com/{guid}")),
            title: title.into(),
            author: None,
            summary: None,
            content_html: Some(format!("<p>{body}</p>")),
            body_text: body.into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        upsert_article(conn, feed_id, &article, false, &[]).unwrap();
    }

    #[test]
    fn rag_search_matches_any_keyword_not_all() {
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        add_article(
            &conn,
            feed_id,
            "rust",
            "Rust news",
            "the borrow checker explained",
        );
        add_article(
            &conn,
            feed_id,
            "privacy",
            "Privacy law",
            "a new data privacy regulation",
        );

        // A natural-language question shares only *some* words with each
        // article. An AND join would require every word to appear and return
        // nothing; the OR-based RAG search still finds both relevant pieces.
        let hits = search_articles_for_rag(
            &conn,
            "what does the new privacy regulation say about the borrow checker",
            6,
        )
        .unwrap();
        let titles: Vec<&str> = hits.iter().map(|(_, t, _)| t.as_str()).collect();
        assert!(titles.contains(&"Rust news"), "got: {titles:?}");
        assert!(titles.contains(&"Privacy law"), "got: {titles:?}");
    }

    #[test]
    fn rag_search_empty_question_returns_no_rows() {
        let (conn, _aid) = test_db();
        // An all-stopword / punctuation-only question must not error and must
        // return nothing (the match-nothing `""` expression).
        assert!(
            search_articles_for_rag(&conn, "??? !!!", 6)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn create_tag_is_idempotent_on_name() {
        let (conn, _aid) = test_db();
        let first = create_tag(&conn, "Rust").unwrap();
        // Re-creating the same name returns the existing id, not a constraint
        // error, and does not add a second row.
        let again = create_tag(&conn, "Rust").unwrap();
        assert_eq!(first, again);
        // Case-insensitive: "rust" resolves to the same tag as "Rust".
        let cased = create_tag(&conn, "rust").unwrap();
        assert_eq!(first, cased);
        assert_eq!(list_tags(&conn).unwrap().len(), 1);
    }

    #[test]
    fn create_tag_trims_whitespace_and_dedups_padded_names() {
        // A tag name with surrounding whitespace must resolve to the same tag
        // as its trimmed form — otherwise the `COLLATE NOCASE` lookup misses
        // and a visually identical near-duplicate tag is created.
        let (conn, _aid) = test_db();
        let rust = create_tag(&conn, "Rust").unwrap();
        assert_eq!(create_tag(&conn, "  Rust  ").unwrap(), rust);
        assert_eq!(create_tag(&conn, "\tRust\n").unwrap(), rust);
        // The stored name is the trimmed form.
        let go = create_tag(&conn, "  Go ").unwrap();
        let name: String = conn
            .query_row("SELECT name FROM tags WHERE id = ?1", [go], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Go");
        assert_eq!(list_tags(&conn).unwrap().len(), 2);
    }

    #[test]
    fn rename_tag_rejects_whitespace_padded_collision() {
        // A rename to a whitespace-padded variant of another tag's name must
        // still be rejected — the trim lets the clash check see through it.
        let (conn, _aid) = test_db();
        let _rust = create_tag(&conn, "Rust").unwrap();
        let go = create_tag(&conn, "Go").unwrap();
        let err = rename_tag(&conn, go, "  Rust  ").unwrap_err();
        assert!(matches!(err, AppError::Coded("tagNameExists")));
    }

    #[test]
    fn create_tag_after_middle_delete_sorts_last() {
        // Deleting a tag from the middle of the list leaves a gap in the
        // `position` sequence. A new tag must still land at the end — a
        // `COUNT(*)`-based position would collide with an existing row.
        let (conn, _aid) = test_db();
        let a = create_tag(&conn, "alpha").unwrap();
        let b = create_tag(&conn, "beta").unwrap();
        let _c = create_tag(&conn, "gamma").unwrap();
        delete_tag(&conn, b).unwrap();
        let zoo = create_tag(&conn, "zeta").unwrap();

        let order: Vec<i64> = list_tags(&conn).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(
            order.last(),
            Some(&zoo),
            "a tag created after a middle delete must sort last, got {order:?}",
        );
        // The pre-existing tags keep their relative order.
        assert!(order.iter().position(|&x| x == a) < order.iter().position(|&x| x == zoo),);
    }

    // ── rule test helpers ─────────────────────────────────────────────

    /// One condition.
    fn cond(field: &str, operator: &str, value: &str) -> RuleCondition {
        RuleCondition {
            field: field.into(),
            operator: operator.into(),
            value: value.into(),
        }
    }

    /// A When-document with a single condition — the common shape.
    fn one(field: &str, operator: &str, value: &str) -> Vec<Vec<RuleCondition>> {
        vec![vec![cond(field, operator, value)]]
    }

    /// Legacy keyword semantics as a When-document: each comma-separated
    /// keyword becomes its own single-condition group (any keyword fires).
    fn kw(field: &str, query: &str) -> Vec<Vec<RuleCondition>> {
        query
            .split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(|term| vec![cond(field, "contains", term)])
            .collect()
    }

    #[test]
    fn create_rule_after_middle_delete_sorts_last() {
        // Same `COUNT(*)`-vs-`MAX(position)` hazard as tags: deleting a rule
        // from the middle leaves a `position` gap, so a `COUNT(*)`-based
        // position collides with an existing row and the new rule no longer
        // sorts last. Rule order is load-bearing for ingestion evaluation.
        let (conn, _aid) = test_db();
        // Five rules, positions {0,1,2,3,4}.
        let _a = create_rule(&conn, "alpha", None, &kw("title", "x"), "skip", &[]).unwrap();
        let b = create_rule(&conn, "beta", None, &kw("title", "y"), "skip", &[]).unwrap();
        let c = create_rule(&conn, "gamma", None, &kw("title", "z"), "skip", &[]).unwrap();
        let _d = create_rule(&conn, "delta", None, &kw("title", "w"), "skip", &[]).unwrap();
        let _e = create_rule(&conn, "epsilon", None, &kw("title", "u"), "skip", &[]).unwrap();
        // Delete two from the middle, leaving positions {0,3,4} — wide enough
        // that a `COUNT(*)` value (3) collides with a non-last rule.
        delete_rule(&conn, b).unwrap();
        delete_rule(&conn, c).unwrap();
        let zoo = create_rule(&conn, "zeta", None, &kw("title", "v"), "skip", &[]).unwrap();

        let order: Vec<i64> = list_rules(&conn).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(
            order.last(),
            Some(&zoo),
            "a rule created after a middle delete must sort last, got {order:?}",
        );
    }

    #[test]
    fn validate_rule_parts_accepts_open_time_actions() {
        // The open-time actions are valid Then-parts with no extras — only
        // `rewrite` demands rewrites — and unknown names are still rejected.
        for action in RULE_OPEN_ACTIONS {
            validate_rule_parts(&one("title", "contains", "x"), action, &[]).unwrap();
        }
        assert_eq!(
            validate_rule_parts(&one("title", "contains", "x"), "nope", &[]).unwrap_err(),
            "badRuleAction",
        );
    }

    /// A title-keyword rule with a fixed action — thin enough that the test
    /// bodies stay readable.
    fn rule(
        conn: &Connection,
        name: &str,
        feed_id: Option<i64>,
        needle: &str,
        action: &str,
    ) -> i64 {
        create_rule(conn, name, feed_id, &kw("title", needle), action, &[]).unwrap()
    }

    #[test]
    fn rule_actions_for_article_returns_matching_open_time_actions() {
        let (conn, aid) = test_db();
        // The fixture article's title contains "Article"; a `read` rule (the
        // ingestion-twin action) must never appear in the open-time set.
        rule(&conn, "tr", None, "Article", "translate");
        rule(&conn, "rd", None, "Article", "read");
        assert_eq!(
            rule_actions_for_article(&conn, aid).unwrap(),
            vec!["translate".to_string()],
        );

        // A rule whose When-document doesn't match contributes nothing.
        rule(&conn, "no", None, "absent", "summarize");
        assert_eq!(
            rule_actions_for_article(&conn, aid).unwrap(),
            vec!["translate".to_string()],
        );
    }

    #[test]
    fn rule_actions_for_article_unions_and_dedupes_matching_rules() {
        let (conn, aid) = test_db();
        rule(&conn, "ft", None, "Article", "fulltext");
        // Same action again (two rules asking for the same behaviour) and a
        // third distinct one — the union is deduped, in rule order.
        rule(&conn, "ft-2", None, "Article", "fulltext");
        rule(&conn, "oo", None, "Article", "open_original");
        assert_eq!(
            rule_actions_for_article(&conn, aid).unwrap(),
            vec!["fulltext".to_string(), "open_original".to_string()],
        );
    }

    #[test]
    fn rule_actions_for_article_respects_enabled_and_feed_scope() {
        let (conn, aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        // Disabled rules never fire.
        let tr = rule(&conn, "tr", None, "Article", "translate");
        update_rule(
            &conn,
            tr,
            "tr",
            false,
            None,
            &one("title", "contains", "Article"),
            "translate",
            &[],
        )
        .unwrap();
        assert!(rule_actions_for_article(&conn, aid).unwrap().is_empty());

        // A feed-scoped rule matches only its own feed's articles.
        rule(&conn, "sc", Some(feed_id), "Article", "summarize");
        assert_eq!(
            rule_actions_for_article(&conn, aid).unwrap(),
            vec!["summarize".to_string()],
        );

        // A rule scoped to a *different* feed must not match.
        let other = insert_feed(
            &conn,
            "https://example.com/other.xml",
            None,
            "Other Feed",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        rule(&conn, "ot", Some(other), "Article", "fulltext");
        assert_eq!(
            rule_actions_for_article(&conn, aid).unwrap(),
            vec!["summarize".to_string()],
        );
    }

    #[test]
    fn rule_actions_for_article_fires_for_legacy_keyword_rows() {
        // A pre-v24 keyword row synthesises its When-document at read time;
        // open-time resolution must see it exactly like the preview/backfill
        // paths do.
        let (conn, aid) = test_db();
        conn.execute(
            "INSERT INTO rules(name, feed_id, field, query, action, position)
             VALUES ('legacy', NULL, 'title', 'Article', 'translate', 0)",
            [],
        )
        .unwrap();
        assert_eq!(
            rule_actions_for_article(&conn, aid).unwrap(),
            vec!["translate".to_string()],
        );
    }

    // ── per-feed unread count ────────────────────────────────────────

    #[test]
    fn count_feed_unread_excludes_articles_pre_marked_read_by_a_rule() {
        // A filter rule with a `read` action inserts a matching article
        // already marked read. `count_feed_unread` must agree with
        // `list_feeds`: it counts only genuinely-unread rows.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        // The fixture article is unread.
        assert_eq!(count_feed_unread(&conn, feed_id).unwrap(), 1);

        // A rule that pre-marks anything titled "Sponsored" as read.
        create_rule(&conn, "ads", None, &kw("title", "Sponsored"), "read", &[]).unwrap();
        let rules = active_rules(&conn).unwrap();

        let read_by_rule = NewArticle {
            guid: "g-sponsored".into(),
            url: Some("https://example.com/sponsored".into()),
            title: "Sponsored Post".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "ad copy".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        let plain = NewArticle {
            guid: "g-plain".into(),
            url: Some("https://example.com/plain".into()),
            title: "A Normal Post".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "ordinary copy".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        // Both rows land, but `upsert_article` returns `true` only for the
        // genuinely-unread one — the rule-read article is not "new".
        assert!(
            !upsert_article(&conn, feed_id, &read_by_rule, false, &rules).unwrap(),
            "an article pre-marked read by a rule is not a new unread article"
        );
        assert!(upsert_article(&conn, feed_id, &plain, false, &rules).unwrap());

        // Three articles inserted total, but the rule-read one is not unread.
        assert_eq!(
            count_feed_unread(&conn, feed_id).unwrap(),
            2,
            "the rule-read article must not be counted as unread"
        );
        // And it matches the count `list_feeds` computes for the same feed.
        let from_list = list_feeds(&conn)
            .unwrap()
            .into_iter()
            .find(|f| f.id == feed_id)
            .unwrap()
            .unread_count;
        assert_eq!(from_list, 2);
    }

    #[test]
    fn upsert_article_reports_rule_read_inserts_as_not_new() {
        // The refresh scheduler tallies `upsert_article(..) == Ok(true)` into
        // the "N new articles" count that drives the refresh toast and the OS
        // notification. An article inserted but pre-marked read by a `read`
        // rule never appears as unread, so it must NOT be counted as new —
        // otherwise the toast/notification claims new articles the user can
        // never find.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        create_rule(&conn, "ads", None, &kw("title", "Sponsored"), "read", &[]).unwrap();
        let rules = active_rules(&conn).unwrap();

        let mk = |guid: &str, title: &str| NewArticle {
            guid: guid.into(),
            url: Some(format!("https://example.com/{guid}")),
            title: title.into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "copy".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };

        // Pre-marked read by the rule → not new.
        assert!(
            !upsert_article(&conn, feed_id, &mk("g-ad", "Sponsored Item"), false, &rules).unwrap()
        );
        // A plain article → genuinely new.
        assert!(upsert_article(&conn, feed_id, &mk("g-ok", "Real Story"), false, &rules).unwrap());
        // A duplicate guid → not new (no double count).
        assert!(!upsert_article(&conn, feed_id, &mk("g-ok", "Real Story"), false, &rules).unwrap());
    }

    // ── rule matching ────────────────────────────────────────────────

    #[test]
    fn any_field_rule_does_not_match_keyword_across_field_boundary() {
        // An `any`-field rule must check each field independently — the same
        // per-column semantics `preview_rule` uses. A keyword that only exists
        // because the title's tail and the body's head happen to abut must NOT
        // fire the rule; otherwise live ingestion acts on articles the rule
        // preview never counted.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();

        // A `skip` rule keyed on the two-word phrase "rust weekly".
        create_rule(&conn, "rw", None, &kw("any", "rust weekly"), "skip", &[]).unwrap();
        let rules = active_rules(&conn).unwrap();

        // Title ends in "rust", author starts with "weekly": the old code
        // concatenated `title author body` with single spaces, so the phrase
        // appeared only at that join. Per-field matching must not fire here,
        // and the article must still insert.
        let straddle = NewArticle {
            guid: "g-straddle".into(),
            url: Some("https://example.com/straddle".into()),
            title: "All about rust".into(),
            author: Some("Weekly Digest".into()),
            summary: None,
            content_html: None,
            body_text: "body text".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(
            upsert_article(&conn, feed_id, &straddle, false, &rules).unwrap(),
            "a keyword straddling the title/author boundary must not skip the article"
        );

        // The phrase wholly within one field still triggers the skip.
        let within = NewArticle {
            guid: "g-within".into(),
            url: Some("https://example.com/within".into()),
            title: "The Rust Weekly roundup".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "intro text".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(
            !upsert_article(&conn, feed_id, &within, false, &rules).unwrap(),
            "a keyword wholly within one field must still skip the article"
        );
    }

    #[test]
    fn preview_rule_case_folds_non_ascii_like_live_ingestion() {
        // `rule_matches` folds case with Rust's Unicode-aware `to_lowercase()`,
        // so a `café` rule matches a `CAFÉ` article during ingestion. SQLite's
        // built-in `LOWER()` is ASCII-only and would leave `É` uppercase,
        // making the preview undercount — `preview_rule` must use the
        // Unicode-aware `unicode_lower` so its count agrees with ingestion.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();

        // An article whose title carries an upper-case non-ASCII letter.
        let article = NewArticle {
            guid: "g-cafe".into(),
            url: Some("https://example.com/cafe".into()),
            title: "CAFÉ CULTURE in Zürich".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(upsert_article(&conn, feed_id, &article, false, &[]).unwrap());

        // Lower-case non-ASCII keywords must still count the article.
        for keyword in ["café", "zürich"] {
            let (count, samples) = preview_rule(&conn, None, &kw("title", keyword)).unwrap();
            assert_eq!(
                count, 1,
                "keyword `{keyword}` should match the CAFÉ article"
            );
            assert_eq!(samples.len(), 1);
        }

        // And the rule that the preview describes must agree at ingest time:
        // a `skip` rule on `café` drops a fresh `CAFÉ`-titled article.
        create_rule(&conn, "no-cafe", None, &kw("title", "café"), "skip", &[]).unwrap();
        let rules = active_rules(&conn).unwrap();
        let fresh = NewArticle {
            guid: "g-cafe-2".into(),
            url: Some("https://example.com/cafe2".into()),
            title: "Another CAFÉ Story".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(
            !upsert_article(&conn, feed_id, &fresh, false, &rules).unwrap(),
            "ingestion must skip the article the `café` preview counted"
        );
    }

    // ── apply_rule_to_existing (retroactive backfill on save) ─────────

    fn seed(conn: &Connection, feed_id: i64, guid: &str, title: &str) {
        let a = NewArticle {
            guid: guid.into(),
            url: Some(format!("https://example.com/{guid}")),
            title: title.into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        upsert_article(conn, feed_id, &a, false, &[]).unwrap();
    }

    #[test]
    fn apply_rule_to_existing_stars_matching_backlog_idempotently() {
        // The bug this guards: saving a `star` rule did nothing to articles
        // already stored — only `upsert_article` ran the rule, and that fires
        // only on freshly fetched articles. `apply_rule_to_existing` backfills
        // the matches when the rule is saved.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        seed(&conn, feed_id, "j1", "Learning Java today");
        seed(&conn, feed_id, "j2", "JavaScript tips");
        seed(&conn, feed_id, "p1", "Rust ownership");

        let n = apply_rule_to_existing(&conn, None, &kw("title", "java"), "star", &[]).unwrap();
        assert_eq!(n, 2, "both Java/JavaScript titles get starred");
        let starred: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM articles WHERE is_starred = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(starred, 2);

        // Re-running counts only rows it actually changes — already-starred rows
        // are excluded, so a second save reports 0.
        let again = apply_rule_to_existing(&conn, None, &kw("title", "java"), "star", &[]).unwrap();
        assert_eq!(again, 0);
    }

    #[test]
    fn apply_rule_to_existing_skip_deletes_matches_keeping_fts_in_sync() {
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        seed(&conn, feed_id, "a1", "Sponsored junk");
        seed(&conn, feed_id, "a2", "more Sponsored stuff");
        seed(&conn, feed_id, "a3", "real content");

        // The preview count is exactly the set the skip apply deletes — they
        // share `rule_match_where`, so the user is never surprised.
        let (preview, _) = preview_rule(&conn, None, &kw("title", "sponsored")).unwrap();
        assert_eq!(preview, 2);
        let n =
            apply_rule_to_existing(&conn, None, &kw("title", "sponsored"), "skip", &[]).unwrap();
        assert_eq!(n, 2);

        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM articles WHERE title LIKE '%Sponsored%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "matching articles are deleted");

        // The `articles_fts_ad` trigger drops the FTS rows with the articles, so
        // the index never goes stale.
        let arts: i64 = conn
            .query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))
            .unwrap();
        let fts: i64 = conn
            .query_row("SELECT COUNT(*) FROM articles_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            arts, fts,
            "FTS index stays in sync after a skip-rule delete"
        );
    }

    #[test]
    fn apply_rule_to_existing_scopes_to_one_feed() {
        let (conn, _aid) = test_db();
        let feed_a: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let feed_b = insert_feed(
            &conn,
            "https://example.com/b.xml",
            None,
            "Feed B",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        seed(&conn, feed_a, "x1", "Deals everywhere");
        seed(&conn, feed_b, "x2", "Deals galore");

        // Scoped to feed B only: feed A's match is left untouched.
        let n = apply_rule_to_existing(&conn, Some(feed_b), &kw("title", "deals"), "star", &[])
            .unwrap();
        assert_eq!(n, 1);
        let starred_in_a: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM articles WHERE is_starred = 1 AND feed_id = ?1",
                params![feed_a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            starred_in_a, 0,
            "a feed-scoped rule must not touch other feeds"
        );
    }

    // ── advanced rules (conditions document + rewrite) ───────────────

    #[test]
    fn legacy_keyword_row_still_matches_via_synthesised_conditions() {
        // A pre-v24 row has only the `field`/`query` keyword columns. It must
        // keep working: `row_to_rule` synthesises a one-condition-per-keyword
        // document with the original OR semantics.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO rules(name, feed_id, field, query, action, position)
             VALUES ('legacy', NULL, 'title', 'sponsored, junk', 'skip', 0)",
            [],
        )
        .unwrap();
        let rules = active_rules(&conn).unwrap();
        assert_eq!(rules.len(), 1);
        // Two keywords → two OR groups, both contains-on-title.
        assert_eq!(rules[0].conditions.len(), 2);
        assert_eq!(rules[0].conditions[0][0].value, "sponsored");
        assert_eq!(rules[0].conditions[1][0].operator, "contains");

        let mk = |guid: &str, title: &str| NewArticle {
            guid: guid.into(),
            url: Some(format!("https://example.com/{guid}")),
            title: title.into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(
            !upsert_article(&conn, feed_id, &mk("l1", "A SPONSORED post"), false, &rules).unwrap(),
            "legacy keyword rule must still skip at ingestion"
        );
        assert!(upsert_article(&conn, feed_id, &mk("l2", "Clean story"), false, &rules).unwrap());
    }

    #[test]
    fn rule_groups_are_and_within_and_or_across() {
        // (title contains rust AND author eq bob) OR (title contains spam)
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let conditions = vec![
            vec![
                cond("title", "contains", "rust"),
                cond("author", "eq", "bob"),
            ],
            vec![cond("title", "contains", "spam")],
        ];
        let mk = |guid: &str, title: &str, author: Option<&str>| NewArticle {
            guid: guid.into(),
            url: Some(format!("https://example.com/{guid}")),
            title: title.into(),
            author: author.map(Into::into),
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        // Seed the backlog *before* the skip rule exists, so the preview counts
        // rows that are actually stored (a skip rule never stores its hits).
        upsert_article(
            &conn,
            feed_id,
            &mk("c1", "rust news", Some("Bob")),
            false,
            &[],
        )
        .unwrap();
        upsert_article(&conn, feed_id, &mk("c2", "spam alert", None), false, &[]).unwrap();
        upsert_article(
            &conn,
            feed_id,
            &mk("c3", "rust news", Some("Alice")),
            false,
            &[],
        )
        .unwrap();
        // The SQL preview must count exactly the same two shapes ingestion
        // would act on.
        let (n, _) = preview_rule(&conn, None, &conditions).unwrap();
        assert_eq!(n, 2, "preview must agree with ingestion on group semantics");

        create_rule(&conn, "combo", None, &conditions, "skip", &[]).unwrap();
        let rules = active_rules(&conn).unwrap();
        // AND-group hit: rust + Bob → skipped.
        assert!(
            !upsert_article(
                &conn,
                feed_id,
                &mk("c4", "rust news", Some("Bob")),
                false,
                &rules
            )
            .unwrap()
        );
        // OR-group hit: spam alone → skipped.
        assert!(
            !upsert_article(&conn, feed_id, &mk("c5", "spam alert", None), false, &rules).unwrap()
        );
        // Only half of the AND group → no match, article inserts.
        assert!(
            upsert_article(
                &conn,
                feed_id,
                &mk("c6", "rust news", Some("Alice")),
                false,
                &rules
            )
            .unwrap(),
            "one matching condition of an AND group must not fire the rule"
        );
    }

    #[test]
    fn rule_operators_cover_negations_and_url() {
        // eq is exact (case-folded), not_contains excludes, and the url field
        // is addressable. Matcher and SQL builder must agree throughout.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        // Title must be exactly "digest" (folded) and contain no "sponsored".
        let conditions = vec![vec![
            cond("title", "eq", "Digest"),
            cond("title", "not_contains", "sponsored"),
            cond("url", "contains", "digest.example"),
        ]];
        let mk = |guid: &str, title: &str, url: &str| NewArticle {
            guid: guid.into(),
            url: Some(url.into()),
            title: title.into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        // Seed the backlog first so the preview sees stored rows.
        upsert_article(
            &conn,
            feed_id,
            &mk("o1", "DIGEST", "https://digest.example/feed"),
            false,
            &[],
        )
        .unwrap();
        upsert_article(
            &conn,
            feed_id,
            &mk("o2", "the digest", "https://digest.example/x"),
            false,
            &[],
        )
        .unwrap();
        upsert_article(
            &conn,
            feed_id,
            &mk("o3", "DIGEST", "https://other.example/feed"),
            false,
            &[],
        )
        .unwrap();
        // SQL side agrees with the matcher: exactly one row holds all three.
        let (n, _) = preview_rule(&conn, None, &conditions).unwrap();
        assert_eq!(n, 1);

        create_rule(&conn, "ops", None, &conditions, "skip", &[]).unwrap();
        let rules = active_rules(&conn).unwrap();
        // All three hold (case folded on title, needle inside the URL).
        assert!(
            !upsert_article(
                &conn,
                feed_id,
                &mk("o4", "DIGEST", "https://digest.example/feed"),
                false,
                &rules
            )
            .unwrap(),
            "eq + not_contains + url-contains must all fire together"
        );
        // Title too long for eq.
        assert!(
            upsert_article(
                &conn,
                feed_id,
                &mk("o5", "the digest", "https://digest.example/x"),
                false,
                &rules
            )
            .unwrap(),
            "eq is exact-match, not substring"
        );
        // URL doesn't carry the needle.
        assert!(
            upsert_article(
                &conn,
                feed_id,
                &mk("o6", "DIGEST", "https://other.example/feed"),
                false,
                &rules
            )
            .unwrap(),
            "the url condition must not match a foreign URL"
        );
    }

    #[test]
    fn regex_operator_uses_pattern_flags_and_invalid_never_matches() {
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        // (?i) makes the pattern case-insensitive; the matcher must NOT
        // lowercase the pattern itself (that would rewrite `\D` into `\d`).
        let ci = one("title", "regex", r"(?i)rust\d");
        create_rule(&conn, "re", None, &ci, "skip", &[]).unwrap();
        let rules = active_rules(&conn).unwrap();

        let mk = |guid: &str, title: &str| NewArticle {
            guid: guid.into(),
            url: Some(format!("https://example.com/{guid}")),
            title: title.into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(!upsert_article(&conn, feed_id, &mk("r1", "RUST2024"), false, &rules).unwrap());
        assert!(
            upsert_article(&conn, feed_id, &mk("r2", "rust d"), false, &rules).unwrap(),
            "digits are required by the pattern"
        );

        // An invalid pattern can only arrive in a hand-edited row: it must
        // never match (ingestion) and never error (SQL preview).
        conn.execute(
            "INSERT INTO rules(name, feed_id, field, query, action, position, conditions)
             VALUES ('badre', NULL, 'title', '', 'skip', 1, ?1)",
            [serde_json::to_string(&one("title", "regex", "([")).unwrap()],
        )
        .unwrap();
        let all = active_rules(&conn).unwrap();
        assert!(
            upsert_article(&conn, feed_id, &mk("r3", "anything ([ at all"), false, &all).unwrap(),
            "an invalid regex row must be inert, not fatal"
        );
        let (n, _) = preview_rule(&conn, None, &one("title", "regex", "([")).unwrap();
        assert_eq!(
            n, 0,
            "the SQL regexp() helper treats an invalid pattern as no-match"
        );
    }

    #[test]
    fn unknown_condition_field_or_operator_is_inert() {
        // Hand-edited rows may carry garbage; both the matcher and the SQL
        // builder must treat unknown values as "never matches", not fall back
        // to a default field.
        let (conn, _aid) = test_db();
        conn.execute(
            "INSERT INTO rules(name, feed_id, field, query, action, position, conditions)
             VALUES ('garbage', NULL, 'title', '', 'skip', 0, ?1)",
            [serde_json::to_string(&vec![
                vec![cond("bogus_field", "contains", "x")],
                vec![cond("title", "bogus_op", "x")],
            ])
            .unwrap()],
        )
        .unwrap();
        let rules = active_rules(&conn).unwrap();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let a = NewArticle {
            guid: "g-garbage".into(),
            url: Some("https://example.com/g".into()),
            title: "x marks the spot".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "x".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(
            upsert_article(&conn, feed_id, &a, false, &rules).unwrap(),
            "unknown field/operator must never match"
        );
        let (n, _) = preview_rule(&conn, None, &rules[0].conditions).unwrap();
        assert_eq!(n, 0, "SQL side must stay inert for the same row");
    }

    #[test]
    fn empty_condition_group_is_inert_on_both_sides() {
        // Regression: `all()` over an empty group is vacuously true, so a
        // hand-edited `[[valid], []]` row would have matched *every* article
        // at ingestion while the SQL side (which skips empty groups) previewed
        // a far smaller set. Both sides must treat the empty group as
        // non-participating; save-time validation rejects it outright.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let doc = vec![vec![cond("title", "contains", "zzz_nomatch")], Vec::new()];
        assert_eq!(
            crate::models::validate_conditions(&doc),
            Err("emptyRuleCondition"),
            "validation must reject an empty group"
        );
        conn.execute(
            "INSERT INTO rules(name, feed_id, field, query, action, position, conditions)
             VALUES ('empty-group', NULL, 'title', '', 'skip', 0, ?1)",
            [serde_json::to_string(&doc).unwrap()],
        )
        .unwrap();
        let rules = active_rules(&conn).unwrap();

        let mk = |guid: &str, title: &str| NewArticle {
            guid: guid.into(),
            url: Some(format!("https://example.com/{guid}")),
            title: title.into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(
            upsert_article(&conn, feed_id, &mk("e1", "a normal story"), false, &rules).unwrap(),
            "an empty group must not turn the rule into match-everything"
        );
        let (n, _) = preview_rule(&conn, None, &doc).unwrap();
        assert_eq!(n, 0, "SQL side agrees: no article carries the needle");
        // The real group still participates: an article carrying the needle
        // is skipped, exactly as the SQL preview implies.
        assert!(
            !upsert_article(&conn, feed_id, &mk("e2", "zzz_nomatch here"), false, &rules).unwrap(),
            "the non-empty group must keep firing normally"
        );
    }

    #[test]
    fn whitespace_only_needle_is_inert_on_both_sides() {
        // Regression: the matcher used a non-trimmed emptiness check, so a
        // `"  "` needle matched articles containing two spaces while the SQL
        // side (trim check) previewed nothing. Whitespace-only values are
        // never-match everywhere.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let doc = vec![vec![cond("title", "contains", "  ")]];
        conn.execute(
            "INSERT INTO rules(name, feed_id, field, query, action, position, conditions)
             VALUES ('spaces', NULL, 'title', '', 'skip', 0, ?1)",
            [serde_json::to_string(&doc).unwrap()],
        )
        .unwrap();
        let rules = active_rules(&conn).unwrap();
        let a = NewArticle {
            guid: "g-sp".into(),
            url: Some("https://example.com/sp".into()),
            title: "two  spaces inside".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(
            upsert_article(&conn, feed_id, &a, false, &rules).unwrap(),
            "a whitespace-only needle must never match at ingestion"
        );
        let (n, _) = preview_rule(&conn, None, &doc).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn corrupt_conditions_document_is_inert_not_resurrected_from_summary() {
        // A corrupt (hand-edited) conditions JSON must not fall back to the
        // `field`/`query` summary: those columns are derived from the
        // document, so rebuilding from them would silently change what the
        // rule matches. The rule goes inert until re-saved instead.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO rules(name, feed_id, field, query, action, position, conditions)
             VALUES ('corrupt', NULL, 'any', 'sponsored', 'skip', 0, '{\"not\": \"a document\"}')",
            [],
        )
        .unwrap();
        let rules = active_rules(&conn).unwrap();
        assert!(
            rules[0].conditions.is_empty(),
            "a corrupt document must load as inert, not as the keyword summary"
        );
        let a = NewArticle {
            guid: "g-corrupt".into(),
            url: Some("https://example.com/corrupt".into()),
            title: "SPONSORED post".into(),
            author: Some("ads".into()),
            summary: None,
            content_html: None,
            body_text: "sponsored".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        assert!(
            upsert_article(&conn, feed_id, &a, false, &rules).unwrap(),
            "the corrupt rule must not skip articles the summary would match"
        );
    }

    #[test]
    fn rewrite_backfill_preserves_extracted_fts_and_invalidates_derivatives() {
        // - A row with `extracted_html` has its FTS body indexed from the
        //   extracted full text (`set_extracted_html`), which rewrites never
        //   touch: the title rewrite must not clobber that index with the
        //   shorter feed body.
        // - Rows whose original text actually changed lose their cached
        //   translation and AI summary; matched-but-unchanged rows keep
        //   theirs. The returned count is rows *changed*, not rows matched.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        seed(&conn, feed_id, "x1", "Rust Weekly #1");
        seed(&conn, feed_id, "x2", "A plain story");
        // Rule matches x1 by title OR x2 by author; only x1's text carries
        // the needle, so only x1 changes.
        let doc = vec![
            vec![cond("title", "contains", "Rust Weekly")],
            vec![cond("author", "eq", "bob")],
        ];
        let rewrites = vec![RuleRewrite {
            from: "Rust Weekly".into(),
            to: "Rust Letter".into(),
        }];

        let x1: i64 = conn
            .query_row("SELECT id FROM articles WHERE guid = 'x1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let x2: i64 = conn
            .query_row("SELECT id FROM articles WHERE guid = 'x2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute("UPDATE articles SET author = 'bob' WHERE id = ?1", [x2])
            .unwrap();
        // x1: extracted full text (its FTS body is that text) + cached derivatives.
        set_extracted_html(&conn, x1, "<p>quasar deep text</p>", None).unwrap();
        set_translation(&conn, x1, "<p>old body</p>", "zh", Some("old title")).unwrap();
        set_ai_summary(&conn, x1, "old summary").unwrap();
        // x2: matched but unchanged — derivatives stay valid.
        set_translation(&conn, x2, "<p>old body</p>", "zh", Some("old title")).unwrap();
        set_ai_summary(&conn, x2, "old summary").unwrap();

        let changed = apply_rule_to_existing(&conn, None, &doc, "rewrite", &rewrites).unwrap();
        assert_eq!(changed, 1, "only x1's text actually contains the needle");

        // The extracted-derived FTS body survives; both titles are rebuilt.
        let quasar: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM articles_fts WHERE articles_fts MATCH 'quasar'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(quasar, 1, "the extracted body index must not be clobbered");
        let letter: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM articles_fts WHERE articles_fts MATCH 'letter'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(letter, 1, "the rewritten title is searchable");
        // x1's derivatives are dropped; x2's stay.
        let (t1, s1): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT translated_html, ai_summary FROM articles WHERE id = ?1",
                [x1],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (t1, s1),
            (None, None),
            "changed rows lose stale derivatives"
        );
        let (t2, s2): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT translated_html, ai_summary FROM articles WHERE id = ?1",
                [x2],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (t2.as_deref(), s2.as_deref()),
            (Some("<p>old body</p>"), Some("old summary")),
            "matched-but-unchanged rows keep their derivatives"
        );
        // The rewritten title landed too.
        let title: String = conn
            .query_row("SELECT title FROM articles WHERE id = ?1", [x1], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "Rust Letter #1");
    }

    #[test]
    fn nul_bearing_needles_are_inert_on_both_sides() {
        // SQLite's LIKE treats its pattern as NUL-terminated: a needle like
        // `x\0y` bound into `LIKE '%x\0y%'` can silently become `%` on the
        // SQL side — matching *everything* — while Rust's `contains` keeps
        // the NUL literal. `contains` / `not_contains` needles with embedded
        // NULs are therefore inert on both sides, the same policy as
        // whitespace-only values.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let contains_doc = vec![vec![cond("title", "contains", "a\0b")]];
        let not_contains_doc = vec![vec![cond("title", "not_contains", "a\0b")]];
        for (name, doc) in [("nul-c", &contains_doc), ("nul-nc", &not_contains_doc)] {
            conn.execute(
                "INSERT INTO rules(name, feed_id, field, query, action, position, conditions)
                 VALUES (?1, NULL, 'title', '', 'skip', 0, ?2)",
                params![name, serde_json::to_string(doc).unwrap()],
            )
            .unwrap();
        }
        let rules = active_rules(&conn).unwrap();

        let mk = |guid: &str, title: &str| NewArticle {
            guid: guid.into(),
            url: Some(format!("https://example.com/{guid}")),
            title: title.into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: "body".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        // A title that literally carries the needle still inserts — the
        // contains rule is inert, not widened.
        assert!(
            upsert_article(&conn, feed_id, &mk("z1", "a\0b headline"), false, &rules).unwrap(),
            "a NUL-bearing contains needle must never fire"
        );
        // An ordinary title would be matched by a (truncated) SQL pattern —
        // inertness keeps it safe; and the not_contains rule must not turn
        // into match-everything either.
        assert!(
            upsert_article(
                &conn,
                feed_id,
                &mk("z2", "ordinary headline"),
                false,
                &rules
            )
            .unwrap(),
            "a NUL-bearing not_contains needle must never fire"
        );
        // The SQL side agrees on both documents.
        let (n1, _) = preview_rule(&conn, None, &contains_doc).unwrap();
        let (n2, _) = preview_rule(&conn, None, &not_contains_doc).unwrap();
        assert_eq!(n1, 0, "SQL contains stays inert for NUL needles");
        assert_eq!(n2, 0, "SQL not_contains stays inert for NUL needles");
    }

    #[test]
    fn rewrite_backfill_leaves_matched_but_untouched_rows_alone() {
        // A matched row whose text the needle never touches must not be
        // updated at all: SQLite's UPDATE OF trigger (v17,
        // `article_preview_translations_au`) fires for every row an UPDATE
        // touches, changed or not, so a blanket update would drop
        // still-valid preview translations on untouched rows.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        seed(&conn, feed_id, "n1", "Weekly roundup");
        seed(&conn, feed_id, "n2", "Weekly recap");
        let rewrites = vec![RuleRewrite {
            from: "roundup".into(),
            to: "meeting".into(),
        }];
        let doc = vec![vec![cond("title", "contains", "Weekly")]];
        let ids: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare("SELECT id, guid FROM articles WHERE guid IN ('n1','n2')")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        for (id, guid) in &ids {
            set_translation(&conn, *id, "<p>old</p>", "zh", Some("old")).unwrap();
            set_ai_summary(&conn, *id, "old summary").unwrap();
            set_preview_translation(&conn, *id, "old title", "old snippet", "zh", "test").unwrap();
            let _ = guid;
        }
        let changed = apply_rule_to_existing(&conn, None, &doc, "rewrite", &rewrites).unwrap();
        assert_eq!(changed, 1, "only n1 carries the needle");

        for (id, guid) in &ids {
            let (title, th, ai, pt): (String, Option<String>, Option<String>, i64) = conn
                .query_row(
                    "SELECT title, translated_html, ai_summary,
                            (SELECT COUNT(*) FROM article_preview_translations
                              WHERE article_id = articles.id)
                     FROM articles WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            if guid == "n1" {
                assert_eq!(title, "Weekly meeting");
                assert_eq!(
                    (th, ai, pt),
                    (None, None, 0),
                    "changed row: derivatives dropped"
                );
            } else {
                assert_eq!(title, "Weekly recap");
                assert_eq!(
                    (th.as_deref(), ai.as_deref(), pt),
                    (Some("<p>old</p>"), Some("old summary"), 1),
                    "untouched row: all caches survive"
                );
            }
        }
    }

    #[test]
    fn rewrite_backfill_treats_from_equal_to_pairs_as_noops() {
        // A from==to pair can never change text (REPLACE with an identical
        // replacement is a no-op): a needle hit on one must not count the
        // row as changed or invalidate its derivative caches.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        seed(&conn, feed_id, "p1", "Weekly roundup");
        let id: i64 = conn
            .query_row("SELECT id FROM articles WHERE guid = 'p1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        set_translation(&conn, id, "<p>old</p>", "zh", Some("old")).unwrap();
        set_ai_summary(&conn, id, "old summary").unwrap();
        set_preview_translation(&conn, id, "old title", "old snippet", "zh", "test").unwrap();

        let doc = vec![vec![cond("title", "contains", "Weekly")]];
        let rewrites = vec![RuleRewrite {
            from: "Weekly".into(),
            to: "Weekly".into(),
        }];
        let changed = apply_rule_to_existing(&conn, None, &doc, "rewrite", &rewrites).unwrap();
        assert_eq!(changed, 0, "a no-op pair changes nothing");

        let (title, th, ai, pt): (String, Option<String>, Option<String>, i64) = conn
            .query_row(
                "SELECT title, translated_html, ai_summary,
                        (SELECT COUNT(*) FROM article_preview_translations
                          WHERE article_id = articles.id)
                 FROM articles WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(title, "Weekly roundup");
        assert_eq!(
            (th.as_deref(), ai.as_deref(), pt),
            (Some("<p>old</p>"), Some("old summary"), 1),
            "no caches may be invalidated by a no-op pair"
        );
    }

    #[test]
    fn rewrite_rule_rewrites_text_at_ingestion_matching_the_original() {
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let rewrites = vec![RuleRewrite {
            from: "Sponsored: ".into(),
            to: String::new(),
        }];
        create_rule(
            &conn,
            "fix",
            None,
            &one("title", "contains", "Sponsored"),
            "rewrite",
            &rewrites,
        )
        .unwrap();
        let rules = active_rules(&conn).unwrap();

        let a = NewArticle {
            guid: "g-rewrite".into(),
            url: Some("https://example.com/rw".into()),
            title: "Sponsored: A great deal".into(),
            author: Some("Ads Inc".into()),
            summary: Some("Sponsored: summary".into()),
            content_html: Some("<p>Sponsored: hello</p>".into()),
            body_text: "Sponsored: hello".into(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        // A rewrite insert still counts as new (it is unread — not pre-read).
        assert!(upsert_article(&conn, feed_id, &a, false, &rules).unwrap());
        let (title, summary, html, body): (String, Option<String>, Option<String>, String) = conn
            .query_row(
                "SELECT title, summary, content_html, body_text FROM articles WHERE guid = 'g-rewrite'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(title, "A great deal");
        assert_eq!(summary.as_deref(), Some("summary"));
        assert_eq!(html.as_deref(), Some("<p>hello</p>"));
        assert_eq!(body, "hello");
        // The FTS row was built from the rewritten text (insert path).
        let fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM articles_fts WHERE articles_fts MATCH 'deal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 1);
    }

    #[test]
    fn rewrite_rules_compose_in_rule_order() {
        // Two matching rewrite rules apply sequentially: the second sees the
        // first's output. Both match the ORIGINAL text (matching happens
        // before any rewrite).
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        create_rule(
            &conn,
            "one",
            None,
            &one("title", "contains", "alpha"),
            "rewrite",
            &[RuleRewrite {
                from: "alpha".into(),
                to: "beta".into(),
            }],
        )
        .unwrap();
        create_rule(
            &conn,
            "two",
            None,
            &one("title", "contains", "alpha"),
            "rewrite",
            &[RuleRewrite {
                from: "beta".into(),
                to: "gamma".into(),
            }],
        )
        .unwrap();
        let rules = active_rules(&conn).unwrap();
        let a = NewArticle {
            guid: "g-compose".into(),
            url: Some("https://example.com/comp".into()),
            title: "alpha".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: String::new(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        upsert_article(&conn, feed_id, &a, false, &rules).unwrap();
        let title: String = conn
            .query_row(
                "SELECT title FROM articles WHERE guid = 'g-compose'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "gamma", "rewrite pairs chain across matching rules");
    }

    #[test]
    fn apply_rule_to_existing_rewrite_updates_text_and_fts() {
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        seed(&conn, feed_id, "w1", "Rust Weekly #1");
        seed(&conn, feed_id, "w2", "Rust Weekly #2");
        seed(&conn, feed_id, "w3", "Go Letter");

        let rewrites = vec![RuleRewrite {
            from: "Rust Weekly".into(),
            to: "Rust Newsletter".into(),
        }];
        let (n, _) = preview_rule(&conn, None, &kw("title", "Rust Weekly")).unwrap();
        assert_eq!(n, 2);
        let changed = apply_rule_to_existing(
            &conn,
            None,
            &kw("title", "Rust Weekly"),
            "rewrite",
            &rewrites,
        )
        .unwrap();
        assert_eq!(changed, 2);

        let titles: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT title FROM articles WHERE guid LIKE 'w%' ORDER BY guid")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(
            titles,
            vec!["Rust Newsletter #1", "Rust Newsletter #2", "Go Letter"]
        );

        // No UPDATE trigger keeps FTS fresh, so the backfill rebuilt the
        // matched rows: the new text is searchable, the old text is gone.
        let fts_new: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM articles_fts WHERE articles_fts MATCH 'newsletter'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let fts_old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM articles_fts WHERE articles_fts MATCH 'weekly'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_new, 2, "rewritten titles are searchable");
        assert_eq!(fts_old, 0, "stale FTS entries must be rebuilt");
    }

    #[test]
    fn rewrite_backfill_matches_once_even_when_pairs_rewrite_the_needle() {
        // Pair 1 removes the very keyword the rule matched on; pair 2 rewrites
        // what pair 1 produced. The snapshot must be taken once — both pairs
        // act on the same row set — mirroring ingestion, which matches the
        // original article and then applies every rewrite.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        seed(&conn, feed_id, "s1", "advert quiet launch");

        let rewrites = vec![
            RuleRewrite {
                from: "advert".into(),
                to: String::new(),
            },
            RuleRewrite {
                from: "quiet".into(),
                to: "loud".into(),
            },
        ];
        let changed =
            apply_rule_to_existing(&conn, None, &kw("title", "advert"), "rewrite", &rewrites)
                .unwrap();
        assert_eq!(changed, 1);
        let title: String = conn
            .query_row("SELECT title FROM articles WHERE guid = 's1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        // The second pair still applied even though the first already removed
        // "advert" from the title (SQLite would otherwise re-evaluate the
        // WHERE per statement and drop the row from later pairs).
        assert_eq!(title.trim(), "loud launch");
    }

    #[test]
    fn create_rule_writes_flattened_keyword_summary() {
        // The legacy `field`/`query` columns stay as an informational summary:
        // one field when the document uses a single field, `any` when mixed.
        let (conn, _aid) = test_db();
        let conditions = vec![
            vec![
                cond("title", "contains", "rust"),
                cond("author", "eq", "bob"),
            ],
            vec![cond("title", "contains", "spam")],
        ];
        let id = create_rule(&conn, "mix", None, &conditions, "skip", &[]).unwrap();
        let (field, query): (String, String) = conn
            .query_row("SELECT field, query FROM rules WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(field, "any", "mixed fields summarise to `any`");
        assert_eq!(query, "rust,bob,spam");

        let id2 = create_rule(&conn, "single", None, &one("url", "eq", "x"), "read", &[]).unwrap();
        let (field, query): (String, String) = conn
            .query_row("SELECT field, query FROM rules WHERE id = ?1", [id2], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(field, "url");
        assert_eq!(query, "x");
    }

    // ── mark_all_read sync queueing ──────────────────────────────────

    #[test]
    fn mark_all_read_queues_articles_without_a_remote_id() {
        // Freshly fetched articles carry no `remote_id` until a sync pull
        // matches them by URL. A bulk "mark all read" run right after a
        // refresh must still queue those changes so they reach the sync
        // server once the id is assigned — not silently drop them.
        let (conn, aid) = test_db();
        assert_eq!(
            conn.query_row("SELECT remote_id FROM articles WHERE id = ?1", [aid], |r| r
                .get::<_, Option<String>>(0))
                .unwrap(),
            None,
            "fixture article should start without a remote id"
        );

        let n = mark_all_read(&conn, &ArticleQuery::All, true).unwrap();
        assert_eq!(n, 1, "the one unread article should be flipped to read");

        let queued: i64 = conn
            .query_row(
                "SELECT count(*) FROM sync_queue WHERE article_id = ?1 AND field = 'read'",
                [aid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(queued, 1, "the read change must be queued for sync");
    }

    #[test]
    fn mark_all_read_skips_sync_queue_when_not_connected() {
        // Without a sync server linked, no rows should land in the queue.
        let (conn, _aid) = test_db();
        mark_all_read(&conn, &ArticleQuery::All, false).unwrap();
        let queued: i64 = conn
            .query_row("SELECT count(*) FROM sync_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(queued, 0);
    }

    // ── sync state reconciliation (issue #96) ────────────────────────

    #[test]
    fn reconcile_marks_tail_read_and_scopes_to_server_feeds() {
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        // Fixture article a1 (guid g1) plus two more in the same server feed —
        // all start unread, as a raw RSS poll would leave them.
        seed(&conn, feed_id, "a2", "Two");
        seed(&conn, feed_id, "a3", "Three");
        // A local-only feed the server doesn't know about.
        let local = insert_feed(
            &conn,
            "https://local.example/feed.xml",
            None,
            "Local",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        seed(&conn, local, "b1", "Local one");

        // Server: only a2 is unread, a3 is starred.
        let unread: std::collections::HashSet<String> =
            ["https://example.com/a2".to_string()].into_iter().collect();
        let starred: std::collections::HashSet<String> =
            ["https://example.com/a3".to_string()].into_iter().collect();

        let changed = reconcile_sync_state(&conn, &[feed_id], &unread, &starred).unwrap();
        assert_eq!(
            changed, 2,
            "a1 -> read and a3 -> read+starred; a2 unchanged"
        );

        let read = |g: &str| -> bool {
            conn.query_row("SELECT is_read FROM articles WHERE guid = ?1", [g], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let is_starred = |g: &str| -> bool {
            conn.query_row(
                "SELECT is_starred FROM articles WHERE guid = ?1",
                [g],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(read("g1"), "tail item not in unread set is marked read");
        assert!(!read("a2"), "server-unread item stays unread");
        assert!(read("a3") && is_starred("a3"), "a3 is read and starred");
        assert!(!read("b1"), "local-only feed is out of scope and untouched");
    }

    #[test]
    fn reconcile_skips_pending_local_edits() {
        let (conn, aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT feed_id FROM articles LIMIT 1", [], |r| r.get(0))
            .unwrap();
        // User marked a1 unread locally; the change hasn't been pushed yet.
        enqueue_sync(&conn, aid, "read", false).unwrap();
        // Server considers a1 read (empty unread set) — but the pending local
        // edit must win, so nothing changes.
        let empty = std::collections::HashSet::new();
        let changed = reconcile_sync_state(&conn, &[feed_id], &empty, &empty).unwrap();
        assert_eq!(changed, 0, "pending article is skipped");
        let read: bool = conn
            .query_row("SELECT is_read FROM articles WHERE id = ?1", [aid], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!read, "a1 keeps its local (unread) state");
    }

    // ── retention cleanup ────────────────────────────────────────────

    /// Insert a read article with an explicit RFC 3339 `published_at`, the
    /// format `to_rfc3339` produces for every feed-dated article.
    fn insert_read_article_published(conn: &Connection, feed_id: i64, guid: &str, rfc3339: &str) {
        let a = NewArticle {
            guid: guid.into(),
            url: None,
            title: "T".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: String::new(),
            image_url: None,
            published_at: Some(rfc3339.into()),
            enclosures: Vec::new(),
        };
        upsert_article(conn, feed_id, &a, false, &[]).unwrap();
        conn.execute(
            "UPDATE articles SET is_read = 1 WHERE guid = ?1",
            params![guid],
        )
        .unwrap();
    }

    #[test]
    fn cleanup_compares_rfc3339_published_at_by_real_instant() {
        // `published_at` is stored RFC 3339 (`...T...+00:00`) while the
        // retention cutoff uses SQLite's space-separated form. A raw string
        // `<` mis-orders the two: the `T` byte sorts *after* a space, so for
        // an article whose `published_at` falls on the same calendar day as
        // the cutoff but earlier in the day, the string compare wrongly
        // reports it as newer and it escapes deletion. The fix normalises
        // both sides with `datetime()`.
        //
        // This test pins exactly that same-day boundary: an article dated to
        // the cutoff's own calendar day but earlier in that day — genuinely
        // outside a 30-day window, yet a string compare wrongly keeps it.
        let (conn, fixture) = test_db();
        let feed_id: i64 = conn
            .query_row(
                "SELECT feed_id FROM articles WHERE id = ?1",
                [fixture],
                |r| r.get(0),
            )
            .unwrap();

        // The cutoff is "now" minus 30 days, kept at the current wall-clock
        // time. Date this article to that very same calendar day but at one
        // second past midnight — genuinely older than the cutoff instant,
        // yet on a string `<` the RFC 3339 `T` makes it look newer.
        let now = chrono::Utc::now();
        let cutoff_day = (now - chrono::Duration::days(30)).date_naive();
        let old = cutoff_day.and_hms_opt(0, 0, 1).unwrap().and_utc();
        // Skip the rare case where the test runs within a second of midnight
        // and the article is not actually before the cutoff.
        assert!(old < now - chrono::Duration::days(30));
        insert_read_article_published(&conn, feed_id, "old", &old.to_rfc3339());
        // Comfortably inside the window — must be kept.
        let recent = chrono::Utc::now() - chrono::Duration::days(1);
        insert_read_article_published(&conn, feed_id, "recent", &recent.to_rfc3339());

        let removed = cleanup_old_articles(&conn, 30).unwrap();
        assert_eq!(removed, 1, "exactly the past-cutoff article should go");

        let surviving: Vec<String> = conn
            .prepare("SELECT guid FROM articles WHERE guid IN ('old','recent')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(surviving, ["recent"], "the old article must be deleted");
    }

    #[test]
    fn cleanup_keeps_starred_and_read_later_articles() {
        let (conn, _fixture) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339();
        insert_read_article_published(&conn, feed_id, "starred", &old);
        insert_read_article_published(&conn, feed_id, "later", &old);
        conn.execute(
            "UPDATE articles SET is_starred = 1 WHERE guid = 'starred'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE articles SET read_later = 1 WHERE guid = 'later'",
            [],
        )
        .unwrap();

        cleanup_old_articles(&conn, 30).unwrap();
        let kept: i64 = conn
            .query_row(
                "SELECT count(*) FROM articles WHERE guid IN ('starred','later')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 2, "starred / read-later articles are never purged");
    }

    #[test]
    fn cleanup_keeps_highlighted_articles() {
        // A read article the user has highlighted must survive retention: the
        // highlights cascade-delete with the article, so purging it would
        // silently destroy the user's annotations. An unhighlighted read
        // article of the same age is still purged.
        let (conn, _fixture) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339();
        insert_read_article_published(&conn, feed_id, "annotated", &old);
        insert_read_article_published(&conn, feed_id, "plain", &old);

        let annotated_id: i64 = conn
            .query_row(
                "SELECT id FROM articles WHERE guid = 'annotated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        insert_highlight(
            &conn,
            &hl(annotated_id, "kept quote", "", "", 0, "yellow", ""),
        )
        .unwrap();

        let removed = cleanup_old_articles(&conn, 30).unwrap();
        assert_eq!(removed, 1, "only the unhighlighted read article is purged");

        let surviving: Vec<String> = conn
            .prepare("SELECT guid FROM articles WHERE guid IN ('annotated','plain')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(surviving, ["annotated"], "the highlighted article is kept");
        // And its highlights are intact, not cascade-deleted.
        assert_eq!(list_highlights(&conn, annotated_id).unwrap().len(), 1);
    }

    #[test]
    fn cleanup_with_non_positive_days_is_a_no_op() {
        // A retention window of 0 days builds the modifier `'-0 days'`, whose
        // cutoff `datetime('now', '-0 days')` is *now* — left unguarded the
        // DELETE would purge every read article regardless of age. A negative
        // window builds a malformed `'--N days'` modifier. Both must be
        // rejected as no-ops so a bad caller / corrupt setting cannot trigger
        // a mass deletion.
        let (conn, _fixture) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        // A read article published just now — well inside any sane window,
        // yet `datetime('now', '-0 days')` would still sweep it.
        let now = chrono::Utc::now().to_rfc3339();
        insert_read_article_published(&conn, feed_id, "fresh", &now);

        assert_eq!(
            cleanup_old_articles(&conn, 0).unwrap(),
            0,
            "0 days deletes nothing"
        );
        assert_eq!(
            cleanup_old_articles(&conn, -30).unwrap(),
            0,
            "negative days deletes nothing"
        );

        let kept: i64 = conn
            .query_row(
                "SELECT count(*) FROM articles WHERE guid = 'fresh'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "the fresh article survives a non-positive window");
    }

    /// Count `articles` rows carrying a given `(feed_id, guid)` — 0 or 1 given
    /// the `UNIQUE(feed_id, guid)` constraint.
    fn guid_count(conn: &Connection, feed_id: i64, guid: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM articles WHERE feed_id = ?1 AND guid = ?2",
            params![feed_id, guid],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn retention_tombstone_blocks_reingestion_of_purged_article() {
        // The #98 loop: a full-archive feed (Hugo's `index.xml` ships its entire
        // history) keeps every past item in the document, so a read article that
        // retention purges is re-fetched on the very next refresh. Without a
        // tombstone it lands again as fresh *unread*, resurfacing the whole
        // archive the user just cleared — every day the daily cleanup runs. The
        // purge records a tombstone so the re-fetch is dropped.
        let (conn, fixture) = test_db();
        let feed_id: i64 = conn
            .query_row(
                "SELECT feed_id FROM articles WHERE id = ?1",
                [fixture],
                |r| r.get(0),
            )
            .unwrap();

        let old = (chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339();
        insert_read_article_published(&conn, feed_id, "archived", &old);

        assert_eq!(cleanup_old_articles(&conn, 30).unwrap(), 1);
        assert_eq!(
            guid_count(&conn, feed_id, "archived"),
            0,
            "purged by retention"
        );

        // The next refresh re-fetches the still-in-feed item. dedup is off (the
        // default), so only the tombstone can suppress the re-insertion.
        let refetched = NewArticle {
            guid: "archived".into(),
            url: Some("https://example.com/archived".into()),
            title: "Archived".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: String::new(),
            image_url: None,
            published_at: Some(old),
            enclosures: Vec::new(),
        };
        assert!(
            !upsert_article(&conn, feed_id, &refetched, false, &[]).unwrap(),
            "a retention-purged article must not be re-ingested as new"
        );
        assert_eq!(
            guid_count(&conn, feed_id, "archived"),
            0,
            "the purged article stays gone across refreshes"
        );
    }

    #[test]
    fn tombstone_is_scoped_to_its_feed() {
        // The tombstone key is (feed_id, guid). A different feed carrying the
        // same guid is unaffected — its article ingests normally.
        let (conn, fixture) = test_db();
        let feed_a: i64 = conn
            .query_row(
                "SELECT feed_id FROM articles WHERE id = ?1",
                [fixture],
                |r| r.get(0),
            )
            .unwrap();
        let feed_b = insert_feed(
            &conn,
            "https://other.example/feed.xml",
            None,
            "Other",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();

        let old = (chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339();
        insert_read_article_published(&conn, feed_a, "shared", &old);
        assert_eq!(cleanup_old_articles(&conn, 30).unwrap(), 1);

        let a = NewArticle {
            guid: "shared".into(),
            url: None,
            title: "T".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: String::new(),
            image_url: None,
            published_at: Some(old),
            enclosures: Vec::new(),
        };
        assert!(
            upsert_article(&conn, feed_b, &a, false, &[]).unwrap(),
            "the same guid under a different feed is not tombstoned"
        );
    }

    // ── article-list chronological ordering ──────────────────────────

    #[test]
    fn list_articles_orders_mixed_date_formats_by_real_instant() {
        // The newest-first list sorts on COALESCE(published_at, fetched_at).
        // A feed-dated row carries `published_at` as RFC 3339 (`...T...+00:00`,
        // a `T` separator); a dateless row falls through to `fetched_at`, which
        // SQLite stores space-separated (`... ...`). A raw string `<` compares
        // the two formats byte-for-byte and the `T` (0x54) sorts *after* a
        // space (0x20), so a dated row looks up to a day newer than it is —
        // a list mixing both kinds of rows comes out subtly out of order.
        //
        // This pins that exact mix: a dateless row fetched *later* than a
        // dated row was published. The dateless one must sort first. Under a
        // string compare the dated row wins (the `T`); `datetime()` fixes it.
        let (conn, fixture) = test_db();
        let feed_id: i64 = conn
            .query_row(
                "SELECT feed_id FROM articles WHERE id = ?1",
                [fixture],
                |r| r.get(0),
            )
            .unwrap();
        // Drop the bare fixture article so only the two controlled rows remain.
        conn.execute("DELETE FROM articles WHERE id = ?1", [fixture])
            .unwrap();

        // Same calendar day so the format difference — not the date — decides:
        // the dated row published at 10:00, the dateless row fetched at 12:00.
        // '2024-01-15T10:00:00+00:00' > '2024-01-15 12:00:00' as raw strings
        // (T beats space) but the dateless row is the genuinely newer one.
        insert_read_article_published(&conn, feed_id, "dated", "2024-01-15T10:00:00+00:00");
        let dateless = NewArticle {
            guid: "dateless".into(),
            url: None,
            title: "T".into(),
            author: None,
            summary: None,
            content_html: None,
            body_text: String::new(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        upsert_article(&conn, feed_id, &dateless, false, &[]).unwrap();
        conn.execute(
            "UPDATE articles SET fetched_at = '2024-01-15 12:00:00' WHERE guid = 'dateless'",
            [],
        )
        .unwrap();

        // Newest-first: the dateless row (fetched 12:00) precedes the dated
        // row (published 10:00).
        let newest = list_articles(&conn, &ArticleQuery::All, false, None, false, 50, 0).unwrap();
        let newest_guids: Vec<i64> = newest.iter().map(|a| a.id).collect();
        let dated_id: i64 = conn
            .query_row("SELECT id FROM articles WHERE guid = 'dated'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let dateless_id: i64 = conn
            .query_row("SELECT id FROM articles WHERE guid = 'dateless'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            newest_guids,
            [dateless_id, dated_id],
            "newest-first: the later-fetched dateless article must come first"
        );

        // Oldest-first is the exact mirror.
        let oldest = list_articles(&conn, &ArticleQuery::All, false, None, true, 50, 0).unwrap();
        let oldest_guids: Vec<i64> = oldest.iter().map(|a| a.id).collect();
        assert_eq!(
            oldest_guids,
            [dated_id, dateless_id],
            "oldest-first: the earlier-published dated article must come first"
        );
    }

    // ── feed rename vs. refresh ──────────────────────────────────────

    #[test]
    fn manual_rename_survives_a_metadata_refresh() {
        // A user renames a feed; a later refresh pulls the feed document's own
        // `<title>` through `update_feed_meta`. The rename must stick — only
        // the other metadata (site_url, description, favicon) should update.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();

        rename_feed(&conn, feed_id, "My Custom Name").unwrap();

        // Simulate a refresh: the feed document still calls itself "Example Feed".
        update_feed_meta(
            &conn,
            feed_id,
            Some("Example Feed"),
            Some("https://example.com"),
            Some("A description"),
            None,
        )
        .unwrap();

        let (title, site_url): (String, Option<String>) = conn
            .query_row(
                "SELECT title, site_url FROM feeds WHERE id = ?1",
                [feed_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            title, "My Custom Name",
            "a manual rename must not be reverted by a refresh"
        );
        assert_eq!(
            site_url.as_deref(),
            Some("https://example.com"),
            "non-title metadata must still refresh normally"
        );
    }

    #[test]
    fn rename_feed_rejects_an_empty_title() {
        // An empty (or whitespace-only) rename must be refused: a rename sets
        // `custom_title = 1`, so an empty title would blank the feed in the
        // sidebar forever — `update_feed_meta` can no longer restore it. The
        // original title must be left untouched.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        let original: String = conn
            .query_row("SELECT title FROM feeds WHERE id = ?1", [feed_id], |r| {
                r.get(0)
            })
            .unwrap();

        for blank in ["", "   ", "\t\n"] {
            let err = rename_feed(&conn, feed_id, blank).unwrap_err();
            assert!(
                err.to_string().contains("emptyFeedTitle"),
                "blank rename {blank:?} should be rejected, got: {err}"
            );
        }

        let (title, custom): (String, bool) = conn
            .query_row(
                "SELECT title, custom_title FROM feeds WHERE id = ?1",
                [feed_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            title, original,
            "a rejected rename must not alter the title"
        );
        assert!(
            !custom,
            "a rejected rename must not set the custom_title flag"
        );
    }

    #[test]
    fn rename_feed_trims_surrounding_whitespace() {
        // A padded name (`"  News  "`) is stored trimmed — the db function is
        // the single trimming chokepoint, so the command no longer trims.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();
        rename_feed(&conn, feed_id, "  Tech News  ").unwrap();
        let title: String = conn
            .query_row("SELECT title FROM feeds WHERE id = ?1", [feed_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "Tech News");
    }

    #[test]
    fn refresh_updates_title_when_not_renamed() {
        // A feed the user has never renamed should still pick up the feed
        // document's title on refresh — the guard only protects manual names.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();

        update_feed_meta(&conn, feed_id, Some("Renamed Upstream"), None, None, None).unwrap();

        let title: String = conn
            .query_row("SELECT title FROM feeds WHERE id = ?1", [feed_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "Renamed Upstream");
    }

    #[test]
    fn refresh_with_empty_title_keeps_existing_name() {
        // `feed-rs` parses `<title></title>` (or a stray empty title) as
        // `Some("")`, and the scheduler's refresh path forwards it straight
        // into `update_feed_meta`. An empty string must be treated like
        // `None` — the feed's good sidebar name must survive, not be wiped
        // blank. The other metadata columns get the same empty-string guard.
        let (conn, _aid) = test_db();
        let feed_id: i64 = conn
            .query_row("SELECT id FROM feeds", [], |r| r.get(0))
            .unwrap();

        // Seed real metadata first (the feed has never been renamed by hand).
        update_feed_meta(
            &conn,
            feed_id,
            Some("Good Feed Name"),
            Some("https://example.com"),
            Some("A description"),
            Some("https://example.com/favicon.ico"),
        )
        .unwrap();

        // A later refresh serves empty metadata — every field must be ignored.
        update_feed_meta(&conn, feed_id, Some(""), Some(""), Some(""), Some("")).unwrap();

        let (title, site_url, description, favicon): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT title, site_url, description, favicon_url FROM feeds WHERE id = ?1",
                [feed_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            title, "Good Feed Name",
            "an empty <title> on refresh must not blank the sidebar name"
        );
        assert_eq!(site_url.as_deref(), Some("https://example.com"));
        assert_eq!(description.as_deref(), Some("A description"));
        assert_eq!(favicon.as_deref(), Some("https://example.com/favicon.ico"));
    }

    #[test]
    fn rename_tag_rejects_collision_with_another_tag() {
        let (conn, _aid) = test_db();
        let rust = create_tag(&conn, "Rust").unwrap();
        let go = create_tag(&conn, "Go").unwrap();

        // Renaming "Go" onto an exact match of "Rust" must be rejected with the
        // localisable code rather than the raw UNIQUE-constraint SQLite error.
        let err = rename_tag(&conn, go, "Rust").unwrap_err();
        assert!(
            matches!(err, AppError::Coded("tagNameExists")),
            "expected tagNameExists, got {err:?}"
        );
        // A *case variant* of another tag must be rejected too — otherwise it
        // would create the near-duplicate `create_tag` deliberately collapses.
        let err = rename_tag(&conn, go, "rust").unwrap_err();
        assert!(matches!(err, AppError::Coded("tagNameExists")));

        // The clash check did not corrupt either name.
        let name = |id| {
            conn.query_row("SELECT name FROM tags WHERE id = ?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
        };
        assert_eq!(name(rust), "Rust");
        assert_eq!(name(go), "Go");
    }

    #[test]
    fn rename_tag_allows_genuine_rename_and_self_case_change() {
        let (conn, _aid) = test_db();
        let id = create_tag(&conn, "draft").unwrap();
        // A free name is accepted.
        rename_tag(&conn, id, "Reading").unwrap();
        // Re-casing the tag's *own* name is allowed (no other tag clashes).
        rename_tag(&conn, id, "READING").unwrap();
        let name: String = conn
            .query_row("SELECT name FROM tags WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "READING");
    }

    // --- folders: same name-uniqueness family as tags. `folders.name` has no
    //     UNIQUE constraint, so create/rename must dedup in code. ---

    #[test]
    fn create_folder_is_idempotent_on_name() {
        let (conn, _aid) = test_db();
        let first = create_folder(&conn, "Tech").unwrap();
        // Re-creating the same name returns the existing id, not a second row.
        assert_eq!(create_folder(&conn, "Tech").unwrap(), first);
        // Case-insensitive: "tech" resolves to the same folder as "Tech".
        assert_eq!(create_folder(&conn, "tech").unwrap(), first);
        assert_eq!(list_folders(&conn).unwrap().len(), 1);
    }

    #[test]
    fn folder_id_by_name_reuses_existing_folder_case_insensitively() {
        // OPML import attaches feeds via `folder_id_by_name`; an imported
        // folder whose name matches an existing one (in any case) must reuse
        // that folder rather than spawn a near-duplicate.
        let (conn, _aid) = test_db();
        let existing = create_folder(&conn, "News").unwrap();
        assert_eq!(folder_id_by_name(&conn, "news").unwrap(), existing);
        assert_eq!(list_folders(&conn).unwrap().len(), 1);
        // A genuinely new name still creates a folder.
        let fresh = folder_id_by_name(&conn, "Science").unwrap();
        assert_ne!(fresh, existing);
        assert_eq!(list_folders(&conn).unwrap().len(), 2);
    }

    #[test]
    fn rename_folder_rejects_collision_with_another_folder() {
        let (conn, _aid) = test_db();
        let tech = create_folder(&conn, "Tech").unwrap();
        let news = create_folder(&conn, "News").unwrap();

        // Renaming "News" onto an exact match of "Tech" is rejected with the
        // localisable code.
        let err = rename_folder(&conn, news, "Tech").unwrap_err();
        assert!(
            matches!(err, AppError::Coded("folderNameExists")),
            "expected folderNameExists, got {err:?}"
        );
        // A case variant of another folder is rejected too.
        let err = rename_folder(&conn, news, "tech").unwrap_err();
        assert!(matches!(err, AppError::Coded("folderNameExists")));

        // Neither name was corrupted by the clash check.
        let name = |id| {
            conn.query_row("SELECT name FROM folders WHERE id = ?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
        };
        assert_eq!(name(tech), "Tech");
        assert_eq!(name(news), "News");
    }

    #[test]
    fn rename_folder_allows_genuine_rename_and_self_case_change() {
        let (conn, _aid) = test_db();
        let id = create_folder(&conn, "Misc").unwrap();
        // A free name is accepted.
        rename_folder(&conn, id, "Reading").unwrap();
        // Re-casing the folder's *own* name is allowed (no other folder clashes).
        rename_folder(&conn, id, "READING").unwrap();
        let name: String = conn
            .query_row("SELECT name FROM folders WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "READING");
    }

    #[test]
    fn create_folder_trims_whitespace_and_dedups_padded_names() {
        // An OPML-imported folder name carrying surrounding whitespace
        // (`<outline text=" Tech ">`) must resolve to the same folder as a
        // plain "Tech" — without the trim the `COLLATE NOCASE` lookup misses
        // it and a second, visually identical folder is spawned.
        let (conn, _aid) = test_db();
        let tech = create_folder(&conn, "Tech").unwrap();
        assert_eq!(create_folder(&conn, "  Tech  ").unwrap(), tech);
        assert_eq!(create_folder(&conn, "\tTech\n").unwrap(), tech);
        // The first creation also stores the trimmed form, not the padded one.
        let padded = create_folder(&conn, " News ").unwrap();
        let name: String = conn
            .query_row("SELECT name FROM folders WHERE id = ?1", [padded], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "News");
        assert_eq!(list_folders(&conn).unwrap().len(), 2);
    }

    #[test]
    fn rename_folder_rejects_whitespace_padded_collision() {
        // A rename to a whitespace-padded variant of another folder's name
        // must still be rejected — the trim makes the clash check see through
        // the padding instead of letting the near-duplicate slip past.
        let (conn, _aid) = test_db();
        let _tech = create_folder(&conn, "Tech").unwrap();
        let news = create_folder(&conn, "News").unwrap();
        let err = rename_folder(&conn, news, "  Tech  ").unwrap_err();
        assert!(matches!(err, AppError::Coded("folderNameExists")));
    }

    #[test]
    fn create_folder_rejects_an_empty_or_blank_name() {
        // An empty / whitespace-only name must be refused at the DB chokepoint:
        // `import_opml` reaches `create_folder` through `folder_id_by_name`
        // without the `PromptDialog` guard, so a blank label would otherwise
        // insert an unlabelled folder into the sidebar. No row may be created.
        let (conn, _aid) = test_db();
        for blank in ["", "   ", "\t\n"] {
            let err = create_folder(&conn, blank).unwrap_err();
            assert!(
                matches!(err, AppError::Coded("emptyFolderName")),
                "blank name {blank:?} must be rejected"
            );
        }
        assert!(list_folders(&conn).unwrap().is_empty());
    }

    #[test]
    fn rename_folder_rejects_an_empty_or_blank_name() {
        // Renaming a folder to a blank string would leave it unlabelled with
        // no recovery path — rejected the same way `create_folder` is.
        let (conn, _aid) = test_db();
        let tech = create_folder(&conn, "Tech").unwrap();
        for blank in ["", "   ", "\t\n"] {
            let err = rename_folder(&conn, tech, blank).unwrap_err();
            assert!(matches!(err, AppError::Coded("emptyFolderName")));
        }
        // The folder keeps its original name.
        let name: String = conn
            .query_row("SELECT name FROM folders WHERE id = ?1", [tech], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "Tech");
    }
    #[test]
    fn rsshub_route_registry_round_trips() {
        let conn = test_conn();
        assert!(rsshub_routes_list(&conn).unwrap().is_empty());

        rsshub_route_upsert(&conn, "/bilibili/user/video/:id").unwrap();
        rsshub_route_upsert(&conn, "/github/activity/:user").unwrap();
        let rows = rsshub_routes_list(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.enabled));

        // Re-installing keeps the configuration (enabled + env).
        rsshub_route_set_env(
            &conn,
            "/bilibili/user/video/:id",
            r#"{"BILIBILI_COOKIE_1":"x"}"#,
        )
        .unwrap();
        rsshub_route_set_enabled(&conn, "/bilibili/user/video/:id", false).unwrap();
        rsshub_route_upsert(&conn, "/bilibili/user/video/:id").unwrap();
        let row = rsshub_route_get(&conn, "/bilibili/user/video/:id")
            .unwrap()
            .unwrap();
        assert!(!row.enabled, "re-install must not re-enable");
        assert_eq!(row.env_json, r#"{"BILIBILI_COOKIE_1":"x"}"#);

        rsshub_route_delete(&conn, "/github/activity/:user").unwrap();
        assert!(
            rsshub_route_get(&conn, "/github/activity/:user")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rsshub_enabled_env_merges_enabled_rows_only() {
        let conn = test_conn();
        rsshub_route_upsert(&conn, "/a/one/:id").unwrap();
        rsshub_route_upsert(&conn, "/a/two/:id").unwrap();
        rsshub_route_upsert(&conn, "/a/three/:id").unwrap();
        rsshub_route_set_env(&conn, "/a/one/:id", r#"{"SHARED":"first","ONLY_A":"1"}"#).unwrap();
        rsshub_route_set_env(&conn, "/a/two/:id", r#"{"SHARED":"second","ONLY_B":"2"}"#).unwrap();
        // Disabled routes contribute nothing.
        rsshub_route_set_env(&conn, "/a/three/:id", r#"{"ONLY_C":"3"}"#).unwrap();
        rsshub_route_set_enabled(&conn, "/a/three/:id", false).unwrap();

        let env = rsshub_enabled_env(&conn);
        assert_eq!(env.get("SHARED").map(String::as_str), Some("second"));
        assert_eq!(env.get("ONLY_A").map(String::as_str), Some("1"));
        assert_eq!(env.get("ONLY_B").map(String::as_str), Some("2"));
        assert!(!env.contains_key("ONLY_C"));
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn rsshub_enabled_env_skips_malformed_rows() {
        let conn = test_conn();
        rsshub_route_upsert(&conn, "/a/broken/:id").unwrap();
        rsshub_route_upsert(&conn, "/a/good/:id").unwrap();
        rsshub_route_set_env(&conn, "/a/broken/:id", "not json").unwrap();
        rsshub_route_set_env(&conn, "/a/good/:id", r#"{"OK":"yes"}"#).unwrap();
        let env = rsshub_enabled_env(&conn);
        assert_eq!(env.get("OK").map(String::as_str), Some("yes"));
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn rsshub_enabled_env_drops_entries_exec_cannot_carry() {
        // The command layer validates on save; a stale, older or hand-edited
        // row must not wedge the engine spawn with an unexplainable OS error.
        let conn = test_conn();
        rsshub_route_upsert(&conn, "/a/hostile/:id").unwrap();
        rsshub_route_set_env(
            &conn,
            "/a/hostile/:id",
            r#"{"GOOD":"1","BAD\u0000KEY":"2","EQ=KEY":"3","NUL_VAL":"a\u0000b"}"#,
        )
        .unwrap();
        let env = rsshub_enabled_env(&conn);
        assert_eq!(env.get("GOOD").map(String::as_str), Some("1"));
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn rsshub_enabled_env_drops_over_long_entries_from_old_rows() {
        // Rows written before the save path grew its length limits may carry
        // entries past the execve E2BIG threshold — filtered at merge time so
        // one bad value cannot stall every RSSHub subscription.
        let conn = test_conn();
        rsshub_route_upsert(&conn, "/a/legacy/:id").unwrap();
        let long_key = "K".repeat(rsshub::MAX_ENV_KEY_BYTES + 1);
        let long_value = "v".repeat(rsshub::MAX_ENV_VALUE_BYTES + 1);
        let json = format!(r#"{{"GOOD":"1","{long_key}":"2","LONG_VAL":"{long_value}"}}"#);
        rsshub_route_set_env(&conn, "/a/legacy/:id", &json).unwrap();
        let env = rsshub_enabled_env(&conn);
        assert_eq!(env.get("GOOD").map(String::as_str), Some("1"));
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn rsshub_feed_queries_track_the_dedicated_source_type() {
        let conn = test_conn();
        let rss = insert_feed(
            &conn,
            "https://example.com/rss",
            None,
            "rss",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        let hub = insert_feed(
            &conn,
            "rsshub:///github/activity/DIYgod",
            None,
            "hub",
            None,
            SourceType::Rsshub,
            None,
        )
        .unwrap();
        let nl = insert_feed(
            &conn,
            "imap://box",
            None,
            "nl",
            None,
            SourceType::Newsletter,
            None,
        )
        .unwrap();
        let x = insert_feed(
            &conn,
            "https://x.com/jack",
            None,
            "x",
            None,
            SourceType::X,
            None,
        )
        .unwrap();

        // The plain-HTTP set excludes newsletters, parser and rsshub sources.
        let http: Vec<i64> = feeds_to_refresh(&conn)
            .unwrap()
            .into_iter()
            .map(|(id, _, _, _)| id)
            .collect();
        assert_eq!(http, vec![rss]);

        // The engine leg sees exactly the rsshub feeds, with revalidators.
        let hub_feeds = rsshub_feeds_to_refresh(&conn).unwrap();
        assert_eq!(hub_feeds.len(), 1);
        assert_eq!(hub_feeds[0].0, hub);
        assert_eq!(hub_feeds[0].1, "rsshub:///github/activity/DIYgod");
        assert!(hub_feeds[0].2.is_none());

        // Per-feed and per-folder scoping.
        assert_eq!(rsshub_feeds_for_feed(&conn, rss).unwrap().len(), 0);
        assert_eq!(rsshub_feeds_for_feed(&conn, hub).unwrap().len(), 1);
        // OPML export and sync never leak rsshub:// URLs.
        let export = feeds_for_export(&conn).unwrap();
        assert!(
            export
                .iter()
                .all(|(_, url, _)| !url.starts_with("rsshub://"))
        );
        assert_eq!(export.len(), 1, "only the plain rss feed exports");
        let sync = feed_urls_for_sync(&conn).unwrap();
        assert!(sync.iter().all(|u| !u.starts_with("rsshub://")));

        let _ = (nl, x);
    }
}
