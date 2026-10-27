// Derived from noite apps/runner/src/host/netisolation.rs @ 58a977a (Apache-2.0).
// Outer change: a single per-app uid range (no shared fleet uid, always
// multi); TCP 25/465/587 blocked; per-uid new-connection rate limits and
// named per-uid byte counters.

use std::collections::HashMap;
use std::net::IpAddr;
use std::process::Stdio;
use std::time::Duration;

/// Everything the ruleset is built from. `uid_range` is (base, count) over
/// the reserved per-app uid range; `free_uids`/`pro_uids` name the uids in
/// each plan class for the byte counters.
pub struct NftConfig {
    pub uid_range: (u32, u32),
    pub storage_addrs: Vec<IpAddr>,
    pub storage_port: u16,
    pub free_uids: Vec<u32>,
    pub pro_uids: Vec<u32>,
}

/// The nftables `skuid` match for the per-app range: nft takes a `lo-hi`
/// range, so a whole range is one rule instead of one rule per app. A
/// one-uid range degrades to a plain match.
fn range_match(range: (u32, u32)) -> String {
    if range.1 <= 1 {
        format!("{}", range.0)
    } else {
        format!("{}-{}", range.0, range.0 + range.1 - 1)
    }
}

/// Every uid the policy counts: the union of both plan classes, sorted and
/// deduplicated.
fn counted_uids(cfg: &NftConfig) -> Vec<u32> {
    let mut uids: Vec<u32> = cfg.free_uids.iter().chain(&cfg.pro_uids).copied().collect();
    uids.sort_unstable();
    uids.dedup();
    uids
}

