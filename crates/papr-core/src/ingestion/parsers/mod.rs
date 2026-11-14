//! Built-in feed parsers — papr's answer to RSSHub-style routes.
//!
//! A parser recognizes a class of URLs (e.g. `x.com/<user>`) and produces
//! feed data by talking to the site directly, without the site offering an
//! RSS document. Each parser is a module under [`self`], is switched on in
//! Settings → Parsers, and may carry its own credentials there.
//!
//! The registry is deliberately tiny: parser ids live in [`PARSER_IDS`], and
//! per-parser configuration persists as one JSON blob under the `parsers`
//! setting — the same shape/Trade-off as the `integrations` blob (plaintext
//! in the local SQLite settings table; the database never leaves the user's
//! machine). Feeds served by a parser are ordinary `feeds` rows whose
//! `source_type` names the parser's kind, excluded from the plain-HTTP
//! refresh queries and polled by a dedicated leg in `refresh_core` — the
//! exact pattern the newsletter sources already follow.

pub mod kuaishou;
pub mod transport;
pub mod x;
pub mod xiaohongshu;

use crate::db;
use crate::models::SourceType;
use rusqlite::Connection;
use serde::Deserialize;

/// The X (Twitter) parser's registry id.
pub const X: &str = "x";
/// The Xiaohongshu (RED) parser's registry id.
pub const XIAOHONGSHU: &str = "xiaohongshu";
/// The Kuaishou parser's registry id.
pub const KUAISHOU: &str = "kuaishou";

/// Every parser id papr ships, in settings-page display order.
pub const PARSER_IDS: &[&str] = &[X, XIAOHONGSHU, KUAISHOU];

/// The `source_type` values owned by built-in parsers, as a SQL fragment for
/// the `IN (…)` lists the refresh / export / sync queries filter with. Kept
/// in one place so a new parser cannot be half-registered.
pub const PARSER_SOURCE_TYPES_SQL: &str = "'x', 'xiaohongshu', 'kuaishou'";

/// `true` when `source_type` belongs to a built-in parser. Such feeds are
/// excluded from the plain-HTTP refresh / OPML-export / sync queries and are
/// handled by the parser leg instead. Grows with [`PARSER_IDS`] as new
/// parsers land.
pub fn is_parser_source_type(source_type: &str) -> bool {
    source_type == SourceType::X.as_str()
        || source_type == SourceType::Xiaohongshu.as_str()
        || source_type == SourceType::Kuaishou.as_str()
}

/// Configuration of one parser, edited by the settings UI inside the `parsers`
/// blob. Only fields a parser needs are ever written; the rest stay absent.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ParserConfig {
    pub enabled: bool,
    /// X: the `auth_token` session cookie.
    pub auth_token: Option<String>,
    /// X: the `ct0` CSRF cookie (sent back as `X-CSRF-Token`).
    pub ct0: Option<String>,
    /// Xiaohongshu / Kuaishou: the pasted `Cookie` header from a logged-in
    /// browser session. Optional for both — xiaohongshu works anonymously
    /// from the SSR page, kuaishou's web API is captcha-gated without it.
    pub cookie: Option<String>,
}

/// The whole `parsers` settings blob: one entry per parser id.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ParsersSettings {
    pub x: ParserConfig,
    pub xiaohongshu: ParserConfig,
    pub kuaishou: ParserConfig,
}

impl ParsersSettings {
    pub fn config(&self, id: &str) -> &ParserConfig {
        match id {
            X => &self.x,
            XIAOHONGSHU => &self.xiaohongshu,
            KUAISHOU => &self.kuaishou,
            _ => {
                debug_assert!(false, "unknown parser id {id}");
                // A static empty config keeps callers total on stale ids.
                static EMPTY: std::sync::LazyLock<ParserConfig> =
                    std::sync::LazyLock::new(ParserConfig::default);
                &EMPTY
            }
        }
    }
}

