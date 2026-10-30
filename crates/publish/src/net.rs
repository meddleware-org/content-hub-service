//! The publishers' HTTP client (ARCHITECTURE.md §11, SSRF):
//!
//! - **https only**; redirects are never followed (a 3xx is an answer, classified like any other);
//! - **public addresses only**: names are resolved here and every address that is not public is dropped before
//!   connecting (no name, or only private answers → refused), so the client connects only to addresses it checked;
//!   an IP-literal URL is checked directly;
//! - system proxy variables are ignored (a proxy would bypass the address check; egress is the namespace's policy);
//! - a connect timeout (5 s) and an overall timeout (30 s); replies are read up to 64 KiB.
//!
//! A second mode, [`Http::internal`], is for the one in-cluster service the hub is configured to call (the Walrus
//! publisher): plain http and private addresses are allowed, but **only** for that exact host name. It still follows no
//! redirects and ignores proxies; every other host is refused before any connection.
//!
//! Errors are classified for the at-most-once rules: a connection that was never made means nothing was sent
//! ([`Failure::NotSent`]); a timeout or an error after the request may have been written is [`Failure::Unknown`];
//! a refused address or an unusable request is [`Failure::Refused`].

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use content_hub_core::delivery::Sent;
use content_hub_core::net::{is_public_ip, is_public_name};
use content_hub_core::platform::Reply;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::Value;

/// The most of a reply that is read.
pub const MAX_REPLY: usize = 64 * 1024;

/// Install ring as rustls' process-wide crypto provider (once; later calls do nothing).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Why a request produced no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The connection was never made: nothing was sent.
    NotSent(String),
    /// The request may have reached the platform (a timeout or an error after writing it).
    Unknown(String),
    /// Refused before sending: a non-public address, an unusable URL or header.
    Refused(String),
}

impl Failure {
    /// The send outcome when this failure happened on the step that posts.
    pub fn to_sent(&self) -> Sent {
        match self {
            Failure::NotSent(r) => Sent::Retry { reason: r.clone(), after: None, rate_limited: false },
            Failure::Unknown(r) => Sent::Unknown { reason: r.clone() },
            Failure::Refused(r) => Sent::Refused { reason: r.clone(), validation: false },
        }
    }

    /// The outcome when this failure happened on a step before anything is posted (a login): never `Unknown`.
    pub fn before_posting(&self) -> Sent {
        match self {
            Failure::Refused(r) => Sent::Refused { reason: r.clone(), validation: false },
            Failure::NotSent(r) | Failure::Unknown(r) => {
                Sent::Retry { reason: format!("before posting: {r}"), after: None, rate_limited: false }
            }
        }
    }
}

#[derive(Debug)]
struct Blocked(String);

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} resolves to no public address", self.0)
    }
}

impl std::error::Error for Blocked {}

