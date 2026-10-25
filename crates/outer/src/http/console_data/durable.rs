//! Console Durable Objects viewer backend: read-only class/instance list
//! plus a best-effort storage preview.
//!
//! Ported from Noite `apps/runner/src/host/storage/durable.rs` @ 58a977a
//! (Apache-2.0): `celld cell list --json` walks the fleet bucket directly
//! (no live node needed), reserved celld-internal classes are skipped, and
//! each instance is additionally probed with `GET /do/{scope}?read=1` — the
//! app's own handler decides what the preview says. Adapted to Outer: scoped
//! credentials from [`super::scoped_creds`], the class allowlist from the
//! app's declared `durable_objects`, and the probe split into its own
//! endpoint so a class with many instances lists fast (Noite embeds one
//! probe per row; the probe stays best-effort either way).
//!
//! There is no write path: celld exposes no generic storage API for a DO
//! instance. The UI says so in its empty and help copy.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};

use super::{console_app, ok, scoped_creds, unavailable};

/// `celld cell list` reads the bucket directly; 30 s like Noite's.
const CELLD_TIMEOUT: Duration = Duration::from_secs(30);
/// The app's own `?read=1` handler gets 5 s; anything slower or absent
/// leaves the preview blank rather than failing the list.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// The probe body is a preview, not a dump.
const PROBE_MAX_CHARS: usize = 2000;
/// A list names every instance; bound the parse so a pathological class
/// cannot grow the response without limit.
const MAX_INSTANCES: usize = 2000;

pub fn router() -> Router<super::AppState> {
    Router::new()
        .route("/api/console/apps/{slug}/data/do/classes", get(api_classes))
        .route(
            "/api/console/apps/{slug}/data/do/instances",
            get(api_instances),
        )
        .route("/api/console/apps/{slug}/data/do/probe", get(api_probe))
}

/// One declared Durable Object binding: the name the app code uses and the
/// class behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoBinding {
    pub name: String,
    pub class: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoInstance {
    id: String,
    scope: String,
}

#[derive(Debug, Clone, Deserialize)]
struct InstancesQuery {
    class: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ProbeQuery {
    scope: String,
}

/// Parse `durable_objects` from the built wrangler config: an object with a
/// `bindings` array of `{name, class_name}`. Both travel to `celld cell
/// list` (the class positionally), so both are validated here — anything
/// starting with `-` would otherwise parse as a celld option.
pub fn durable_objects(value: &serde_json::Value) -> anyhow::Result<Vec<DoBinding>> {
    let Some(obj) = value.get("durable_objects") else {
        return Ok(Vec::new());
    };
    let bindings = obj
        .get("bindings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("durable_objects.bindings must be an array"))?;
    bindings
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let entry = entry.as_object().ok_or_else(|| {
                anyhow::anyhow!("durable_objects.bindings[{i}] must be an object")
            })?;
            let name = entry.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                anyhow::anyhow!("durable_objects.bindings[{i}] needs a string name")
            })?;
            let class = entry
                .get("class_name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!("durable_objects.bindings[{i}] needs a string class_name")
                })?;
            validate_do_name(name)
                .map_err(|e| anyhow::anyhow!("durable_objects.bindings[{i}]: {e:#}"))?;
            validate_do_name(class)
                .map_err(|e| anyhow::anyhow!("durable_objects.bindings[{i}]: {e:#}"))?;
            Ok(DoBinding {
                name: name.to_string(),
                class: class.to_string(),
            })
        })
        .collect()
}

/// A binding name or class name: JS-identifier shape, bounded. Class names
/// reach `celld cell list` positionally, so the leading-dash rule closes
/// option injection (no `--` separator: celld 0.6.2 leaf parsers honor
/// neither `--` nor `--help` for positional parsing).
pub(crate) fn validate_do_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("name must be 1..=64 characters");
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('_');
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        anyhow::bail!("name {name:?} must start with a letter, `_` or `$`");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        anyhow::bail!("name {name:?} must be letters, digits, `_` or `$`");
    }
    Ok(())
}

/// Look up the declared class behind a request class name. Anything not
/// declared is refused before celld ever sees it.
pub fn find_class<'a>(bindings: &'a [DoBinding], class: &str) -> anyhow::Result<&'a DoBinding> {
    bindings
        .iter()
        .find(|b| b.class == class)
        .ok_or_else(|| anyhow::anyhow!("unknown Durable Object class {class:?}"))
}

/// The exact argv for `celld cell list`, built pure so tests pin it.
pub fn cell_list_argv(bucket_url: &str, endpoint: &str, region: &str, class: &str) -> Vec<String> {
    vec![
        "cell".to_string(),
        "list".to_string(),
        class.to_string(),
        "--json".to_string(),
        "--bucket".to_string(),
        bucket_url.to_string(),
        "--endpoint".to_string(),
        endpoint.to_string(),
        "--region".to_string(),
        region.to_string(),
    ]
}