/// Read the `parsers` settings blob. A missing, empty or malformed value
/// degrades to the default (nothing enabled) rather than failing — the
/// settings UI owns the blob's shape and repairs it on the next save.
pub fn load_settings(conn: &Connection) -> ParsersSettings {
    db::get_setting(conn, "parsers")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// `true` when parser `id` is switched on. Unknown ids are always off.
pub fn parser_enabled(conn: &Connection, id: &str) -> bool {
    load_settings(conn).config(id).enabled
}

/// The parser's `(auth_token, ct0)` pair, both non-empty. `None` when either
/// cookie is missing/blank — callers must treat credentials as required,
/// never fall back to an unauthenticated scrape.
pub fn x_credentials(conn: &Connection) -> Option<(String, String)> {
    let settings = load_settings(conn);
    let cfg = settings.config(X);
    let auth = cfg.auth_token.as_deref()?.trim();
    let ct0 = cfg.ct0.as_deref()?.trim();
    if auth.is_empty() || ct0.is_empty() {
        return None;
    }
    Some((auth.to_string(), ct0.to_string()))
}

/// A parser's pasted `Cookie` header value (xiaohongshu / kuaishou), trimmed.
/// `None` when absent or blank — callers treat that as "anonymous mode".
pub fn cookie_credentials(conn: &Connection, id: &str) -> Option<String> {
    let settings = load_settings(conn);
    let cfg = settings.config(id);
    let cookie = cfg.cookie.as_deref()?.trim();
    (!cookie.is_empty()).then(|| cookie.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        crate::db::test_conn()
    }

    #[test]
    fn missing_blob_means_default_settings() {
        let conn = conn();
        let settings = load_settings(&conn);
        assert!(!settings.x.enabled);
        assert!(!parser_enabled(&conn, X));
        assert!(x_credentials(&conn).is_none());
    }

    #[test]
    fn malformed_blob_degrades_to_default() {
        let conn = conn();
        db::set_setting(&conn, "parsers", "not json at all").unwrap();
        assert!(!parser_enabled(&conn, X));
        assert!(x_credentials(&conn).is_none());
    }

    #[test]
    fn blob_round_trips_enabled_and_credentials() {
        let conn = conn();
        db::set_setting(
            &conn,
            "parsers",
            r#"{"x":{"enabled":true,"authToken":"abc","ct0":"123"}}"#,
        )
        .unwrap();
        assert!(parser_enabled(&conn, X));
        assert_eq!(x_credentials(&conn), Some(("abc".into(), "123".into())));
    }

    #[test]
    fn blank_credential_fields_are_not_credentials() {
        let conn = conn();
        db::set_setting(
            &conn,
            "parsers",
            r#"{"x":{"enabled":true,"authToken":"  ","ct0":"123"}}"#,
        )
        .unwrap();
        assert!(x_credentials(&conn).is_none());
    }

    #[test]
    fn parser_source_type_matches_only_x() {
        assert!(is_parser_source_type("x"));
        assert!(is_parser_source_type("xiaohongshu"));
        assert!(is_parser_source_type("kuaishou"));
        assert!(!is_parser_source_type("rss"));
        assert!(!is_parser_source_type("newsletter"));
        assert!(!is_parser_source_type("rsshub"));
    }

    #[test]
    fn cookie_credentials_round_trip_and_blank_handling() {
        let conn = conn();
        assert!(cookie_credentials(&conn, XIAOHONGSHU).is_none());
        db::set_setting(
            &conn,
            "parsers",
            r#"{"xiaohongshu":{"enabled":true,"cookie":" a=1; b=2 "}}"#,
        )
        .unwrap();
        assert_eq!(
            cookie_credentials(&conn, XIAOHONGSHU).as_deref(),
            Some("a=1; b=2")
        );
        // A blank cookie is not a credential; an unrelated parser stays empty.
        db::set_setting(
            &conn,
            "parsers",
            r#"{"xiaohongshu":{"enabled":true,"cookie":"   "},"kuaishou":{"enabled":true}}"#,
        )
        .unwrap();
        assert!(cookie_credentials(&conn, XIAOHONGSHU).is_none());
        assert!(cookie_credentials(&conn, KUAISHOU).is_none());
    }

    #[test]
    fn new_parser_configs_default_off() {
        let conn = conn();
        assert!(!parser_enabled(&conn, XIAOHONGSHU));
        assert!(!parser_enabled(&conn, KUAISHOU));
        db::set_setting(&conn, "parsers", "not json").unwrap();
        assert!(!parser_enabled(&conn, XIAOHONGSHU));
    }

    #[test]
    fn registry_and_db_source_type_lists_agree() {
        // The refresh leg filters by `db::PARSER_SOURCE_TYPES_IN`; the plain
        // HTTP queries exclude everything it names plus newsletter/rsshub.
        // A new parser registered in one place but not the other would either
        // never refresh or be fetched as a feed document — pin the two.
        let parser_ids: Vec<&str> = PARSER_IDS.to_vec();
        for id in parser_ids {
            assert!(
                db::PARSER_SOURCE_TYPES_IN.contains(&format!("'{id}'").as_str()),
                "parser '{id}' missing from db::PARSER_SOURCE_TYPES_IN"
            );
            assert!(
                db::NON_HTTP_SOURCE_TYPES.contains(&format!("'{id}'").as_str()),
                "parser '{id}' missing from db::NON_HTTP_SOURCE_TYPES"
            );
            assert!(
                is_parser_source_type(id),
                "registry id '{id}' is not a parser source type"
            );
        }
        // The SQL lists name no parser the registry does not know.
        for quoted in ["'x'", "'xiaohongshu'", "'kuaishou'"] {
            let id = quoted.trim_matches('\'');
            assert!(PARSER_IDS.contains(&id), "db list has unknown parser '{id}'");
        }
    }
}
