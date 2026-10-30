//! The public process's router. It holds a pool of the READ-ONLY role (which can read only the
//! `public.public_v1_*` views) and serves only [`PUBLIC_ROUTES`] and the probes. Every response is cacheable: a
//! strong `ETag` (SHA-256 of the body), `If-None-Match` → 304, and `Cache-Control` for browsers and the edge.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use content_hub_store::repo::public::{self as repo, Audience, FeedEntry, PublicPhase, PublicSnapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::error::ApiError;
use crate::{not_found, openapi, probe};

/// Operator configuration of the public surfaces (environment; never in the repository).
#[derive(Debug, Clone)]
pub struct PublicConfig {
    /// The feeds' title, e.g. "Example updates".
    pub feed_title: String,
    /// The human site the feeds point to (the roadmap site), e.g. `https://roadmap.example.org`.
    pub home_page_url: String,
    /// This machine host's origin, e.g. `https://feeds.example.dev` (the feeds' self links).
    pub feeds_base_url: String,
    /// `/llms.txt` of the machine host (a generic text when unset).
    pub llms_txt: Option<String>,
    /// `/robots.txt` of the machine host (a generic text when unset).
    pub robots_txt: Option<String>,
    /// A public Walrus aggregator (https), so the snapshot route can link the blob, e.g. `https://aggregator.example.org`.
    pub aggregator_url: Option<String>,
}

impl Default for PublicConfig {
    fn default() -> Self {
        PublicConfig {
            feed_title: "Updates".into(),
            home_page_url: "https://example.org".into(),
            feeds_base_url: "https://feeds.example.org".into(),
            llms_txt: None,
            robots_txt: None,
            aggregator_url: None,
        }
    }
}

/// The public process's state: nothing that can write or authenticate.
#[derive(Clone)]
pub struct PublicState {
    /// A pool of the read-only role (`Access::ReadOnly`).
    pub db: PgPool,
    pub config: std::sync::Arc<PublicConfig>,
}

const FEED_LIMIT: i64 = 50;
const SHORT: &str = "public, max-age=60, s-maxage=300";
const LONG: &str = "public, max-age=3600, s-maxage=3600";

fn cached(headers: &HeaderMap, content_type: &'static str, cache: &'static str, body: Vec<u8>) -> Response {
    let etag = format!("\"{}\"", Sha256::digest(&body).iter().take(16).map(|b| format!("{b:02x}")).collect::<String>());
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    let mut resp = if matches { StatusCode::NOT_MODIFIED.into_response() } else { body.into_response() };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    resp
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Roadmap {
    /// The response schema version (bumped only on a breaking change).
    pub schema_version: u32,
    /// The newest change to anything shown.
    pub updated_at: Option<DateTime<Utc>>,
    pub phases: Vec<PublicPhase>,
}

/// The roadmap document for an audience, exactly as the public routes serve it. The admin console's preview calls
/// this too, so what an operator sees is what the public process returns.
pub async fn build_roadmap(c: &mut sqlx::PgConnection, audience: Audience) -> Result<Roadmap, ApiError> {
    let phases = repo::roadmap(c, audience).await?;
    let updated_at = phases.iter().flat_map(|p| p.items.iter().map(|i| i.updated_at)).max();
    Ok(Roadmap { schema_version: 1, updated_at, phases })
}

async fn roadmap_for(s: &PublicState, headers: &HeaderMap, audience: Audience) -> Result<Response, ApiError> {
    let mut c = s.db.acquire().await?;
    let body = serde_json::to_vec(&build_roadmap(&mut c, audience).await?).map_err(|_| ApiError::Internal)?;
    Ok(cached(headers, "application/json", SHORT, body))
}

async fn roadmap(State(s): State<PublicState>, headers: HeaderMap) -> Response {
    roadmap_for(&s, &headers, Audience::Human).await.unwrap_or_else(IntoResponse::into_response)
}

async fn roadmap_bot(State(s): State<PublicState>, headers: HeaderMap) -> Response {
    roadmap_for(&s, &headers, Audience::Bot).await.unwrap_or_else(IntoResponse::into_response)
}

/// The latest signed snapshot of the roadmap (ADR-0007): where its bytes are on Walrus and how to check them.
#[derive(Serialize, utoipa::ToSchema)]
pub struct SnapshotResponse {
    /// The snapshot document's `schema` value.
    pub schema: &'static str,
    /// `null` until the first snapshot has been exported.
    pub snapshot: Option<PublicSnapshot>,
    /// The blob on a public Walrus aggregator, when one is configured and the snapshot is stored.
    pub blob_url: Option<String>,
    /// How to verify it, in words.
    pub verify: &'static str,
}

const VERIFY: &str = "Fetch the blob, check that its SHA-256 equals `sha256`, then verify `signature` over the blob's bytes as a Sui personal message (for example with @mysten/sui verifyPersonalMessageSignature, or `sui keytool`) and check that the signing address equals `signer_address`, the address published by the operator.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotQuery {
    /// `localnet`, `testnet` or `mainnet` (default: the newest of any).
    pub network: Option<String>,
}

