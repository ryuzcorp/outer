//! Configuration: one struct, read once from the environment at boot.
//!
//! Every variable has exactly one name, defaults live here, and `validate`
//! reports every problem at once instead of failing on the first.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Where platform objects (git bundles, app storage, the state snapshot) live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreKind {
    /// Cloudflare R2 through its S3 API. Required off localhost.
    R2(R2Config),
    /// In-process map, lost on restart. Local development only.
    Memory,
}

#[derive(Clone, PartialEq, Eq)]
pub struct R2Config {
    /// `https://<account>.eu.r2.cloudflarestorage.com` for an EU bucket.
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for R2Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("R2Config")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Platform host; apps live at `<slug>.<domain>` (`OUTER_DOMAIN`).
    pub domain: String,
    /// SQLite, git mirrors, worktrees and caches (`OUTER_DATA`).
    pub data_dir: PathBuf,
    /// Plain-HTTP listener (`OUTER_HTTP_LISTEN`).
    pub http_listen: SocketAddr,
    /// This host's identity for the snapshot owner fence (`OUTER_HOST_ID`).
    pub host_id: String,
    /// `OUTER_STORE=r2|memory` plus the R2 variables.
    pub store: StoreKind,
    /// Log filter directives (`RUST_LOG`, default `info`).
    pub log_filter: String,
}

const DEFAULT_DOMAIN: &str = "outer.localhost";
const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

impl Config {
    pub fn from_env() -> Result<Config, Vec<String>> {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        Config::from_vars(&vars)
    }

    /// Parse and validate. Pure, so tests pass a map instead of the process env.
    pub fn from_vars(vars: &BTreeMap<String, String>) -> Result<Config, Vec<String>> {
        let get = |name: &str| vars.get(name).map(|v| v.trim()).filter(|v| !v.is_empty());
        let mut problems = Vec::new();

        let domain = get("OUTER_DOMAIN")
            .unwrap_or(DEFAULT_DOMAIN)
            .to_ascii_lowercase();
        let data_dir = PathBuf::from(get("OUTER_DATA").unwrap_or("./data"));
        let http_listen = match get("OUTER_HTTP_LISTEN").unwrap_or(DEFAULT_LISTEN).parse() {
            Ok(addr) => addr,
            Err(_) => {
                problems.push(format!(
                    "OUTER_HTTP_LISTEN={:?} is not a socket address (expected host:port, e.g. {DEFAULT_LISTEN})",
                    get("OUTER_HTTP_LISTEN").unwrap_or_default()
                ));
                DEFAULT_LISTEN
                    .parse()
                    .expect("default listen address parses")
            }
        };
        let host_id = get("OUTER_HOST_ID")
            .map(str::to_string)
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|h| h.trim().to_string())
            })
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "outer".to_string());

        let store = match get("OUTER_STORE").unwrap_or("r2") {
            "memory" => StoreKind::Memory,
            "r2" => {
                let mut required = |name: &str| match get(name) {
                    Some(v) => v.to_string(),
                    None => {
                        problems.push(format!("{name} is required when OUTER_STORE=r2"));
                        String::new()
                    }
                };
                StoreKind::R2(R2Config {
                    endpoint: required("OUTER_R2_ENDPOINT"),
                    bucket: required("OUTER_R2_BUCKET"),
                    access_key_id: required("R2_ACCESS_KEY_ID"),
                    secret_access_key: required("R2_SECRET_ACCESS_KEY"),
                })
            }
            other => {
                problems.push(format!("OUTER_STORE={other:?} is not one of: r2, memory"));
                StoreKind::Memory
            }
        };

        let log_filter = get("RUST_LOG").unwrap_or("info").to_string();
        if let Err(err) = tracing_subscriber::EnvFilter::try_new(&log_filter) {
            problems.push(format!(
                "RUST_LOG={log_filter:?} is not a valid filter: {err}"
            ));
        }

        let config = Config {
            domain,
            data_dir,
            http_listen,
            host_id,
            store,
            log_filter,
        };
        problems.extend(config.validate());
        if problems.is_empty() {
            Ok(config)
        } else {
            Err(problems)
        }
    }

    /// Cross-field rules that keep a production boot from running on dev settings.
    fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !is_hostname(&self.domain) {
            problems.push(format!("OUTER_DOMAIN={:?} is not a hostname", self.domain));
        }
        if self.store == StoreKind::Memory && !self.is_local() {
            problems.push(format!(
                "OUTER_STORE=memory loses every app on restart and is refused for OUTER_DOMAIN={}; use r2",
                self.domain
            ));
        }
        if let StoreKind::R2(r2) = &self.store {
            if !r2.endpoint.is_empty() && !r2.endpoint.starts_with("https://") && !self.is_local() {
                problems.push(format!("OUTER_R2_ENDPOINT={:?} must be https", r2.endpoint));
            }
        }
        problems
    }

    /// The `localhost` family: dev-only settings are allowed here and nowhere else.
    pub fn is_local(&self) -> bool {
        self.domain == "localhost" || self.domain.ends_with(".localhost")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("outer.sqlite")
    }
}

fn is_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn local_dev_runs_on_the_memory_store_with_defaults() {
        let cfg = Config::from_vars(&vars(&[("OUTER_STORE", "memory")])).unwrap();
        assert_eq!(cfg.domain, "outer.localhost");
        assert_eq!(cfg.store, StoreKind::Memory);
        assert_eq!(cfg.http_listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(cfg.db_path(), PathBuf::from("./data/outer.sqlite"));
    }

    #[test]
    fn production_refuses_the_memory_store() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            ("OUTER_STORE", "memory"),
        ]))
        .unwrap_err();
        assert!(
            err.iter().any(|p| p.contains("OUTER_STORE=memory")),
            "{err:?}"
        );
    }

    #[test]
    fn r2_reports_every_missing_variable_at_once() {
        let err = Config::from_vars(&vars(&[("OUTER_DOMAIN", "outer.now")])).unwrap_err();
        for name in [
            "OUTER_R2_ENDPOINT",
            "OUTER_R2_BUCKET",
            "R2_ACCESS_KEY_ID",
            "R2_SECRET_ACCESS_KEY",
        ] {
            assert!(
                err.iter().any(|p| p.starts_with(name)),
                "missing {name} in {err:?}"
            );
        }
    }

    #[test]
    fn r2_config_parses_and_never_prints_the_secret() {
        let cfg = Config::from_vars(&vars(&[
            ("OUTER_DOMAIN", "outer.now"),
            (
                "OUTER_R2_ENDPOINT",
                "https://acct.eu.r2.cloudflarestorage.com",
            ),
            ("OUTER_R2_BUCKET", "outer"),
            ("R2_ACCESS_KEY_ID", "key"),
            ("R2_SECRET_ACCESS_KEY", "s3cret"),
        ]))
        .unwrap();
        let StoreKind::R2(r2) = &cfg.store else {
            panic!("expected r2")
        };
        assert_eq!(r2.bucket, "outer");
        assert!(!format!("{cfg:?}").contains("s3cret"));
    }

    #[test]
    fn rejects_bad_listen_address_and_bad_domain_together() {
        let err = Config::from_vars(&vars(&[
            ("OUTER_STORE", "memory"),
            ("OUTER_DOMAIN", "Not A Host"),
            ("OUTER_HTTP_LISTEN", "eighty"),
        ]))
        .unwrap_err();
        assert!(
            err.iter().any(|p| p.starts_with("OUTER_HTTP_LISTEN")),
            "{err:?}"
        );
        assert!(err.iter().any(|p| p.starts_with("OUTER_DOMAIN")), "{err:?}");
    }
}
