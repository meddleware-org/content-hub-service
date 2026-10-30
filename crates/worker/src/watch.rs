//! Wallet watch (the hub's wallets are float-only, ADR-0007): polls a Sui GraphQL endpoint for the balances of a few
//! public addresses and exposes them on the worker's `/metrics`, so the monitoring stack can warn **before** a depleted
//! float makes the daily snapshot export fail.
//!
//! One background task (not one query per scrape: a scrape every 15 s would hit the public endpoint 5,760 times a day
//! for a balance that changes a few times a month). The numbers are the readings of the last successful poll, with the
//! time of the last attempt and a failure counter so a dead endpoint is itself visible (a stale reading is never
//! presented as fresh). Only public addresses are configured; nothing here holds a key.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use content_hub_core::ident::is_sui_address;
use content_hub_publish::{Http, Request};
use serde_json::{Value, json};

/// The coins that are always reported (zero when the wallet holds none), by the type's last path segment.
const COINS: [&str; 2] = ["SUI", "WAL"];

/// What to watch and how often.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// A Sui GraphQL endpoint (https, e.g. `https://graphql.testnet.sui.io/graphql`).
    pub graphql_url: String,
    pub wallets: Vec<Wallet>,
    pub every: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wallet {
    /// A short label for the metric (`publisher`).
    pub label: String,
    pub address: String,
}

/// Parse `label=0xaddress,label=0xaddress` (what `WATCH_WALLETS` holds). Labels are `[a-z][a-z0-9_-]{0,31}`, addresses
/// `0x` and 64 hex digits, each label and address at most once; at most ten wallets.
pub fn parse_wallets(text: &str) -> Result<Vec<Wallet>, String> {
    let mut out: Vec<Wallet> = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (label, address) = part.split_once('=').ok_or_else(|| format!("'{part}' is not label=0xaddress"))?;
        let label_ok = !label.is_empty()
            && label.len() <= 32
            && label.starts_with(|c: char| c.is_ascii_lowercase())
            && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !label_ok {
            return Err(format!("label '{label}' must be lower-case letters, digits, '_' or '-' (at most 32)"));
        }
        if !is_sui_address(address) {
            return Err(format!("'{address}' is not a Sui address (0x and 64 hex digits)"));
        }
        if out.iter().any(|w| w.label == label || w.address == address) {
            return Err(format!("'{label}' or its address is listed twice"));
        }
        out.push(Wallet { label: label.to_owned(), address: address.to_ascii_lowercase() });
    }
    if out.len() > 10 {
        return Err("at most ten wallets".into());
    }
    Ok(out)
}

/// The last readings of one wallet.
#[derive(Debug, Clone, Default)]
struct WalletState {
    /// Balance per coin name, in the coin's smallest unit (MIST, FROST), from the last successful poll.
    balances: BTreeMap<String, u128>,
    /// Unix seconds of the last successful poll (0: never).
    ok_at: i64,
    /// Unix seconds of the last attempt (0: never).
    tried_at: i64,
    failures: u64,
}

/// The shared readings: written by [`run`], read by `/metrics`.
#[derive(Debug, Default)]
pub struct Watch {
    state: RwLock<BTreeMap<String, (String, WalletState)>>,
}

impl Watch {
    pub fn new(wallets: &[Wallet]) -> Watch {
        let state = wallets.iter().map(|w| (w.label.clone(), (w.address.clone(), WalletState::default()))).collect();
        Watch { state: RwLock::new(state) }
    }

    fn record(&self, label: &str, result: Result<BTreeMap<String, u128>, String>, now: i64) {
        let Ok(mut g) = self.state.write() else { return };
        if let Some((_, s)) = g.get_mut(label) {
            s.tried_at = now;
            match result {
                Ok(b) => {
                    s.balances = b;
                    s.ok_at = now;
                }
                Err(e) => {
                    s.failures += 1;
                    tracing::warn!(wallet = label, error = %e, "wallet balance check failed");
                }
            }
        }
    }

