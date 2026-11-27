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
//! Two ways to sign in exist side by side while Cloudflare Access is being replaced (`ADMIN_AUTH`, [`AdminAuth`]): the Access token above, or a session made by
//! the platform's OpenID Connect provider ([`oidc`], routes in [`routes`]), whose role check is the policy decision point instead of Keto.
//!
//! A development mode (static JWKS file, static admin list) exists for local work; the configuration loader refuses
//! it in production (cli/main.rs).

/// Token verification.
pub mod jwt;
/// Sign-in through the platform's OpenID Connect provider.
pub mod oidc;
/// The `/auth/*` routes of that sign-in.
pub mod routes;

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

/// The shortest time between two key fetches triggered by an unknown key id.
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(30);
/// How long a fetched key set is trusted.
const JWKS_MAX_AGE: Duration = Duration::from_secs(3600);
/// How long an "is an admin" answer is reused.
const ROLE_ALLOWED_TTL: Duration = Duration::from_secs(30);
/// How long a "not an admin" answer is reused (shorter, so a new grant takes effect soon).
const ROLE_DENIED_TTL: Duration = Duration::from_secs(5);
/// The largest key set that is read.
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

/// How tokens are checked and roles looked up.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// The Access team domain, e.g. `https://team.cloudflareaccess.com` (the token's `iss`).
    pub issuer: String,
    /// The Access application's AUD tag.
    pub audience: String,
    /// Where the signing keys come from.
    pub jwks: JwksSource,
    /// Where roles come from.
    pub roles: RoleSource,
    /// The admin host's origin, e.g. `https://hub.example.org`.
    pub admin_origin: String,
}

impl AuthConfig {
    /// A configuration with no Access path at all (`ADMIN_AUTH=oidc`): an empty key set and no roles, so no Access token could ever verify.
    pub fn without_access(admin_origin: String) -> Self {
        AuthConfig {
            issuer: String::new(),
            audience: String::new(),
            jwks: JwksSource::Static(JwkSet { keys: Vec::new() }),
            roles: RoleSource::Static(BTreeSet::new()),
            admin_origin,
        }
    }
}

/// Which sign-in paths the admin process accepts (`ADMIN_AUTH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminAuth {
    /// Only the Cloudflare Access token (what ran before the migration; the safe rollback).
    Access,
    /// A session from the OpenID Connect provider is preferred; the Access token still works. The soak mode.
    Both,
    /// Only the OpenID Connect session; the Access token is ignored.
    Oidc,
}

impl AdminAuth {
    /// Read the setting's value (`access`, `both` or `oidc`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "access" => Some(Self::Access),
            "both" => Some(Self::Both),
            "oidc" => Some(Self::Oidc),
            _ => None,
        }
    }

    /// Whether a session cookie is looked at.
    pub fn accepts_session(self) -> bool {
        self != Self::Access
    }

    /// Whether the Access token is looked at.
    pub fn accepts_access(self) -> bool {
        self != Self::Oidc
    }
}

/// How the operator signed in (`GET /api/v1/me` shows it, so the soak week can see which path is in use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// The Cloudflare Access token.
    Access,
    /// A session made by the OpenID Connect provider.
    Oidc,
}

/// The authenticated operator, attached to the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The identity provider's subject.
    pub subject: String,
    /// The name shown.
    pub display_name: String,
    /// Which sign-in path was used.
    pub via: Via,
}

/// Verifies tokens and looks roles up, caching both.
pub struct Authenticator {
    /// The configuration.
    cfg: AuthConfig,
    /// The HTTP client for the key and role requests.
    http: reqwest::Client,
    /// The cached keys and when they were fetched.
    keys: RwLock<(JwkSet, Option<Instant>)>,
    /// When the keys were last fetched.
    last_refresh: Mutex<Option<Instant>>,
    /// Recent role answers by subject, with when they were made.
    roles: Mutex<HashMap<String, (bool, Instant)>>,
    /// Which sign-in paths are accepted.
    mode: AdminAuth,
    /// The OpenID Connect sign-in, when configured (always present for `Both` and `Oidc`).
    oidc: Option<Arc<oidc::Oidc>>,
}

/// The shape of Keto's answer to a check.
#[derive(Deserialize)]
struct KetoCheck {
    /// Whether the subject has the relation.
    allowed: bool,
}

impl Authenticator {
    /// Build an authenticator that accepts only the Access token (`ADMIN_AUTH=access`); its HTTP client has short timeouts and follows no redirects.
    pub fn new(cfg: AuthConfig) -> Result<Arc<Self>, ApiError> {
        Self::with_oidc(cfg, AdminAuth::Access, None)
    }