/// Resolves with the system resolver and keeps only public addresses.
struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            if !is_public_name(&host) {
                return Err(Box::new(Blocked(host)) as Box<dyn std::error::Error + Send + Sync>);
            }
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.filter(|a| is_public_ip(a.ip())).collect();
            if addrs.is_empty() {
                return Err(Box::new(Blocked(host)) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Resolves only one configured host name (to whatever address it has: it is an internal service).
struct SingleHostResolver {
    host: String,
}

impl Resolve for SingleHostResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allowed = self.host.clone();
        let asked = name.as_str().to_owned();
        Box::pin(async move {
            if !asked.eq_ignore_ascii_case(&allowed) {
                return Err(Box::new(Blocked(asked)) as Box<dyn std::error::Error + Send + Sync>);
            }
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((allowed.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// One request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: reqwest::Method,
    pub url: String,
    /// Header names and values (credentials included: never logged or stored).
    pub headers: Vec<(&'static str, String)>,
    pub json: Option<Value>,
    /// A raw body (a blob), sent as `application/octet-stream`.
    pub bytes: Option<Vec<u8>>,
}

impl Request {
    pub fn post(url: String, json: Value) -> Request {
        Request { method: reqwest::Method::POST, url, headers: Vec::new(), json: Some(json), bytes: None }
    }

    pub fn get(url: String) -> Request {
        Request { method: reqwest::Method::GET, url, headers: Vec::new(), json: None, bytes: None }
    }

    pub fn put_bytes(url: String, bytes: Vec<u8>) -> Request {
        Request { method: reqwest::Method::PUT, url, headers: Vec::new(), json: None, bytes: Some(bytes) }
    }

    pub fn header(mut self, name: &'static str, value: String) -> Request {
        self.headers.push((name, value));
        self
    }
}

#[derive(Clone)]
enum Mode {
    /// https only, public addresses only.
    Public,
    /// Exactly this host, any scheme http(s), any address.
    Internal(String),
    /// Tests: anything.
    Unchecked,
}

/// The publishers' client.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    mode: Mode,
}

impl Http {
    /// The production client (the rules in the module documentation).
    pub fn new() -> Result<Http, String> {
        Self::build(Mode::Public, Duration::from_secs(30))
    }

    /// A client for the one in-cluster service the hub calls (the Walrus publisher): only `host`, over http or https,
    /// at whatever address it has. See the module documentation.
    pub fn internal(host: &str) -> Result<Http, String> {
        if host.is_empty() || host.contains(['/', ':', '@', ' ']) {
            return Err("the internal host must be a bare host name".into());
        }
        Self::build(Mode::Internal(host.to_ascii_lowercase()), Duration::from_secs(60))
    }

    /// A client for tests against local mock servers: plain http and loopback addresses are allowed. Nothing in the
    /// configuration can select it.
    #[doc(hidden)]
    pub fn unchecked_for_tests(timeout: Duration) -> Http {
        Self::build(Mode::Unchecked, timeout).expect("the test client builds")
    }

    fn build(mode: Mode, timeout: Duration) -> Result<Http, String> {
        install_crypto_provider();
        let mut b = reqwest::Client::builder()
            .user_agent(concat!(
                "content-hub/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/meddleware-org/content-hub-service)"
            ))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5).min(timeout))
            .timeout(timeout);
        match &mode {
            Mode::Public => b = b.https_only(true).dns_resolver(Arc::new(PublicResolver)),
            Mode::Internal(host) => b = b.dns_resolver(Arc::new(SingleHostResolver { host: host.clone() })),
            Mode::Unchecked => {}
        }
        Ok(Http { client: b.build().map_err(|_| "cannot build the HTTP client".to_owned())?, mode })
    }

    /// Send one request and read the reply (up to [`MAX_REPLY`]).
    pub async fn send(&self, r: Request) -> Result<Reply, Failure> {
        let url = url::Url::parse(&r.url).map_err(|_| Failure::Refused("not a valid URL".into()))?;
        if let Mode::Internal(host) = &self.mode {
            let same = matches!(url.host(), Some(url::Host::Domain(d)) if d.eq_ignore_ascii_case(host));
            if !same || !matches!(url.scheme(), "http" | "https") {
                return Err(Failure::Refused("this client calls only its configured internal host".into()));
            }
        }
        if matches!(self.mode, Mode::Public) {
            if url.scheme() != "https" {
                return Err(Failure::Refused("only https URLs are called".into()));
            }
            let literal: Option<IpAddr> = match url.host() {
                Some(url::Host::Ipv4(a)) => Some(a.into()),
                Some(url::Host::Ipv6(a)) => Some(a.into()),
                _ => None,
            };
            if literal.is_some_and(|ip| !is_public_ip(ip)) {
                return Err(Failure::Refused("the URL names a non-public address".into()));
            }
        }
        let mut req = self.client.request(r.method, url);
        for (k, v) in &r.headers {
            req = req.header(*k, v);
        }
        if let Some(j) = &r.json {
            req = req.json(j);
        }
        if let Some(b) = r.bytes {
            req = req.header("content-type", "application/octet-stream").body(b);
        }
        let mut resp = req.send().await.map_err(classify)?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_ascii_lowercase(), v.to_owned())))
            .collect();
        // The status is the answer; a body that cannot be read in full is kept as far as it was read.
        let mut body = Vec::new();
        while body.len() < MAX_REPLY {
            match resp.chunk().await {
                Ok(Some(c)) => body.extend_from_slice(&c[..c.len().min(MAX_REPLY - body.len())]),
                Ok(None) | Err(_) => break,
            }
        }
        Ok(Reply { status, headers, body: String::from_utf8_lossy(&body).into_owned() })
    }
}

fn classify(e: reqwest::Error) -> Failure {
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        if let Some(b) = s.downcast_ref::<Blocked>() {
            return Failure::Refused(b.to_string());
        }
        source = s.source();
    }
    if e.is_builder() {
        Failure::Refused("the request could not be built".into())
    } else if e.is_connect() {
        Failure::NotSent("the connection could not be made".into())
    } else if e.is_timeout() {
        Failure::Unknown("no answer before the timeout, after the request was sent".into())
    } else {
        Failure::Unknown("the exchange failed after the connection was made".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_production_client_refuses_private_and_plain_targets() {
        let http = Http::new().expect("client");
        for (url, want) in [
            ("http://example.org/", "only https"),
            ("https://127.0.0.1/", "non-public"),
            ("https://[::1]/", "non-public"),
            ("https://10.1.2.3/x", "non-public"),
        ] {
            match http.send(Request::get(url.into())).await {
                Err(Failure::Refused(m)) => assert!(m.contains(want), "{url}: {m}"),
                other => panic!("{url}: {other:?}"),
            }
        }
        // A name that can only resolve inside a network is refused by the resolver, before any connection.
        match http.send(Request::get("https://localhost/".into())).await {
            Err(Failure::Refused(m)) => assert!(m.contains("no public address"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn failures_map_to_outcomes() {
        assert!(matches!(Failure::NotSent("x".into()).to_sent(), Sent::Retry { .. }));
        assert!(matches!(Failure::Unknown("x".into()).to_sent(), Sent::Unknown { .. }));
        assert!(matches!(Failure::Unknown("x".into()).before_posting(), Sent::Retry { .. }));
        assert!(matches!(Failure::Refused("x".into()).before_posting(), Sent::Refused { .. }));
    }
}