async fn snapshot(State(s): State<PublicState>, headers: HeaderMap, Query(q): Query<SnapshotQuery>) -> Response {
    if q.network.as_deref().is_some_and(|n| !matches!(n, "localnet" | "testnet" | "mainnet")) {
        return ApiError::Invalid("network must be localnet, testnet or mainnet".into()).into_response();
    }
    let result = async {
        let mut c = s.db.acquire().await?;
        Ok::<_, ApiError>(repo::latest_snapshot(&mut c, q.network.as_deref()).await?)
    }
    .await;
    match result {
        Err(e) => e.into_response(),
        Ok(snap) => {
            let blob_url = snap.as_ref().and_then(|x| x.blob_id.as_ref()).and_then(|id| {
                s.config.aggregator_url.as_ref().map(|a| format!("{}/v1/blobs/{id}", a.trim_end_matches('/')))
            });
            let body = SnapshotResponse {
                schema: content_hub_core::snapshot::SCHEMA,
                snapshot: snap,
                blob_url,
                verify: VERIFY,
            };
            match serde_json::to_vec(&body) {
                Ok(b) => cached(&headers, "application/json", SHORT, b),
                Err(_) => ApiError::Internal.into_response(),
            }
        }
    }
}

/// JSON Feed 1.1 (<https://jsonfeed.org/version/1.1>).
pub fn json_feed(cfg: &PublicConfig, entries: &[FeedEntry]) -> serde_json::Value {
    serde_json::json!({
        "version": "https://jsonfeed.org/version/1.1",
        "title": cfg.feed_title,
        "home_page_url": cfg.home_page_url,
        "feed_url": format!("{}/public/v1/feed.json", cfg.feeds_base_url.trim_end_matches('/')),
        "items": entries.iter().map(|e| serde_json::json!({
            "id": format!("urn:uuid:{}", e.id),
            "title": e.title,
            "content_text": e.body_markdown,
            "date_published": e.published_at.to_rfc3339(),
            "date_modified": e.updated_at.to_rfc3339(),
        })).collect::<Vec<_>>(),
    })
}

fn xml(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            // Characters XML 1.0 forbids are dropped.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => o.push(c),
        }
    }
    o
}

/// RSS 2.0 (chosen over Atom: the format most bots, automation tools and Discord feed bots read).
pub fn rss(cfg: &PublicConfig, entries: &[FeedEntry]) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\"><channel>");
    out.push_str(&format!(
        "<title>{}</title><link>{}</link><description>{}</description>",
        xml(&cfg.feed_title),
        xml(&cfg.home_page_url),
        xml(&cfg.feed_title)
    ));
    if let Some(newest) = entries.first() {
        out.push_str(&format!("<lastBuildDate>{}</lastBuildDate>", newest.published_at.to_rfc2822()));
    }
    for e in entries {
        out.push_str(&format!(
            "<item><title>{}</title><guid isPermaLink=\"false\">urn:uuid:{}</guid><pubDate>{}</pubDate><description>{}</description></item>",
            xml(&e.title),
            e.id,
            e.published_at.to_rfc2822(),
            xml(&e.body_markdown)
        ));
    }
    out.push_str("</channel></rss>\n");
    out
}

