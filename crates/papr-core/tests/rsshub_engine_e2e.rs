//! Real-engine end-to-end refresh test.
//!
//! Ignored by default: it needs the engine archive produced by
//! `scripts/build-engine.sh` (`target/engine/*.tar.gz`) and network access
//! (the route crawls github.com). Run it explicitly with
//! `cargo test -p papr-core --test rsshub_engine_e2e -- --ignored`.
//!
//! Covers the full in-app loop the unit tests only cover with fakes:
//! engine archive install → `rsshub://` feed + installed template →
//! `refresh_core`'s engine leg → feed-rs parse → articles in the database.

use std::path::PathBuf;

use papr_core::db;
use papr_core::ingestion::fetch;
use papr_core::ingestion::refresh::{refresh_core, RefreshScope};
use papr_core::ingestion::rsshub::engine::EngineManager;
use papr_core::models::SourceType;

fn engine_archive() -> PathBuf {
    let engine_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/engine");
    std::fs::read_dir(&engine_dir)
        .expect("target/engine missing — run scripts/build-engine.sh first")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "gz"))
        .expect("no engine archive in target/engine — run scripts/build-engine.sh first")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "heavy: boots the real engine from target/engine and crawls github.com"]
async fn real_engine_refresh_ingests_articles() {
    let archive = engine_archive();

    let engine_root = tempfile::tempdir().expect("engine root");
    let manager = EngineManager::new(engine_root.path().to_path_buf());
    manager
        .install_from_archive(&archive)
        .await
        .expect("engine install from archive");
    assert!(manager.installed());

    let db_dir = tempfile::tempdir().expect("db dir");
    let conn = db::open(&db_dir.path().join("papr.db")).expect("open database");
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