    /// The exposition text of the readings (empty when nothing is watched).
    pub fn render(&self) -> String {
        let Ok(g) = self.state.read() else { return String::new() };
        if g.is_empty() {
            return String::new();
        }
        let mut o = String::new();
        let _ = writeln!(
            o,
            "# HELP content_hub_wallet_balance Balance of a watched wallet in the coin's smallest unit (MIST for SUI, FROST for WAL), from the last successful check.\n# TYPE content_hub_wallet_balance gauge"
        );
        for (label, (address, s)) in g.iter() {
            for coin in COINS {
                let v = s.balances.get(coin).copied().unwrap_or(0);
                let _ = writeln!(
                    o,
                    "content_hub_wallet_balance{{wallet=\"{label}\",address=\"{address}\",coin=\"{coin}\"}} {v}"
                );
            }
        }
        let _ = writeln!(
            o,
            "# HELP content_hub_wallet_checked_timestamp_seconds Unix time of the last SUCCESSFUL balance check (0: never).\n# TYPE content_hub_wallet_checked_timestamp_seconds gauge"
        );
        for (label, (_, s)) in g.iter() {
            let _ = writeln!(o, "content_hub_wallet_checked_timestamp_seconds{{wallet=\"{label}\"}} {}", s.ok_at);
        }
        let _ = writeln!(
            o,
            "# HELP content_hub_wallet_check_failures_total Balance checks that failed since this process started.\n# TYPE content_hub_wallet_check_failures_total counter"
        );
        for (label, (_, s)) in g.iter() {
            let _ = writeln!(o, "content_hub_wallet_check_failures_total{{wallet=\"{label}\"}} {}", s.failures);
        }
        o
    }
}

/// The GraphQL request for one address's balances.
fn query(address: &str) -> Value {
    json!({
        "query": "query($a: SuiAddress!) { address(address: $a) { balances { nodes { coinType { repr } totalBalance } } } }",
        "variables": { "a": address }
    })
}

/// The balances in a GraphQL reply, by coin name (the last `::` segment of the coin type).
pub fn parse_balances(body: &str) -> Result<BTreeMap<String, u128>, String> {
    let v: Value = serde_json::from_str(body).map_err(|_| "the reply is not JSON".to_owned())?;
    if let Some(e) = v.get("errors").and_then(Value::as_array).and_then(|a| a.first()) {
        let m = e.get("message").and_then(Value::as_str).unwrap_or("an error");
        return Err(format!("GraphQL error: {}", m.chars().take(120).collect::<String>()));
    }
    // A known address that has never held a coin may be answered with `address: null`: that is a zero balance, not a failure.
    let address =
        v.get("data").and_then(|d| d.get("address")).ok_or_else(|| "the reply has no data.address".to_owned())?;
    let mut out = BTreeMap::new();
    if address.is_null() {
        return Ok(out);
    }
    let nodes = address
        .pointer("/balances/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "the reply has no balances".to_owned())?;
    for n in nodes {
        let ty = n
            .pointer("/coinType/repr")
            .and_then(Value::as_str)
            .ok_or_else(|| "a balance has no coin type".to_owned())?;
        let total = n
            .get("totalBalance")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u128>().ok())
            .ok_or_else(|| "a balance is not a number".to_owned())?;
        let name = ty.rsplit("::").next().unwrap_or(ty);
        *out.entry(name.to_owned()).or_insert(0) += total;
    }
    Ok(out)
}

/// Check every wallet once and record the results.
pub async fn poll_once(http: &Http, cfg: &WatchConfig, watch: &Watch) {
    for w in &cfg.wallets {
        let result = match http.send(Request::post(cfg.graphql_url.clone(), query(&w.address))).await {
            Ok(r) if r.status == 200 => parse_balances(&r.body),
            Ok(r) => Err(format!("HTTP {}", r.status)),
            Err(f) => Err(format!("{f:?}")),
        };
        watch.record(&w.label, result, Utc::now().timestamp());
    }
}

