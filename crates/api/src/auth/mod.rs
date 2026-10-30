//! Admin authentication and authorisation (ARCHITECTURE.md §4), applied to every `/api/v1` request:
//!
//! 1. **CSRF** (state-changing methods): the `X-Content-Hub-Request` header must be present and `Origin` must equal
//!    the admin origin. A cross-site form or `fetch` without CORS cannot do both. → 403
//! 2. **Identity**: the `Cf-Access-Jwt-Assertion` header must verify (jwt.rs) against the team's JWKS, which is
//!    cached and refreshed when a token names an unknown key (at most every 30 s). Missing or invalid → 401.
//! 3. **Role**: Keto must say the subject is `ContentHub:admin#member`. Cached for 30 s when allowed and 5 s when
//!    denied. Not a member → 403. Keto unreachable → 503: it fails closed.
//! 4. The operator's row is refreshed (`users`), and the identity is attached to the request.
//!
//! A development mode (static JWKS file, static admin list) exists for local work; the configuration loader refuses
//! it in production (cli/main.rs).

pub mod jwt;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::error::ApiError;
use jwt::{JwkSet, JwtError};

/// The header Cloudflare Access adds to every proxied request.
pub const ACCESS_JWT_HEADER: &str = "cf-access-jwt-assertion";
/// The custom header state-changing requests must carry (`@meddleware/content-hub-client` `CSRF_HEADER`).
pub const CSRF_HEADER: &str = "x-content-hub-request";

const JWKS_MIN_REFRESH: Duration = Duration::from_secs(30);
const JWKS_MAX_AGE: Duration = Duration::from_secs(3600);
const ROLE_ALLOWED_TTL: Duration = Duration::from_secs(30);
const ROLE_DENIED_TTL: Duration = Duration::from_secs(5);
const MAX_JWKS_BYTES: usize = 64 * 1024;

/// Where the signing keys come from.
#[derive(Debug, Clone)]
pub enum JwksSource {
    /// `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`.
    Remote(String),
    /// Development only: a fixed set.
    Static(JwkSet),
}

/// Where roles come from.
#[derive(Debug, Clone)]
pub enum RoleSource {
    /// Keto's read API, e.g. `http://keto-read.auth.svc:4466`.
    Keto(String),
    /// Development only: these subjects are admins.
    Static(BTreeSet<String>),
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// The Access team domain, e.g. `https://team.cloudflareaccess.com` (the token's `iss`).
    pub issuer: String,
    /// The Access application's AUD tag.
    pub audience: String,
    pub jwks: JwksSource,
    pub roles: RoleSource,
    /// The admin host's origin, e.g. `https://hub.example.org`.
    pub admin_origin: String,
}

/// The authenticated operator, attached to the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub display_name: String,
}

pub struct Authenticator {
    cfg: AuthConfig,
    http: reqwest::Client,
    keys: RwLock<(JwkSet, Option<Instant>)>,
    last_refresh: Mutex<Option<Instant>>,
    roles: Mutex<HashMap<String, (bool, Instant)>>,
}

#[derive(Deserialize)]
struct KetoCheck {
    allowed: bool,
}

