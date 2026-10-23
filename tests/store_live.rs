//! `Store` against a real S3-compatible server. Ignored by default: it needs
//! one. Run it against a local RustFS (or an R2 bucket) with:
//!
//! ```bash
//! podman run -d --name outer-rustfs -p 127.0.0.1:19000:9000 \
//!   -e RUSTFS_ACCESS_KEY=outeraccess -e RUSTFS_SECRET_KEY=outersecretoutersecret12 \
//!   docker.io/rustfs/rustfs:1.0.3 /data
//! OUTER_TEST_S3_ENDPOINT=http://127.0.0.1:19000 OUTER_TEST_S3_KEY=outeraccess \
//!   OUTER_TEST_S3_SECRET=outersecretoutersecret12 cargo test --test store_live -- --ignored
//! ```
//!
//! The test creates its bucket when it's missing (RustFS starts empty).

use std::time::Duration;

use outer::store::{S3Config, Store};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live store test"))
}

async fn ensure_bucket(endpoint: &str, bucket: &str, key: &str, secret: &str) {
    let b = Bucket::new(
        endpoint.parse().unwrap(),
        UrlStyle::Path,
        bucket.to_string(),
        "us-east-1".to_string(),
    )
    .unwrap();
    let creds = Credentials::new(key, secret);
    let url = b.create_bucket(&creds).sign(Duration::from_secs(60));
    let status = reqwest::Client::new()
        .put(url)
        .send()
        .await
        .unwrap()
        .status();
    // 200 created, 409 already owned by us.
    assert!(
        status.is_success() || status.as_u16() == 409,
        "create bucket: {status}"
    );
}

#[tokio::test]
#[ignore = "needs an S3-compatible server; see the module docs"]
async fn store_contract_holds_against_a_real_bucket() {
    let endpoint = env("OUTER_TEST_S3_ENDPOINT");
    let (key, secret) = (env("OUTER_TEST_S3_KEY"), env("OUTER_TEST_S3_SECRET"));
    let bucket =
        std::env::var("OUTER_TEST_S3_BUCKET").unwrap_or_else(|_| "outer-live-test".to_string());
    ensure_bucket(&endpoint, &bucket, &key, &secret).await;
    let store = Store::s3(S3Config {
        endpoint,
        region: std::env::var("OUTER_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
        bucket,
        access_key_id: key,
        secret_access_key: secret,
        session_token: None,
    })
    .unwrap();
    let run = uuid::Uuid::new_v4().to_string();
    let prefix = format!("live/{run}/");

    // Missing keys: None, false, and an Ok delete.
    assert_eq!(store.get(&format!("{prefix}missing")).await.unwrap(), None);
    let dir = tempfile::tempdir().unwrap();
    assert!(!store
        .get_to_file(&format!("{prefix}missing"), &dir.path().join("x"))
        .await
        .unwrap());
    store.delete(&format!("{prefix}missing")).await.unwrap();

    // Round trips, in memory and through files (8 MiB crosses any chunk size).
    store
        .put(&format!("{prefix}small"), b"hello".to_vec())
        .await
        .unwrap();
    assert_eq!(
        store
            .get(&format!("{prefix}small"))
            .await
            .unwrap()
            .as_deref(),
        Some(&b"hello"[..])
    );
    let big: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let src = dir.path().join("big.bin");
    std::fs::write(&src, &big).unwrap();
    store.put_file(&format!("{prefix}big"), &src).await.unwrap();
    let dest = dir.path().join("big.out");
    assert!(store
        .get_to_file(&format!("{prefix}big"), &dest)
        .await
        .unwrap());
    assert!(
        std::fs::read(&dest).unwrap() == big,
        "streamed download must match the upload"
    );

    // Listing paginates past the 1000-key page.
    let many = format!("{prefix}many/");
    for i in 0..1005 {
        store.put(&format!("{many}{i:04}"), vec![]).await.unwrap();
    }
    let listed = store.list(&many).await.unwrap();
    assert_eq!(listed.len(), 1005);
    assert!(listed.iter().all(|k| k.starts_with(&many)));

    // delete_prefix removes everything under the prefix and nothing else.
    store.delete_prefix(&many).await.unwrap();
    assert!(store.list(&many).await.unwrap().is_empty());
    assert!(store
        .get(&format!("{prefix}small"))
        .await
        .unwrap()
        .is_some());
    store.delete_prefix(&prefix).await.unwrap();
    assert!(store.list(&prefix).await.unwrap().is_empty());
}
