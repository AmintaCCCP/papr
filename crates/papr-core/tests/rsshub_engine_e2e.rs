//! Real-engine end-to-end refresh test.
//!
//! Ignored by default: it downloads Bun and installs `rsshub@latest` from npm,
//! then fetches a real `rsshub://` route (which requires network access).
//! Run it explicitly with:
//! `cargo test -p papr-core --test rsshub_engine_e2e -- --ignored --nocapture`
//!
//! Covers the full in-app loop the unit tests only cover with fakes:
//! `install_or_update` (Bun bootstrap + `rsshub@latest` npm install) →
//! `rsshub://` feed + installed template → `refresh_core`'s engine leg →
//! feed-rs parse → articles in the database.

use papr_core::db;
use papr_core::ingestion::fetch;
use papr_core::ingestion::refresh::{refresh_core, RefreshScope};
use papr_core::ingestion::rsshub::engine::{EngineManager, EngineProxyMode};
use papr_core::models::SourceType;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "heavy: downloads Bun + installs rsshub@latest from npm and crawls github.com"]
async fn real_engine_refresh_ingests_articles() {
    let engine_root = tempfile::tempdir().expect("engine root");
    let manager = EngineManager::new(engine_root.path().to_path_buf());

    let db_dir = tempfile::tempdir().expect("db dir");
    let conn = db::open(&db_dir.path().join("papr.db")).expect("open database");
    if let Ok(proxy_val) = std::env::var("PAPR_E2E_PROXY") {
        db::set_setting(&conn, "net_proxy", &proxy_val).expect("set net_proxy");
    }
    let dl_client = fetch::build_download_client(&conn);
    let proxy_setting = db::get_setting(&conn, "net_proxy").ok().flatten();
    let proxy_mode = EngineProxyMode::from_setting(proxy_setting.as_deref());
    let use_cn_mirror = std::env::var("PAPR_E2E_CN_MIRROR")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let info = manager
        .install_or_update(&dl_client, use_cn_mirror, &proxy_mode)
        .await
        .expect("engine install_or_update");
    assert_eq!(info.version, "2");
    assert!(!info.rsshub_version.is_empty());
    assert!(!info.bun_version.is_empty());
    assert!(manager.installed());

    let db = tokio::sync::Mutex::new(conn);

    // Install the route template that gates the feed, then subscribe the
    // way the UI does: a verbatim `rsshub://` feed URL.
    let feed_id = {
        let conn = db.lock().await;
        db::rsshub_route_upsert(&conn, "/github/activity/:user").expect("install route");
        db::insert_feed(
            &conn,
            "rsshub:///github/activity/DIYgod",
            None,
            "DIYgod's GitHub activity",
            None,
            SourceType::Rsshub,
            None,
        )
        .expect("insert rsshub feed")
    };

    let client = fetch::build_client(30, "none");
    let summary = refresh_core(&db, &client, Some(&manager), RefreshScope::All, |_| {})
        .await
        .expect("refresh");

    let ingested: i64 = {
        let conn = db.lock().await;
        conn.query_row(
            "SELECT COUNT(*) FROM articles WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )
        .expect("count articles")
    };

    manager.shutdown().await;

    assert!(
        summary.ran,
        "an All-scope refresh must run the pipeline, not skip it"
    );
    assert!(
        ingested > 0,
        "the engine route should have ingested articles, got {ingested} (summary: {summary:?})"
    );
}