impl Authenticator {
    pub fn new(cfg: AuthConfig) -> Result<Arc<Self>, ApiError> {
        install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ApiError::Internal)?;
        let keys = match &cfg.jwks {
            JwksSource::Static(k) => (k.clone(), Some(Instant::now())),
            JwksSource::Remote(_) => (JwkSet { keys: Vec::new() }, None),
        };
        Ok(Arc::new(Authenticator {
            cfg,
            http,
            keys: RwLock::new(keys),
            last_refresh: Mutex::new(None),
            roles: Mutex::new(HashMap::new()),
        }))
    }

    pub fn admin_origin(&self) -> &str {
        &self.cfg.admin_origin
    }

    async fn refresh_keys(&self, force: bool) -> Result<(), ApiError> {
        let JwksSource::Remote(url) = &self.cfg.jwks else { return Ok(()) };
        {
            // At most one fetch per JWKS_MIN_REFRESH for unknown-key refreshes; a stale or empty set (force) is
            // always fetched, so a cold start or an expired set never waits.
            let mut last = self.last_refresh.lock().await;
            if !force && last.is_some_and(|t| t.elapsed() < JWKS_MIN_REFRESH) {
                return Ok(());
            }
            *last = Some(Instant::now());
        }
        let resp = self.http.get(url).send().await.map_err(|_| ApiError::Unavailable("identity keys unavailable"))?;
        if !resp.status().is_success() {
            return Err(ApiError::Unavailable("identity keys unavailable"));
        }
        let body = resp.bytes().await.map_err(|_| ApiError::Unavailable("identity keys unavailable"))?;
        if body.len() > MAX_JWKS_BYTES {
            return Err(ApiError::Unavailable("identity keys unavailable"));
        }
        let set: JwkSet =
            serde_json::from_slice(&body).map_err(|_| ApiError::Unavailable("identity keys unavailable"))?;
        *self.keys.write().await = (set, Some(Instant::now()));
        Ok(())
    }

    /// Verify the Access token and return the identity (without the role check).
    pub async fn identify(&self, token: &str) -> Result<Identity, ApiError> {
        let stale = self.keys.read().await.1.is_none_or(|t| t.elapsed() > JWKS_MAX_AGE);
        if stale {
            self.refresh_keys(true).await?;
        }
        let now = chrono::Utc::now().timestamp();
        let attempt = {
            let keys = self.keys.read().await;
            jwt::verify(token, &keys.0, &self.cfg.issuer, &self.cfg.audience, now)
        };
        let verified = match attempt {
            Err(JwtError::UnknownKey) if jwt::key_id(token).is_some() => {
                self.refresh_keys(false).await?;
                let keys = self.keys.read().await;
                jwt::verify(token, &keys.0, &self.cfg.issuer, &self.cfg.audience, now)
            }
            other => other,
        }
        .map_err(|e| {
            tracing::info!(reason = %e, "access token refused");
            ApiError::Unauthorized
        })?;
        Ok(Identity { subject: verified.subject, display_name: verified.display_name })
    }

    /// Whether the subject holds the admin role (cached briefly; fails closed).
    pub async fn is_admin(&self, subject: &str) -> Result<bool, ApiError> {
        if let Some((allowed, at)) = self.roles.lock().await.get(subject).copied() {
            let ttl = if allowed { ROLE_ALLOWED_TTL } else { ROLE_DENIED_TTL };
            if at.elapsed() < ttl {
                return Ok(allowed);
            }
        }
        let allowed = match &self.cfg.roles {
            RoleSource::Static(admins) => admins.contains(subject),
            RoleSource::Keto(base) => {
                let mut url = url::Url::parse(&format!("{}/relation-tuples/check/openapi", base.trim_end_matches('/')))
                    .map_err(|_| ApiError::Unavailable("authorisation service misconfigured"))?;
                url.query_pairs_mut()
                    .append_pair("namespace", "ContentHub")
                    .append_pair("object", "admin")
                    .append_pair("relation", "member")
                    .append_pair("subject_id", subject);
                let resp = self
                    .http
                    .get(url)
                    .send()
                    .await
                    .map_err(|_| ApiError::Unavailable("authorisation service unavailable"))?;
                if !resp.status().is_success() {
                    return Err(ApiError::Unavailable("authorisation service unavailable"));
                }
                resp.json::<KetoCheck>()
                    .await
                    .map_err(|_| ApiError::Unavailable("authorisation service unavailable"))?
                    .allowed
            }
        };
        self.roles.lock().await.insert(subject.to_owned(), (allowed, Instant::now()));
        Ok(allowed)
    }
}

/// Make ring the process's rustls provider (the one TLS stack of the service; idempotent).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The CSRF rule for one request.
pub fn csrf_ok(method: &Method, headers: &HeaderMap, admin_origin: &str) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    let marked = headers.contains_key(CSRF_HEADER);
    let origin_ok = headers.get("origin").and_then(|o| o.to_str().ok()).is_some_and(|o| o == admin_origin);
    marked && origin_ok
}

/// What the middleware needs from the admin state.
pub trait AuthState: Clone + Send + Sync + 'static {
    fn authenticator(&self) -> &Arc<Authenticator>;
    fn db(&self) -> &sqlx::PgPool;
}

/// The middleware for every `/api/v1` route.
pub async fn require_admin<S: AuthState>(State(state): State<S>, mut req: Request, next: Next) -> Response {
    let auth = state.authenticator().clone();
    if !csrf_ok(req.method(), req.headers(), auth.admin_origin()) {
        return ApiError::Forbidden("cross-site request refused").into_response();
    }
    let Some(token) = req.headers().get(ACCESS_JWT_HEADER).and_then(|v| v.to_str().ok()).map(str::to_owned) else {
        return ApiError::Unauthorized.into_response();
    };
    let identity = match auth.identify(&token).await {
        Ok(i) => i,
        Err(e) => return e.into_response(),
    };
    match auth.is_admin(&identity.subject).await {
        Ok(true) => {}
        Ok(false) => return ApiError::Forbidden("not an admin of the content hub").into_response(),
        Err(e) => return e.into_response(),
    }
    match state.db().acquire().await {
        Ok(mut conn) => {
            if let Err(e) =
                content_hub_store::repo::users::touch(&mut conn, &identity.subject, &identity.display_name).await
            {
                return ApiError::from(e).into_response();
            }
        }
        Err(e) => return ApiError::from(e).into_response(),
    }
    req.extensions_mut().insert(identity);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn csrf_requires_the_header_and_the_origin_on_writes_only() {
        let origin = "https://hub.example.org";
        let mut h = HeaderMap::new();
        assert!(csrf_ok(&Method::GET, &h, origin));
        assert!(!csrf_ok(&Method::POST, &h, origin));
        h.insert(CSRF_HEADER, HeaderValue::from_static("1"));
        assert!(!csrf_ok(&Method::POST, &h, origin), "no Origin");
        h.insert("origin", HeaderValue::from_static("https://evil.example"));
        assert!(!csrf_ok(&Method::PATCH, &h, origin));
        h.insert("origin", HeaderValue::from_static("https://hub.example.org"));
        assert!(csrf_ok(&Method::DELETE, &h, origin));
    }
}