async fn feed_json(State(s): State<PublicState>, headers: HeaderMap) -> Response {
    let run = async {
        let mut c = s.db.acquire().await?;
        let entries = repo::feed(&mut c, FEED_LIMIT).await?;
        let body = serde_json::to_vec(&json_feed(&s.config, &entries)).map_err(|_| ApiError::Internal)?;
        Ok::<_, ApiError>(cached(&headers, "application/feed+json", SHORT, body))
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}

async fn feed_xml(State(s): State<PublicState>, headers: HeaderMap) -> Response {
    let run = async {
        let mut c = s.db.acquire().await?;
        let entries = repo::feed(&mut c, FEED_LIMIT).await?;
        Ok::<_, ApiError>(cached(
            &headers,
            "application/rss+xml; charset=utf-8",
            SHORT,
            rss(&s.config, &entries).into_bytes(),
        ))
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}

async fn public_openapi(headers: HeaderMap) -> Response {
    let body = serde_json::to_vec(&openapi::public()).unwrap_or_default();
    cached(&headers, "application/json", LONG, body)
}

async fn llms(State(s): State<PublicState>, headers: HeaderMap) -> Response {
    let text = s.config.llms_txt.clone().unwrap_or_else(|| {
        format!(
            "# {title}\n\n> Machine-readable roadmap and update feeds.\n\n- [Roadmap (JSON)]({base}/public/v1/roadmap-bot): phases and items, versioned schema\n- [Feed (JSON Feed 1.1)]({base}/public/v1/feed.json)\n- [Feed (RSS 2.0)]({base}/public/v1/feed.xml)\n- [OpenAPI]({base}/public/v1/openapi.json)\n",
            title = s.config.feed_title,
            base = s.config.feeds_base_url.trim_end_matches('/')
        )
    });
    cached(&headers, "text/plain; charset=utf-8", LONG, text.into_bytes())
}

async fn robots(State(s): State<PublicState>, headers: HeaderMap) -> Response {
    let text = s.config.robots_txt.clone().unwrap_or_else(|| "User-agent: *\nAllow: /\n".into());
    cached(&headers, "text/plain; charset=utf-8", LONG, text.into_bytes())
}

pub fn router(state: PublicState) -> Router {
    Router::new()
        .route("/healthz", get(probe::healthz))
        .route("/readyz", get(|State(s): State<PublicState>| async move { probe::readyz(&s.db).await }))
        .route("/public/v1/roadmap", get(roadmap))
        .route("/public/v1/roadmap-bot", get(roadmap_bot))
        .route("/public/v1/snapshot", get(snapshot))
        .route("/public/v1/feed.json", get(feed_json))
        .route("/public/v1/feed.xml", get(feed_xml))
        .route("/public/v1/openapi.json", get(public_openapi))
        .route("/llms.txt", get(llms))
        .route("/robots.txt", get(robots))
        .fallback(not_found)
        .layer(axum::extract::DefaultBodyLimit::max(16 << 10))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            std::time::Duration::from_secs(10),
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> FeedEntry {
        FeedEntry {
            id: uuid::Uuid::nil(),
            title: "A & B <c>".into(),
            body_markdown: "x\u{1}y".into(),
            published_at: DateTime::from_timestamp(1_800_000_000, 0).expect("time"),
            updated_at: DateTime::from_timestamp(1_800_000_100, 0).expect("time"),
        }
    }

    #[test]
    fn rss_escapes_and_drops_forbidden_characters() {
        let r = rss(&PublicConfig::default(), &[entry()]);
        assert!(r.contains("<title>A &amp; B &lt;c&gt;</title>"));
        assert!(r.contains("<description>xy</description>"));
        assert!(r.contains("urn:uuid:00000000-0000-0000-0000-000000000000"));
    }

    #[test]
    fn json_feed_is_version_1_1() {
        let f = json_feed(&PublicConfig::default(), &[entry()]);
        assert_eq!(f["version"], "https://jsonfeed.org/version/1.1");
        assert_eq!(f["items"][0]["content_text"], "x\u{1}y");
    }
}