    /// Build an authenticator for a mode. `Both` and `Oidc` need the OpenID Connect sign-in; `Access` ignores it.
    pub fn with_oidc(cfg: AuthConfig, mode: AdminAuth, oidc: Option<Arc<oidc::Oidc>>) -> Result<Arc<Self>, ApiError> {
        if mode != AdminAuth::Access && oidc.is_none() {
            return Err(ApiError::Internal);
        }
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
            mode,
            oidc,
        }))
    }

    /// The accepted sign-in paths.
    pub fn mode(&self) -> AdminAuth {
        self.mode
    }

    /// The OpenID Connect sign-in, when this process has one.
    pub fn oidc(&self) -> Option<&Arc<oidc::Oidc>> {
        self.oidc.as_ref()
    }

    /// The admin host's origin.
    pub fn admin_origin(&self) -> &str {
        &self.cfg.admin_origin
    }

    /// Fetch the key set again (at most every 30 seconds unless `force`).
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
        Ok(Identity { subject: verified.subject, display_name: verified.display_name, via: Via::Access })
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
    /// The authenticator.
    fn authenticator(&self) -> &Arc<Authenticator>;
    /// The database pool.
    fn db(&self) -> &sqlx::PgPool;
}

/// The session cookie's value in the request's `Cookie` headers, if there is one.
pub(crate) fn cookie_of<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| meddleware_identity::session::cookie_value(h, name))
        .filter(|v| !v.is_empty())
}

/// The person behind a session cookie, when the OpenID Connect path is accepted and the browser sent a live session. Their role is the policy decision point's:
/// not a member is 403, no answer is 503. An unknown or expired session is no session (`None`), so the other path or a 401 follows.
async fn identify_by_session(
    auth: &Authenticator,
    db: &sqlx::PgPool,
    headers: &HeaderMap,
) -> Result<Option<Identity>, ApiError> {
    let Some(o) = auth.oidc().filter(|_| auth.mode().accepts_session()) else { return Ok(None) };
    let Some(id) = cookie_of(headers, o.cookie_name()) else { return Ok(None) };
    let Some(found) = o.identify(db, id).await? else { return Ok(None) };
    if o.is_member(&found.subject).await? {
        Ok(Some(found))
    } else {
        Err(ApiError::Forbidden("not an admin of the content hub"))
    }
}

/// The person behind a Cloudflare Access token, when that path is accepted and the header is present. Their role is Keto's.
async fn identify_by_access(auth: &Authenticator, headers: &HeaderMap) -> Result<Option<Identity>, ApiError> {
    if !auth.mode().accepts_access() {
        return Ok(None);
    }
    let Some(token) = headers.get(ACCESS_JWT_HEADER).and_then(|v| v.to_str().ok()) else { return Ok(None) };
    let found = auth.identify(token).await?;
    if auth.is_admin(&found.subject).await? {
        Ok(Some(found))
    } else {
        Err(ApiError::Forbidden("not an admin of the content hub"))
    }
}

/// The 401 answer. When the OpenID Connect path is accepted it also names where a sign-in starts (`signIn`), so the console can send the person there instead of
/// guessing; a process that accepts only Cloudflare Access says nothing extra (the edge does the sign-in).
fn unauthorized(auth: &Authenticator) -> Response {
    // The same body as every other 401, plus the hint only when this process has the routes.
    let mut body = serde_json::json!({ "error": "unauthorized", "message": ApiError::Unauthorized.to_string() });
    if auth.oidc().is_some() && auth.mode().accepts_session() {
        body["signIn"] = serde_json::json!("/auth/login");
    }
    (axum::http::StatusCode::UNAUTHORIZED, axum::Json(body)).into_response()
}

/// The middleware for every `/api/v1` route.
pub async fn require_admin<S: AuthState>(State(state): State<S>, mut req: Request, next: Next) -> Response {
    let auth = state.authenticator().clone();
    if !csrf_ok(req.method(), req.headers(), auth.admin_origin()) {
        return ApiError::Forbidden("cross-site request refused").into_response();
    }
    // A session from the OpenID Connect provider is preferred; the Access token is the other path, used when no live session came with the request.
    let identity = match identify_by_session(&auth, state.db(), req.headers()).await {
        Ok(Some(i)) => i,
        Ok(None) => match identify_by_access(&auth, req.headers()).await {
            Ok(Some(i)) => i,
            Ok(None) => return unauthorized(&auth),
            Err(e) => return e.into_response(),
        },
        Err(e) => return e.into_response(),
    };
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

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    /// Csrf requires the header and the origin on writes only.
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
