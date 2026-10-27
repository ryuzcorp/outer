//! The live manifest celld publishes per upload: `deploy/current.json`
//! carries the versioned `prefix`, and the authoritative deployed config is
//! the manifest at `fleets/{slug}/{prefix}/manifest.json`. There is no
//! `fleets/{slug}/wrangler.json` in the bucket, and the worktree
//! `dist/wrangler.json` is a wipeable build artifact — so the manifest is
//! the only source of truth. This is the ONE manifest reader for every
//! consumer: the console data panels parse bindings out of it, and the
//! deploy pipeline reads the live version's declared D1 databases out of it
//! to decide the migrate/upload order. `vars` — which may carry injected
//! secret values — is never read here and never lands in a response.
//!
//! A missing pointer or manifest means never-deployed (`None`); malformed
//! JSON or an unsafe prefix is an error.

use crate::deploy::migrate::{self, D1Database};
use crate::store::Store;

/// The deploy pointer's versioned prefix must stay a relative deploy path:
/// it becomes a store key suffix, so anything absolute or escaping fails the
/// read instead of probing an unintended key.
pub fn validate_prefix(prefix: &str) -> anyhow::Result<()> {
    if prefix.is_empty()
        || prefix.starts_with('/')
        || prefix.contains("..")
        || prefix.contains('\\')
        || prefix.contains('\0')
    {
        anyhow::bail!("deploy pointer prefix {prefix:?} is not a relative deploy path");
    }
    Ok(())
}

/// The live manifest for `slug`, via the scoped store. A missing pointer or
/// manifest means never-deployed (`None`); malformed JSON or an unsafe
/// prefix is an error.
pub async fn deployed_manifest(
    store: &Store,
    slug: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let pointer_key = format!("fleets/{slug}/deploy/current.json");
    let Some(pointer) = store.get(&pointer_key).await? else {
        return Ok(None);
    };
    let pointer: serde_json::Value = serde_json::from_slice(&pointer)
        .map_err(|e| anyhow::anyhow!("parsing deploy pointer: {e:#}"))?;
    let prefix = pointer
        .get("prefix")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("deploy pointer has no prefix"))?;
    validate_prefix(prefix)?;
    let key = format!("fleets/{slug}/{prefix}/manifest.json");
    let Some(manifest) = store.get(&key).await? else {
        return Ok(None);
    };
    let manifest: serde_json::Value = serde_json::from_slice(&manifest)
        .map_err(|e| anyhow::anyhow!("parsing deployed manifest: {e:#}"))?;
    Ok(Some(manifest))
}

/// The live manifest's declared D1 databases, from `raw_metadata.bindings`:
/// `type: "d1"` entries map `name` → binding and
/// `database_name`/`database_id` → identity. Other binding types are ignored.
/// `vars` — which may carry injected secret values — is never read here and
/// never lands in the synthesized cwd config, so secrets can't leak through
/// the materialized project dir or the fake-celld-style logs that read it.
///
/// An asset-only live version (no `main_module`) declares nothing: celld
/// serves it without a worker, so there is no live code that could reach a
/// database, and migrations must not wait on it.
pub fn manifest_dbs(manifest: &serde_json::Value) -> anyhow::Result<Vec<D1Database>> {
    if manifest
        .get("main_module")
        .and_then(|v| v.as_str())
        .is_none()
    {
        return Ok(Vec::new());
    }
    let Some(bindings) = manifest.pointer("/raw_metadata/bindings") else {
        return Ok(Vec::new());
    };
    let bindings = bindings.as_array().ok_or_else(|| {
        anyhow::anyhow!("deployed manifest raw_metadata.bindings must be an array")
    })?;
    let mut dbs = Vec::new();
    for (i, entry) in bindings.iter().enumerate() {
        let obj = entry
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("deployed manifest bindings[{i}] must be an object"))?;
        let kind = obj.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
            anyhow::anyhow!("deployed manifest bindings[{i}] needs a string type")
        })?;
        if kind != "d1" {
            continue;
        }
        let binding = obj.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            anyhow::anyhow!("deployed manifest bindings[{i}] needs a string name")
        })?;
        let database_name = obj
            .get("database_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("deployed manifest bindings[{i}] needs a string database_name")
            })?;
        let database_id = match obj.get("database_id") {
            None => None,
            Some(v) => {
                let id = v.as_str().ok_or_else(|| {
                    anyhow::anyhow!("deployed manifest bindings[{i}] needs a string database_id")
                })?;
                migrate::validate_database_id(id)
                    .map_err(|e| anyhow::anyhow!("deployed manifest bindings[{i}]: {e:#}"))?;
                Some(id.to_string())
            }
        };
        migrate::validate_database_name(database_name)
            .map_err(|e| anyhow::anyhow!("deployed manifest bindings[{i}]: {e:#}"))?;
        dbs.push(D1Database {
            binding: binding.to_string(),
            database_name: database_name.to_string(),
            database_id,
            migrations_dir: None,
        });
    }
    Ok(dbs)
}

/// The `database_name`s in `wanted` the live version does not declare, by
/// name. The pipeline migrates by name (that is what reaches celld's argv),
/// so the order decision is by name too: any name here means the live fleet
/// cannot reach that database and the upload must go first.
pub fn missing_from_live(live: &[D1Database], wanted: &[String]) -> Vec<String> {
    wanted
        .iter()
        .filter(|name| !live.iter().any(|db| &db.database_name == *name))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(main: Option<&str>, bindings: serde_json::Value) -> serde_json::Value {
        let mut manifest = serde_json::json!({
            "schema_version": 2,
            "script_name": "outer-app",
            "raw_metadata": {
                "bindings": bindings,
                "compatibility_date": "2026-09-01",
            }
        });
        if let Some(main) = main {
            manifest["main_module"] = serde_json::Value::String(main.to_string());
        }
        manifest
    }

    #[test]
    fn asset_only_live_manifest_declares_nothing() {
        // A static site's manifest has no worker: even with d1-shaped
        // bindings present, there is no live code that could reach a
        // database, so the pipeline must treat every database as missing.
        let dbs = manifest_dbs(&manifest_with(
            None,
            serde_json::json!([{"type": "d1", "name": "DB", "database_name": "site-db"}]),
        ))
        .unwrap();
        assert!(dbs.is_empty());
        let bare = serde_json::json!({"schema_version": 2, "script_name": "x"});
        assert!(manifest_dbs(&bare).unwrap().is_empty());
    }

    #[test]
    fn worker_manifest_maps_d1_bindings() {
        let dbs = manifest_dbs(&manifest_with(
            Some("index.js"),
            serde_json::json!([
                {"type": "d1", "name": "DB", "database_name": "app-db", "database_id": "abc-123"},
                {"type": "kv_namespace", "name": "CACHE", "namespace_id": "kv-1"},
            ]),
        ))
        .unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].binding, "DB");
        assert_eq!(dbs[0].effective_identity(), "abc-123");
    }

    #[test]
    fn missing_from_live_reports_undeclared_names() {
        let live = manifest_dbs(&manifest_with(
            Some("index.js"),
            serde_json::json!([{"type": "d1", "name": "DB", "database_name": "app-db"}]),
        ))
        .unwrap();
        assert!(
            missing_from_live(&live, &["app-db".to_string()]).is_empty(),
            "a declared database needs no upload-first"
        );
        assert_eq!(
            missing_from_live(&live, &["app-db".to_string(), "site-db".to_string()]),
            vec!["site-db".to_string()]
        );
    }
}
