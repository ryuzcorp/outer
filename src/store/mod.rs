//! Object store: Cloudflare R2 through its S3 API, plus an in-memory
//! backend for tests and local dev.
//!
//! The git slice reads and writes bundles through [`Store`]; the snapshot
//! module keeps the SQLite state in the same bucket.

pub mod s3;
pub mod snapshot;

use std::collections::BTreeMap;
use std::path::Path;

pub use s3::S3Backend;

/// Connection parameters for the R2 bucket (S3 API).
pub struct S3Config {
    /// R2 endpoint, e.g. `https://<account>.eu.r2.cloudflarestorage.com`.
    pub endpoint: String,
    /// Region: `"auto"` for R2.
    pub region: String,
    /// Bucket name.
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// R2 temporary credentials; sent as `x-amz-security-token` on every
    /// signed request when present.
    pub session_token: Option<String>,
}

/// Object store handle. Both backends share the same semantics: `get` and
/// `get_to_file` report a missing key as `None`/`false` (never an error),
/// `delete` treats a missing key as Ok, and `list`/`delete_prefix` match by
/// plain key prefix.
#[derive(Clone)]
pub struct Store {
    inner: StoreInner,
}

#[derive(Clone)]
enum StoreInner {
    S3(std::sync::Arc<S3Backend>),
    Memory(std::sync::Arc<std::sync::Mutex<BTreeMap<String, Vec<u8>>>>),
}

impl Store {
    /// R2/S3-backed store. Fails fast when the endpoint is not a URL.
    pub fn s3(cfg: S3Config) -> anyhow::Result<Store> {
        Ok(Store {
            inner: StoreInner::S3(std::sync::Arc::new(S3Backend::new(&cfg)?)),
        })
    }

    /// In-memory store for tests and local dev; behaves like the bucket.
    pub fn memory() -> Store {
        Store {
            inner: StoreInner::Memory(std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new()))),
        }
    }

    /// Small-object upload (manifests, meta, owner claims).
    pub async fn put(&self, key: &str, body: Vec<u8>) -> anyhow::Result<()> {
        match &self.inner {
            StoreInner::S3(backend) => backend.put(key, body).await,
            StoreInner::Memory(map) => {
                lock(map).insert(key.to_string(), body);
                Ok(())
            }
        }
    }

    /// File upload that streams large files (bundles, snapshots) instead of
    /// buffering them whole.
    pub async fn put_file(&self, key: &str, path: &Path) -> anyhow::Result<()> {
        match &self.inner {
            StoreInner::S3(backend) => backend.put_file(key, path).await,
            StoreInner::Memory(map) => {
                let bytes = tokio::fs::read(path).await?;
                lock(map).insert(key.to_string(), bytes);
                Ok(())
            }
        }
    }

    /// Whole-object download. `None` means the key does not exist (404).
    pub async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match &self.inner {
            StoreInner::S3(backend) => backend.get(key).await,
            StoreInner::Memory(map) => Ok(lock(map).get(key).cloned()),
        }
    }

    /// Object download streamed to `dest`. `false` means the key does not
    /// exist (404); no partial file is left behind in that case.
    pub async fn get_to_file(&self, key: &str, dest: &Path) -> anyhow::Result<bool> {
        match &self.inner {
            StoreInner::S3(backend) => backend.get_to_file(key, dest).await,
            StoreInner::Memory(map) => {
                let bytes = lock(map).get(key).cloned();
                let Some(bytes) = bytes else { return Ok(false) };
                tokio::fs::write(dest, bytes).await?;
                Ok(true)
            }
        }
    }

    /// Delete one key. A missing key is Ok.
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        match &self.inner {
            StoreInner::S3(backend) => backend.delete(key).await,
            StoreInner::Memory(map) => {
                lock(map).remove(key);
                Ok(())
            }
        }
    }

    /// All keys under `prefix`, in sort order.
    pub async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        match &self.inner {
            StoreInner::S3(backend) => backend.list(prefix).await,
            StoreInner::Memory(map) => Ok(lock(map)
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect()),
        }
    }

    /// Delete every key under `prefix`. An empty match is a no-op.
    pub async fn delete_prefix(&self, prefix: &str) -> anyhow::Result<()> {
        match &self.inner {
            StoreInner::S3(backend) => backend.delete_prefix(prefix).await,
            StoreInner::Memory(map) => {
                lock(map).retain(|k, _| !k.starts_with(prefix));
                Ok(())
            }
        }
    }
}

fn lock(
    map: &std::sync::Arc<std::sync::Mutex<BTreeMap<String, Vec<u8>>>>,
) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
    map.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_put_get_roundtrip_overwrites() {
        let store = Store::memory();
        assert_eq!(store.get("git/demo/MANIFEST.json").await.unwrap(), None);
        store
            .put("git/demo/MANIFEST.json", b"{}".to_vec())
            .await
            .unwrap();
        assert_eq!(
            store.get("git/demo/MANIFEST.json").await.unwrap(),
            Some(b"{}".to_vec())
        );
        store
            .put("git/demo/MANIFEST.json", b"{\"a\":1}".to_vec())
            .await
            .unwrap();
        assert_eq!(
            store.get("git/demo/MANIFEST.json").await.unwrap(),
            Some(b"{\"a\":1}".to_vec())
        );
    }

    #[tokio::test]
    async fn memory_delete_missing_is_ok_and_removes() {
        let store = Store::memory();
        store.delete("no/such/key").await.unwrap();
        store.put("k", b"v".to_vec()).await.unwrap();
        store.delete("k").await.unwrap();
        assert_eq!(store.get("k").await.unwrap(), None);
        store.delete("k").await.unwrap();
    }

    #[tokio::test]
    async fn memory_list_matches_prefix_in_sort_order() {
        let store = Store::memory();
        for key in [
            "git/b/MANIFEST.json",
            "git/a/MANIFEST.json",
            "git/a/main/x.bundle",
            "fleets/a/image.tar",
        ] {
            store.put(key, b"x".to_vec()).await.unwrap();
        }
        assert_eq!(
            store.list("git/a/").await.unwrap(),
            vec!["git/a/MANIFEST.json", "git/a/main/x.bundle"]
        );
        assert_eq!(
            store.list("git/").await.unwrap(),
            vec![
                "git/a/MANIFEST.json",
                "git/a/main/x.bundle",
                "git/b/MANIFEST.json",
            ]
        );
        assert_eq!(store.list("").await.unwrap().len(), 4);
        assert!(store.list("nothing/").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn memory_delete_prefix_removes_only_its_scope() {
        let store = Store::memory();
        for key in ["git/a/1", "git/a/2", "git/b/1"] {
            store.put(key, b"x".to_vec()).await.unwrap();
        }
        store.delete_prefix("git/a/").await.unwrap();
        assert_eq!(store.list("").await.unwrap(), vec!["git/b/1"]);
        // Empty matches are a no-op, so retries after a partial delete stay quiet.
        store.delete_prefix("git/a/").await.unwrap();
        store.delete_prefix("").await.unwrap();
        assert!(store.list("").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn memory_file_roundtrip_streams_through_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("up.bundle");
        let dest = dir.path().join("down.bundle");
        tokio::fs::write(&src, vec![7u8; 100_000]).await.unwrap();
        let store = Store::memory();
        store.put_file("git/a/x.bundle", &src).await.unwrap();
        assert!(store.get_to_file("git/a/x.bundle", &dest).await.unwrap());
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), vec![7u8; 100_000]);
        assert!(!store.get_to_file("git/a/missing", &dest).await.unwrap());
    }
}