/// Poll until `stop` turns true (the first poll is immediate).
pub async fn run(http: Http, cfg: WatchConfig, watch: Arc<Watch>, mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        poll_once(&http, &cfg, &watch).await;
        tokio::select! {
            () = tokio::time::sleep(cfg.every) => {}
            _ = stop.changed() => {}
        }
        if *stop.borrow() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::post;

    const A: &str = "0x664266bc48766e8b8eb1701f5d646895e27c09d184ab0cc678c492dd82b42fe5";
    const B: &str = "0x0000000000000000000000000000000000000000000000000000000000000002";

    #[test]
    fn wallets_are_parsed_strictly() {
        let ok = parse_wallets(&format!("publisher={A}, relay={B}")).expect("parses");
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0], Wallet { label: "publisher".into(), address: A.into() });
        assert_eq!(parse_wallets("").expect("empty is fine"), vec![]);
        for bad in [
            "publisher".to_owned(),
            format!("Publisher={A}"),
            format!("1pub={A}"),
            "publisher=0x12".to_owned(),
            format!("a={A},a={B}"),
            format!("a={A},b={A}"),
            format!("a b={A}"),
        ] {
            assert!(parse_wallets(&bad).is_err(), "{bad}");
        }
        let many: Vec<String> = (0..11).map(|i| format!("w{i}=0x{i:064x}")).collect();
        assert!(parse_wallets(&many.join(",")).is_err(), "more than ten");
    }

    #[test]
    fn balances_are_read_by_coin_name() {
        let body = r#"{"data":{"address":{"balances":{"nodes":[
            {"coinType":{"repr":"0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI"},"totalBalance":"1782817884"},
            {"coinType":{"repr":"0x8270::wal::WAL"},"totalBalance":"800000000"}]}}}}"#;
        let b = parse_balances(body).expect("parses");
        assert_eq!(b.get("SUI"), Some(&1_782_817_884));
        assert_eq!(b.get("WAL"), Some(&800_000_000));
        assert_eq!(parse_balances(r#"{"data":{"address":null}}"#).expect("null address is zero"), BTreeMap::new());
        for bad in [
            "nope",
            r#"{"errors":[{"message":"rate limited"}]}"#,
            r#"{"data":{}}"#,
            r#"{"data":{"address":{"balances":{"nodes":[{"coinType":{"repr":"x::y::Z"},"totalBalance":"-1"}]}}}}"#,
        ] {
            assert!(parse_balances(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn missing_coins_read_as_zero_and_a_failed_check_is_visible() {
        let w = Watch::new(&[Wallet { label: "publisher".into(), address: A.into() }]);
        let text = w.render();
        assert!(
            text.contains(&format!(
                "content_hub_wallet_balance{{wallet=\"publisher\",address=\"{A}\",coin=\"WAL\"}} 0"
            ))
        );
        assert!(text.contains("content_hub_wallet_checked_timestamp_seconds{wallet=\"publisher\"} 0"));
        w.record("publisher", Ok(BTreeMap::from([("SUI".to_owned(), 5)])), 100);
        w.record("publisher", Err("down".into()), 200);
        let text = w.render();
        // The reading of the last SUCCESSFUL check stays; its age and the failure count say it is stale.
        assert!(text.contains("coin=\"SUI\"} 5"));
        assert!(text.contains("content_hub_wallet_checked_timestamp_seconds{wallet=\"publisher\"} 100"));
        assert!(text.contains("content_hub_wallet_check_failures_total{wallet=\"publisher\"} 1"));
        assert!(Watch::new(&[]).render().is_empty());
    }

    async fn graphql(reply: &'static str, status: u16) -> String {
        let app = Router::new().route(
            "/graphql",
            post(move |axum::Json(q): axum::Json<Value>| async move {
                // The request names the address as a variable (never spliced into the query text).
                assert!(q["variables"]["a"].as_str().is_some_and(|a| a.starts_with("0x")));
                assert!(!q["query"].as_str().unwrap_or_default().contains("0x"));
                (axum::http::StatusCode::from_u16(status).expect("status"), reply)
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = l.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(l, app).await.expect("serve") });
        format!("http://{addr}/graphql")
    }

    #[tokio::test]
    async fn a_poll_records_balances_and_a_bad_reply_counts_as_a_failure() {
        let http = Http::unchecked_for_tests(Duration::from_secs(5));
        let good = graphql(
            r#"{"data":{"address":{"balances":{"nodes":[{"coinType":{"repr":"0x2::sui::SUI"},"totalBalance":"42"}]}}}}"#,
            200,
        )
        .await;
        let wallets = vec![Wallet { label: "publisher".into(), address: A.into() }];
        let watch = Watch::new(&wallets);
        poll_once(
            &http,
            &WatchConfig { graphql_url: good, wallets: wallets.clone(), every: Duration::from_secs(60) },
            &watch,
        )
        .await;
        assert!(watch.render().contains("coin=\"SUI\"} 42"));

        let bad = graphql("oops", 503).await;
        poll_once(&http, &WatchConfig { graphql_url: bad, wallets, every: Duration::from_secs(60) }, &watch).await;
        let text = watch.render();
        assert!(text.contains("coin=\"SUI\"} 42"), "the last good reading stays");
        assert!(text.contains("content_hub_wallet_check_failures_total{wallet=\"publisher\"} 1"));
    }

    #[tokio::test]
    async fn the_production_client_calls_only_public_https() {
        // A configured URL on a private address is refused by the checked client: nothing is read, one failure is counted.
        let http = Http::new().expect("client");
        let wallets = vec![Wallet { label: "publisher".into(), address: A.into() }];
        let watch = Watch::new(&wallets);
        let cfg =
            WatchConfig { graphql_url: "https://10.0.0.1/graphql".into(), wallets, every: Duration::from_secs(60) };
        poll_once(&http, &cfg, &watch).await;
        assert!(watch.render().contains("content_hub_wallet_check_failures_total{wallet=\"publisher\"} 1"));
    }
}