/// Parse `celld cell list --json` output: one JSON object per line. Reserved
/// celld-internal classes (D1, KV, queues, …) are skipped — a user class
/// never collides with one, but a collision must not surface internals.
/// Rows without an id are skipped; the scope defaults to the id.
pub fn parse_cell_list(output: &str) -> Vec<DoInstance> {
    let mut instances = Vec::new();
    for line in output.lines() {
        if instances.len() >= MAX_INSTANCES {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if row.get("reserved").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let Some(id) = row.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let scope = row.get("scope").and_then(|v| v.as_str()).unwrap_or(id);
        instances.push(DoInstance {
            id: id.to_string(),
            scope: scope.to_string(),
        });
    }
    instances
}

/// Durable Object classes from the deployed manifest's top-level
/// `do_classes`/`sqlite_classes` lists. Entries are class names as strings
/// (objects with a `class_name` are accepted too); celld-internal classes
/// (`__D1Database` and any `__`-prefixed name) are skipped — a user class
/// never collides with one, but a collision must not surface internals.
/// The manifest carries no binding names, so `name` is the class: the UI
/// shows classes, and the local-config fallback below is the only path
/// that knows binding names.
pub(crate) fn parse_manifest_do(manifest: &serde_json::Value) -> Vec<DoBinding> {
    let mut out = Vec::new();
    for key in ["do_classes", "sqlite_classes"] {
        let Some(list) = manifest.get(key).and_then(|v| v.as_array()) else {
            continue;
        };
        for entry in list {
            let class = match entry {
                serde_json::Value::String(name) => name.as_str(),
                serde_json::Value::Object(_) => entry
                    .get("class_name")
                    .or_else(|| entry.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                _ => continue,
            };
            if class.starts_with("__") {
                continue;
            }
            if validate_do_name(class).is_err() {
                tracing::warn!(class, "skipping manifest DO class that fails validation");
                continue;
            }
            if out.iter().any(|b: &DoBinding| b.class == class) {
                continue;
            }
            out.push(DoBinding {
                name: class.to_string(),
                class: class.to_string(),
            });
        }
    }
    out
}

async fn declared_classes(state: &super::AppState, slug: &str) -> anyhow::Result<Vec<DoBinding>> {
    // The bucket manifest is authoritative whenever present; the local
    // build artifact is only the fallback when it is missing.
    if let Some(manifest) = super::deploy_manifest(state, slug).await? {
        return Ok(parse_manifest_do(&manifest));
    }
    let Some(value) = super::built_config_value(state, slug)? else {
        return Ok(Vec::new());
    };
    durable_objects(&value)
}

async fn api_classes(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
) -> Response {
    let (_account, app) = match console_app(&state, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    match declared_classes(&state, &app.slug).await {
        Ok(bindings) => ok(serde_json::json!({"classes": bindings})),
        Err(err) => unavailable(format!("{err:#}")),
    }
}

async fn api_instances(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
    Query(query): Query<InstancesQuery>,
) -> Response {
    let (_account, app) = match console_app(&state, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    match instances_inner(&state, &app.slug, &query).await {
        Ok(instances) => ok(serde_json::json!({
            "class_name": query.class,
            "instances": instances,
        })),
        Err(err) => {
            let msg = format!("{err:#}");
            if msg.contains("unknown Durable Object class") {
                super::bad_request(msg)
            } else {
                unavailable(msg)
            }
        }
    }
}

async fn instances_inner(
    state: &super::AppState,
    slug: &str,
    query: &InstancesQuery,
) -> anyhow::Result<Vec<DoInstance>> {
    let bindings = declared_classes(state, slug).await?;
    let binding = find_class(&bindings, &query.class)?;
    let settings = super::deploy_settings(state)?;
    let storage = &settings.storage;
    let creds = scoped_creds(state, slug).await?;
    let args = cell_list_argv(
        &storage.bucket_url(slug),
        &storage.endpoint,
        &storage.region,
        &binding.class,
    );
    let output = super::run_celld(
        state,
        &args,
        &super::celld_env(storage, &creds),
        CELLD_TIMEOUT,
    )
    .await
    .map_err(|e| anyhow::anyhow!("celld cell list failed: {e:#}"))?;
    Ok(parse_cell_list(&output))
}

async fn api_probe(
    Path(slug): Path<String>,
    headers: HeaderMap,
    State(state): State<super::AppState>,
    Query(query): Query<ProbeQuery>,
) -> Response {
    let (_account, app) = match console_app(&state, &headers, &slug).await {
        Ok(pair) => pair,
        Err(res) => return res,
    };
    // Best-effort by contract: a sleeping fleet, a missing handler, or any
    // failure answers `preview: null`, never an error.
    let preview = probe_instance(app.internal_port, &query.scope).await;
    ok(serde_json::json!({"scope": query.scope, "preview": preview}))
}

/// `GET /do/{scope}?read=1` on the fleet's internal listener, capped and
/// best-effort. The app's own handler decides what the preview says —
///
/// there is no generic storage read for a Durable Object outside the worker
/// itself. A fleet that is asleep refuses the connection and the preview
/// stays blank; the list endpoint never wakes it.
async fn probe_instance(internal_port: u16, scope: &str) -> Option<String> {
    if scope.is_empty() || scope.len() > 1024 || scope.chars().any(char::is_control) {
        return None;
    }
    let mut url = reqwest::Url::parse(&format!("http://127.0.0.1:{internal_port}/do/")).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .push(scope)
        .pop_if_empty();
    url.set_query(Some("read=1"));
    let body = reqwest::Client::new()
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !body.status().is_success() {
        return None;
    }
    let text = body.text().await.ok()?;
    Some(text.chars().take(PROBE_MAX_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_come_from_declared_durable_objects() {
        let value = serde_json::json!({
            "durable_objects": {
                "bindings": [
                    {"name": "COUNTER", "class_name": "Counter"},
                    {"name": "RATE", "class_name": "RateLimiter"},
                ],
            },
        });
        let bindings = durable_objects(&value).unwrap();
        assert_eq!(
            bindings,
            vec![
                DoBinding {
                    name: "COUNTER".to_string(),
                    class: "Counter".to_string()
                },
                DoBinding {
                    name: "RATE".to_string(),
                    class: "RateLimiter".to_string()
                },
            ]
        );
        assert!(durable_objects(&serde_json::json!({})).unwrap().is_empty());
        assert!(find_class(&bindings, "Counter").is_ok());
        assert!(find_class(&bindings, "Other").is_err());
    }

    #[test]
    fn manifest_do_classes_skip_reserved_and_invalid_entries() {
        let manifest = serde_json::json!({
            "do_classes": ["Counter", "RateLimiter", "__D1Database", "__Other", "", "-evil"],
            "sqlite_classes": ["Counter", {"class_name": "Queue"}],
        });
        let bindings = parse_manifest_do(&manifest);
        // Strings and class_name objects parse; duplicates collapse;
        // celld-internal `__` classes and invalid names are skipped.
        // The manifest carries no binding names, so `name` is the class.
        assert_eq!(
            bindings,
            vec![
                DoBinding {
                    name: "Counter".to_string(),
                    class: "Counter".to_string()
                },
                DoBinding {
                    name: "RateLimiter".to_string(),
                    class: "RateLimiter".to_string()
                },
                DoBinding {
                    name: "Queue".to_string(),
                    class: "Queue".to_string()
                },
            ]
        );
        assert!(parse_manifest_do(&serde_json::json!({})).is_empty());
        assert!(find_class(&bindings, "Counter").is_ok());
        assert!(find_class(&bindings, "__D1Database").is_err());
    }

    #[test]
    fn binding_entries_reject_option_injection_and_bad_shapes() {
        for bad in [
            serde_json::json!({"durable_objects": {"bindings": [{"name": "C", "class_name": "-evil"}]}}),
            serde_json::json!({"durable_objects": {"bindings": [{"name": "has space", "class_name": "C"}]}}),
            serde_json::json!({"durable_objects": {"bindings": [{"name": "C"}]}}),
            serde_json::json!({"durable_objects": {"bindings": "nope"}}),
            serde_json::json!({"durable_objects": []}),
        ] {
            assert!(durable_objects(&bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn cell_list_parsing_skips_reserved_classes() {
        let output = [
            r#"{"id": "abc", "scope": "room-1", "reserved": false}"#,
            r#"{"id": "d1-internal", "scope": "x", "reserved": true}"#,
            r#"{"scope": "no-id"}"#,
            "not json at all",
            "",
            r#"{"id": "kv", "reserved": true}"#,
            r#"{"id": "solo"}"#,
        ]
        .join("\n");
        let instances = parse_cell_list(&output);
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].id, "abc");
        assert_eq!(instances[0].scope, "room-1");
        // A row without a scope falls back to its id.
        assert_eq!(instances[1].id, "solo");
        assert_eq!(instances[1].scope, "solo");
    }

    #[test]
    fn cell_list_argv_pins_bucket_url_and_positions() {
        let args = cell_list_argv("s3://bucket/fleets/my-app", "https://ep", "auto", "Counter");
        assert_eq!(
            args,
            vec![
                "cell",
                "list",
                "Counter",
                "--json",
                "--bucket",
                "s3://bucket/fleets/my-app",
                "--endpoint",
                "https://ep",
                "--region",
                "auto",
            ]
        );
    }
}
