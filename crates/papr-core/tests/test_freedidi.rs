// Manual end-to-end check against the live freedidi.com WAF (network test;
// not run in CI — `#[ignore]` needs an explicit opt-in):
// `cargo test -p papr-core --test test_freedidi -- --ignored --nocapture`
use papr_core::ingestion::fetch;

#[tokio::test]
#[ignore = "live-network test against freedidi.com's WAF — run by hand"]
async fn freedidi_feed_resolves_through_challenge() {
    let client = fetch::build_client(30, "system");
    let url = "https://www.freedidi.com/feed";
    for attempt in 1..=2 {
        match fetch::get(&client, url).await {
            Ok((bytes, ct, _final_url)) => {
                let looks_rss = bytes.starts_with(b"<?xml");
                println!("attempt {attempt}: len={} ct={ct:?} rss={looks_rss}", bytes.len());
                assert!(looks_rss, "expected RSS document, got: {}", String::from_utf8_lossy(&bytes[..bytes.len().min(120)]));
            }
            Err(e) => panic!("attempt {attempt} failed: {e:?}"),
        }
    }
}