/// The nftables ruleset for the per-app uid range. Pure so tests pin it.
///
/// Order matters: the first matching rule wins. Replies go first, then DNS
/// (which must be accepted before the private/loopback rejects, because the
/// container resolver lives exactly there), then the storage allowance, then
/// the SMTP and private-range rejects, then the per-uid rate-limit drops, and
/// finally the per-uid counters on egress.
pub fn ruleset(cfg: &NftConfig) -> String {
    let id = range_match(cfg.uid_range);
    let mut rules = Vec::new();
    // Per-uid egress byte counters, read back for monthly usage. First and
    // verdict-free: almost every byte an app sends belongs to an established
    // connection, so counters below the `established` accept would only ever
    // see each connection's opening packet. Loopback is excluded because
    // that's the app answering outer's proxy, i.e. serving visitors, which
    // the proxy meters as requests rather than as outbound traffic.
    for uid in counted_uids(cfg) {
        rules.push(format!(
            "    meta skuid {uid} oifname != \"lo\" counter name \"app_{uid}\""
        ));
    }
    // Replies next: these rules only restrict connections an app *opens*.
    // Without this, a fleet's answers to outer's proxy (over loopback) match
    // the rejects below, and no app is reachable at all.
    rules.push("    ct state established,related accept".to_string());
    // DNS is matched on the port the socket dialed (conntrack's original
    // tuple), not the packet's: Docker's embedded resolver answers on
    // 127.0.0.11:53 through a DNAT to a random high port, and that NAT (output
    // hook, priority -100) runs before this chain, so a plain `dport 53`
    // never matched and the loopback reject below refused every lookup.
    rules.push(format!(
        "    meta skuid {id} meta l4proto {{ tcp, udp }} ct original proto-dst 53 accept"
    ));
    // The app's own celld needs the object store, which may sit on a private
    // address: allow exactly those addresses and port, ahead of the
    // private-range rejects.
    let (v4, v6): (Vec<IpAddr>, Vec<IpAddr>) =
        cfg.storage_addrs.iter().copied().partition(IpAddr::is_ipv4);
    let join = |ips: &[IpAddr]| {
        ips.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !v4.is_empty() {
        rules.push(format!(
            "    meta skuid {id} ip daddr {{ {} }} tcp dport {} accept",
            join(&v4),
            cfg.storage_port
        ));
    }
    if !v6.is_empty() {
        rules.push(format!(
            "    meta skuid {id} ip6 daddr {{ {} }} tcp dport {} accept",
            join(&v6),
            cfg.storage_port
        ));
    }
    // No mail submission: apps send email through an HTTP API.
    rules.push(format!(
        "    meta skuid {id} tcp dport {{ 25, 465, 587 }} reject"
    ));
    // No new connection to loopback, any protocol: that is where outer, its
    // proxy and every other fleet listen.
    rules.push(format!("    meta skuid {id} ip daddr 127.0.0.0/8 reject"));
    rules.push(format!("    meta skuid {id} ip6 daddr ::1 reject"));
    rules.push(format!(
        "    meta skuid {id} ip daddr {{ 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10 }} reject"
    ));
    rules.push(format!(
        "    meta skuid {id} ip6 daddr {{ fc00::/7, fe80::/10 }} reject"
    ));
    // No per-uid connection rate limit: a `drop` here stalled `bun install`
    // (hundreds of parallel registry connections) until the build timed
    // out, live on 2026-10-27, and the owner's plan table has no egress or
    // connection caps. Every verdict for app uids is a fast `reject`.
    let mut counters = String::new();
    for uid in counted_uids(cfg) {
        counters.push_str(&format!("  counter app_{uid} {{\n  }}\n"));
    }
    format!(
        r#"table inet outer
delete table inet outer
table inet outer {{
{counters}  chain output {{
    type filter hook output priority 0; policy accept;
{}
  }}
}}"#,
        rules.join("\n")
    )
}

/// Parse `nft -j list counters` output into uid -> egress bytes. The JSON
/// shape varies by nft version, so this walks the whole value for `counter`
/// objects named `app_<uid>` instead of assuming a layout. Unparseable input
/// reads as empty.
pub fn read_counters(nft_json: &str) -> HashMap<u32, u64> {
    let mut out = HashMap::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(nft_json) else {
        return out;
    };
    walk_counters(&value, &mut out);
    out
}

fn walk_counters(value: &serde_json::Value, out: &mut HashMap<u32, u64>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                walk_counters(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Object(counter)) = map.get("counter") {
                if let (
                    Some(serde_json::Value::String(name)),
                    Some(serde_json::Value::Number(bytes)),
                ) = (counter.get("name"), counter.get("bytes"))
                {
                    if let (Some(uid), Some(count)) = (
                        name.strip_prefix("app_")
                            .and_then(|uid| uid.parse::<u32>().ok()),
                        bytes.as_u64(),
                    ) {
                        *out.entry(uid).or_insert(0) += count;
                    }
                }
            }
            for item in map.values() {
                walk_counters(item, out);
            }
        }
        _ => {}
    }
}

/// Install the table (idempotent: the ruleset replaces it atomically).
/// Best-effort: returns an error when `nft` is missing or CAP_NET_ADMIN is
/// absent, and the caller runs without isolation loudly.
pub async fn apply(rules: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("nft")
        .args(["-f", "-"])
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn nft: {e}"))?;
    // `nft -f -` reads the ruleset from stdin.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(rules.as_bytes())
            .await
            .map_err(|_| anyhow::anyhow!("nft stdin write failed"))?;
    }
    match tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => {
            tracing::info!("nft egress policy installed");
            Ok(())
        }
        Ok(Ok(out)) => Err(anyhow::anyhow!(
            "nft apply failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(anyhow::anyhow!("nft timed out")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> NftConfig {
        NftConfig {
            uid_range: (200000, 100000),
            storage_addrs: vec!["10.89.0.7".parse().unwrap()],
            storage_port: 443,
            free_uids: vec![200001],
            pro_uids: vec![200002],
        }
    }

    #[test]
    fn dns_and_storage_are_accepted_before_any_reject() {
        let rules = ruleset(&test_config());
        let first_reject = rules.find(" reject").expect("has rejects");
        let range = "200000-299999";
        // Pre-NAT port: Docker DNATs its resolver off port 53.
        let dns = rules
            .find(&format!(
                "meta skuid {range} meta l4proto {{ tcp, udp }} ct original proto-dst 53 accept"
            ))
            .expect("dns accept present");
        assert!(dns < first_reject, "dns must come first");
        let store = rules
            .find(&format!(
                "meta skuid {range} ip daddr {{ 10.89.0.7 }} tcp dport 443 accept"
            ))
            .expect("storage allowance");
        assert!(
            store < first_reject,
            "storage must be allowed before the rejects"
        );
        let replies = rules
            .find("ct state established,related accept")
            .expect("reply rule");
        assert!(
            replies < first_reject,
            "established/related must come first"
        );
        // Atomic replace: the file recreates the table in one transaction.
        assert!(rules.starts_with("table inet outer\ndelete table inet outer\n"));
    }

    #[test]
    fn whole_range_is_closed_off_in_every_rule() {
        let rules = ruleset(&test_config());
        let range = "200000-299999";
        for prefix in [
            format!("meta skuid {range} ip daddr 127.0.0.0/8 reject"),
            format!("meta skuid {range} ip6 daddr ::1 reject"),
            format!(
                "meta skuid {range} ip daddr {{ 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10 }} reject"
            ),
            format!(
                "meta skuid {range} ip6 daddr {{ fc00::/7, fe80::/10 }} reject"
            ),
            format!("meta skuid {range} tcp dport {{ 25, 465, 587 }} reject"),
        ] {
            assert!(rules.contains(&prefix), "missing: {prefix}");
        }
    }

    #[test]
    fn smtp_is_rejected_before_anything_private() {
        let rules = ruleset(&test_config());
        let smtp = rules.find("dport { 25, 465, 587 }").expect("smtp reject");
        let loopback = rules.find("127.0.0.0/8").expect("loopback reject");
        assert!(smtp < loopback);
    }

    /// A silently dropped SYN hangs the client until its own timeout (a
    /// `bun install` build died that way); app traffic is only ever
    /// rejected, so a refusal is immediate and visible.
    #[test]
    fn app_traffic_is_never_silently_dropped() {
        let rules = ruleset(&test_config());
        for line in rules.lines().filter(|l| l.contains("skuid")) {
            assert!(!line.trim_end().ends_with("drop"), "drop verdict: {line}");
            assert!(!line.contains("limit rate"), "rate limit: {line}");
        }
    }

    #[test]
    fn per_uid_counters_are_declared_and_counted_on_egress() {
        let rules = ruleset(&test_config());
        for uid in [200001, 200002] {
            assert!(
                rules.contains(&format!("counter app_{uid} {{")),
                "counter declaration for {uid}"
            );
            assert!(
                rules.contains(&format!(
                    "meta skuid {uid} oifname != \"lo\" counter name \"app_{uid}\""
                )),
                "counter rule for {uid}"
            );
        }
        // Counters come before the established accept, or they would only see
        // each connection's first packet.
        let first_counter = rules.find("counter name").expect("has counters");
        let replies = rules
            .find("ct state established,related accept")
            .expect("reply rule");
        assert!(first_counter < replies);
    }

    #[test]
    fn counters_parse_from_nft_json() {
        let fixture = r#"{"nftables": [
            {"metainfo": {"version": "1.0.9"}},
            {"counter": {"family": "inet", "table": "outer", "name": "app_200001", "handle": 3, "packets": 12, "bytes": 3456}},
            {"counter": {"family": "inet", "table": "outer", "name": "app_200002", "handle": 4, "packets": 0, "bytes": 0}},
            {"counter": {"family": "inet", "table": "outer", "name": "other", "handle": 5, "packets": 9, "bytes": 99}}
        ]}"#;
        let counters = read_counters(fixture);
        assert_eq!(counters.get(&200001), Some(&3456));
        assert_eq!(counters.get(&200002), Some(&0));
        assert_eq!(counters.len(), 2, "only app_<uid> counters count");
        // Garbage in: empty out, never an error.
        assert!(read_counters("not json").is_empty());
    }

    #[tokio::test]
    #[ignore = "needs CAP_NET_ADMIN and nft to install the table"]
    async fn installs_the_generated_ruleset() {
        apply(&ruleset(&test_config())).await.unwrap();
    }
}
