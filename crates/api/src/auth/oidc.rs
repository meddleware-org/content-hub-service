//! Sign-in through the platform's OpenID Connect provider (docs/auth/CONSOLE_SIGNIN.md in the platform workspace; the identity contract).
//!
//! The hub's admin process is the confidential OIDC client (a backend for the browser console): it sends the browser to the provider, receives the
//! code on `/auth/callback`, exchanges it with its own secret and the PKCE verifier, verifies the ID token (signature, issuer, audience, nonce, expiry
//! and the `phr` authentication class), asks the policy decision point whether the person is a member of the console, and only then makes a
//! server-side session. The browser holds nothing but a random session identifier in a `__Host-` cookie; the database holds its SHA-256.
//!
//! The pure checks live in the shared `meddleware-identity` crate. This module is the network, clock and storage around them.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use content_hub_store::repo::sessions;
use meddleware_identity::authzen::{self, DecisionCache};
use meddleware_identity::discovery::Discovery;
use meddleware_identity::idtoken::{self, IdClaims, IdOptions};
use meddleware_identity::jwk::JwkSet;
use meddleware_identity::keyset::KeyCache;
use meddleware_identity::logout::{self, LogoutOptions, ReplayCache};
use meddleware_identity::pkce::{self, Pending};
use meddleware_identity::{Error as IdentityError, jwt, session};
use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::{Mutex, RwLock};

use crate::auth::Identity;
use crate::error::ApiError;

/// The authentication class the console requires: a phishing-resistant key with user verification (`acr` of the contract).
pub const ACR: &str = "phr";
/// How long a sign-in may take between the redirect and the callback.
pub const LOGIN_MAX_AGE_SECS: i64 = 600;
/// How long the login-binding cookie lives (the same window).
const BINDING_COOKIE_SECS: u64 = 600;
/// How long a fetched discovery document is reused.
const DISCOVERY_MAX_AGE: Duration = Duration::from_secs(3600);
/// The largest token-endpoint answer that is read.
const MAX_TOKEN_RESPONSE: usize = 256 * 1024;
/// The name shown for a person whose token carries no preferred username.
const DEFAULT_DISPLAY_NAME: &str = "operator";

/// How the console signs people in.
#[derive(Clone)]
pub struct OidcConfig {
    /// The provider's issuer URL, exactly as its discovery document names itself.
    pub issuer: String,
    /// This console's client id at the provider.
    pub client_id: String,
    /// This console's client secret (never logged: see the `Debug` implementation).
    pub client_secret: String,
    /// The exact redirect URI registered for the client (`https://<host>/auth/callback`).
    pub redirect_uri: String,
    /// Where the provider sends the browser after sign-out.
    pub post_logout_uri: String,
    /// The in-cluster base URL that replaces the issuer's public origin for the console's own calls (token exchange and key set), or `None` to use the public URLs.
    pub internal_base: Option<String>,
    /// The policy decision point's base URL for the store, e.g. `http://openfga.auth.svc.cluster.local:8080/stores/<id>`.
    pub authz_base: String,
    /// The pre-shared key the policy decision point expects as a Bearer token, if any.
    pub authz_key: Option<String>,
    /// The resource id that membership is checked on (`console:<resource>`).
    pub resource: String,
    /// The session cookie's name (`__Host-` prefixed).
    pub cookie_name: String,
    /// The login-binding cookie's name (`__Host-` prefixed).
    pub binding_cookie_name: String,
    /// How long a session may sit unused.
    pub idle: Duration,
    /// How long a session may live in all.
    pub absolute: Duration,
    /// Development only: accept `http` URLs for the issuer and the endpoints.
    pub allow_insecure: bool,
}

impl fmt::Debug for OidcConfig {
    /// Everything except the two secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("resource", &self.resource)
            .field("authz_key", &self.authz_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// What the callback produces: the session cookie to set and the sign-in cookie to clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    /// The `Set-Cookie` value for the new session.
    pub session_cookie: String,
    /// The `Set-Cookie` value that clears the login-binding cookie.
    pub clear_binding: String,
}

/// The sign-in machinery: the provider's documents, key cache, replay cache and decision cache.
pub struct Oidc {
    /// The configuration.
    cfg: OidcConfig,
    /// The issuer's public origin (`https://host`), which `cfg.internal_base` replaces in the console's own calls.
    public_origin: String,
    /// The HTTP client: short timeouts, no redirects.
    http: reqwest::Client,
    /// The discovery document and when it was fetched.
    discovery: RwLock<Option<(Arc<Discovery>, Instant)>>,
    /// The provider's signing keys and the refresh rules.
    keys: Mutex<KeyCache>,
    /// Back-channel logout tokens already acted on.
    replay: Mutex<ReplayCache>,
    /// Recent membership decisions.
    decisions: Mutex<DecisionCache>,
}

/// The token endpoint's answer, reduced to what is used.
#[derive(Deserialize)]
struct TokenResponse {
    /// The ID token.
    id_token: String,
}

/// The origin (`scheme://host[:port]`) of a URL, or `None` for anything that is not an absolute http(s) URL.
fn origin_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    matches!(u.scheme(), "http" | "https").then(|| u.origin().ascii_serialization())
}

/// The current time, Unix seconds.
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The 503 for anything the provider or the policy decision point failed to supply.
fn unavailable() -> ApiError {
    ApiError::Unavailable("sign-in service unavailable")
}

impl Oidc {
    /// Build the sign-in machinery. Nothing is fetched yet: the provider is contacted at the first sign-in, so an outage of the provider does not stop the console starting.
    pub fn new(cfg: OidcConfig) -> Result<Arc<Self>, ApiError> {
        // One TLS stack for the process (idempotent), before any client is built.
        crate::auth::install_crypto_provider();
        // The cookie names must carry the prefix, or the browser would not enforce Secure, Path=/ and no Domain.
        if !cfg.cookie_name.starts_with("__Host-") || !cfg.binding_cookie_name.starts_with("__Host-") {
            return Err(ApiError::Internal);
        }
        // The origin the in-cluster base replaces in the provider's URLs; an issuer that is not an http(s) URL is a configuration error.
        let public_origin = origin_of(&cfg.issuer).ok_or(ApiError::Internal)?;
        // Short timeouts and no redirects: a slow or redirecting provider must become a 503, never a hung request or a followed redirect.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ApiError::Internal)?;
        // Everything starts empty: the discovery document, the keys and the caches fill on first use.
        Ok(Arc::new(Oidc {
            cfg,
            public_origin,
            http,
            discovery: RwLock::new(None),
            keys: Mutex::new(KeyCache::new()),
            replay: Mutex::new(ReplayCache::new()),
            decisions: Mutex::new(DecisionCache::new()),
        }))
    }

    /// The session cookie's name (the middleware reads it).
    pub fn cookie_name(&self) -> &str {
        &self.cfg.cookie_name
    }

    /// The login-binding cookie's name (the callback reads it).
    pub fn binding_cookie_name(&self) -> &str {
        &self.cfg.binding_cookie_name
    }

    /// A URL of the provider as the console itself must call it: the public origin replaced by the in-cluster base, when one is configured.
    fn internal(&self, url: &str) -> String {
        match (&self.cfg.internal_base, url.strip_prefix(&self.public_origin)) {
            (Some(base), Some(rest)) => format!("{}{rest}", base.trim_end_matches('/')),
            _ => url.to_owned(),
        }
    }

    /// The discovery document: cached for an hour, fetched again when older. The fetch goes to the in-cluster URL; the document must still name the configured issuer.
    async fn discovery(&self) -> Result<Arc<Discovery>, ApiError> {
        // A discovery document younger than an hour is reused.
        if let Some(d) = self
            .discovery
            .read()
            .await
            .as_ref()
            .filter(|(_, at)| at.elapsed() < DISCOVERY_MAX_AGE)
            .map(|(d, _)| d.clone())
        {
            return Ok(d);
        }
        // Where the document lives, as this process must reach it (in-cluster when configured).
        let url = self.internal(&format!("{}/.well-known/openid-configuration", self.cfg.issuer.trim_end_matches('/')));
        let body = self.get_bytes(&url, meddleware_identity::discovery::MAX_BYTES).await?;
        // The document must name the configured issuer, offer S256, send the issuer parameter and support back-channel logout, or it is refused.
        let d = Arc::new(Discovery::parse(&body, &self.cfg.issuer, self.cfg.allow_insecure).map_err(|e| {
            tracing::error!(reason = %e, "the provider's discovery document was refused");
            unavailable()
        })?);
        // Remember it with the time it was fetched.
        *self.discovery.write().await = Some((d.clone(), Instant::now()));
        Ok(d)
    }

    /// GET a URL and read at most `max` bytes of a successful answer.
    async fn get_bytes(&self, url: &str, max: usize) -> Result<Vec<u8>, ApiError> {
        let resp = self.http.get(url).send().await.map_err(|_| unavailable())?;
        // Anything but a success is "no answer"; the body of a failure is never read or logged.
        if !resp.status().is_success() {
            return Err(unavailable());
        }
        let body = resp.bytes().await.map_err(|_| unavailable())?;
        // A body above the limit is refused rather than parsed.
        if body.len() > max {
            return Err(unavailable());
        }
        Ok(body.to_vec())
    }

    /// The signing keys to verify a token with, fetching when the refresh rules say so (cold start, an hour old, or a key id the set lacks, never more than every 30 seconds).
    async fn keys_for(&self, token: &str) -> Result<JwkSet, ApiError> {
        // One clock reading for the whole decision.
        let at = now();
        // The key id the token names (none for a token that is not well formed; verification then fails later).
        let kid = jwt::key_id(token);
        // Read the cache under the lock, then release it before any network call.
        let (cached, refresh) = {
            let cache = self.keys.lock().await;
            let cached = cache.keys(at).cloned();
            let known = match (&cached, &kid) {
                (Some(set), Some(kid)) => set.find(kid).is_some(),
                _ => false,
            };
            (cached, cache.should_refresh(at, known))
        };
        // The cached set is good (or no refresh is allowed yet): use it, or fail when there is none.
        if !refresh {
            return cached.ok_or_else(unavailable);
        }
        // Fetch the keys: discovery names the key set's URL.
        let fetched = async {
            let d = self.discovery().await?;
            let body = self.get_bytes(&self.internal(&d.jwks_uri), meddleware_identity::jwk::MAX_JWKS_BYTES).await?;
            JwkSet::parse(&body).map_err(|_| unavailable())
        }
        .await;
        // Record the outcome in the cache.
        let mut cache = self.keys.lock().await;
        match fetched {
            Ok(set) => {
                cache.replace(set.clone(), at);
                Ok(set)
            }
            Err(e) => {
                // The old keys stay usable while they are young; the next attempt waits the minimum time.
                cache.failed(at);
                cached.ok_or(e)
            }
        }
    }

    /// Start a sign-in: record it, and return the provider URL to send the browser to and the cookie that binds the callback to this browser.
    pub async fn begin(&self, db: &PgPool) -> Result<(String, String), ApiError> {
        let d = self.discovery().await?;
        // Fresh state, nonce and PKCE verifier for this one sign-in.
        let p = Pending::new();
        // The binding value never leaves the browser except as a cookie; only its hash is stored.
        let binding = pkce::random_token(24);
        // The three secrets are stored (state and binding hashed) so the callback can find them again.
        let mut conn = db.acquire().await?;
        sessions::begin_login(
            &mut conn,
            &session::hash_id(&p.state),
            &session::hash_id(&binding),
            &p.nonce,
            &p.verifier,
        )
        .await?;
        // The provider's authorization endpoint with this client's request: code flow, PKCE S256, the nonce, and the key-with-verification class.
        let mut url = url::Url::parse(&d.authorization_endpoint).map_err(|_| unavailable())?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", &self.cfg.redirect_uri)
            .append_pair("scope", "openid")
            .append_pair("state", &p.state)
            .append_pair("nonce", &p.nonce)
            .append_pair("code_challenge", &p.challenge())
            .append_pair("code_challenge_method", "S256")
            .append_pair("acr_values", ACR);
        // The binding cookie: Secure, HttpOnly, SameSite=Lax (sent on the provider's redirect back), ten minutes.
        let cookie = session::set_cookie(&self.cfg.binding_cookie_name, &binding, BINDING_COOKIE_SECS)
            .ok_or(ApiError::Internal)?;
        Ok((url.into(), cookie))
    }

    /// Finish a sign-in from the callback's query (`code`, `state`, `iss`, or an `error`) and the browser's binding cookie value.
    pub async fn complete(
        &self,
        db: &PgPool,
        query: &[(String, String)],
        binding: Option<&str>,
    ) -> Result<SignedIn, ApiError> {
        // A query parameter by name.
        let param = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        // Without a state nothing can be matched to a sign-in.
        let state = param("state").filter(|s| !s.is_empty()).ok_or(ApiError::Unauthorized)?;
        // The state is consumed first, whatever happens next, so a code or state value works once.
        let mut conn = db.acquire().await?;
        let login = sessions::take_login(&mut conn, &session::hash_id(state), LOGIN_MAX_AGE_SECS)
            .await?
            .ok_or(ApiError::Unauthorized)?;
        // The callback must come from the browser that started the sign-in (otherwise an attacker could finish their own sign-in in the victim's browser).
        // Both sides are SHA-256 hashes of random secrets, so an ordinary comparison reveals nothing usable about the secret.
        let bound = binding.is_some_and(|b| session::hash_id(b) == login.binding_hash);
        if !bound {
            tracing::info!("sign-in refused: the callback did not come from the browser that started it");
            return Err(ApiError::Unauthorized);
        }
        // The provider reports a refusal (or the person cancelled) with an `error` parameter.
        if let Some(e) = param("error") {
            // The provider refused or the person cancelled; the code is never in the log.
            tracing::info!(error = %e.chars().take(64).collect::<String>(), "the provider ended the sign-in");
            return Err(ApiError::Unauthorized);
        }
        // RFC 9207: the response names its issuer, and it must be the configured one (the discovery check guarantees the provider sends it).
        if param("iss") != Some(self.cfg.issuer.as_str()) {
            tracing::info!("sign-in refused: the response names another issuer or none");
            return Err(ApiError::Unauthorized);
        }
        // The authorization code to exchange.
        let code = param("code").filter(|c| !c.is_empty()).ok_or(ApiError::Unauthorized)?;
        // Exchange the code, then verify the ID token it returns.
        let id_token = self.exchange(code, &login.verifier).await?;
        let claims = self.verify(&id_token, &login.nonce).await?;
        // Membership is decided before any session exists.
        if !self.is_member(&claims.subject).await? {
            tracing::info!("sign-in refused: not a member of the console");
            return Err(ApiError::Forbidden("not an admin of the content hub"));
        }
        // A new identifier at every sign-in, so one planted before sign-in is worthless.
        let (id, hash) = session::new_session();
        // The name shown is the provider's preferred username (display only, never used for authorisation).
        let name = if claims.username.is_empty() { DEFAULT_DISPLAY_NAME } else { claims.username.as_str() };
        let name: String = name.chars().take(200).collect();
        sessions::create_session(&mut conn, &hash, &claims.subject, &claims.session_id, &name, &id_token).await?;
        // The cookie lives as long as the session's absolute limit; the database enforces the real limits.
        let max_age = self.cfg.absolute.as_secs();
        Ok(SignedIn {
            session_cookie: session::set_cookie(&self.cfg.cookie_name, &id, max_age).ok_or(ApiError::Internal)?,
            clear_binding: session::clear_cookie(&self.cfg.binding_cookie_name).ok_or(ApiError::Internal)?,
        })
    }

    /// Exchange the authorization code for tokens at the token endpoint, with the client secret and the PKCE verifier; returns the ID token.
    async fn exchange(&self, code: &str, verifier: &str) -> Result<String, ApiError> {
        let d = self.discovery().await?;
        // The token request body: the code, the redirect URI it was issued for, and the PKCE verifier.
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &self.cfg.redirect_uri)
            .append_pair("code_verifier", verifier)
            .finish();
        // The client authenticates with its secret (HTTP Basic, client_secret_basic); the call goes to the in-cluster address.
        let resp = self
            .http
            .post(self.internal(&d.token_endpoint))
            .basic_auth(&self.cfg.client_id, Some(&self.cfg.client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| unavailable())?;
        // Read the answer once, bounded.
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|_| unavailable())?;
        if status.is_server_error() || bytes.len() > MAX_TOKEN_RESPONSE {
            return Err(unavailable());
        }
        if !status.is_success() {
            // A refused code (expired, reused, wrong verifier) is the person's problem, not an outage.
            tracing::info!(status = status.as_u16(), "the token endpoint refused the code");
            return Err(ApiError::Unauthorized);
        }
        Ok(serde_json::from_slice::<TokenResponse>(&bytes).map_err(|_| unavailable())?.id_token)
    }

    /// Verify an ID token against the provider's keys, for this client and this sign-in's nonce, requiring the `phr` class.
    async fn verify(&self, token: &str, nonce: &str) -> Result<IdClaims, ApiError> {
        let keys = self.keys_for(token).await?;
        // What this sign-in expects of the token.
        let o = IdOptions {
            issuer: &self.cfg.issuer,
            client_id: &self.cfg.client_id,
            nonce,
            accepted_acr: &[ACR],
            now: now(),
        };
        idtoken::verify_id_token(token, &keys, &o).map_err(|e| {
            tracing::info!(reason = %e, "the ID token was refused");
            ApiError::Unauthorized
        })
    }

    /// Whether the subject is a member of this console, according to the policy decision point (AuthZEN). Cached 30 s when allowed and 5 s when denied; no answer is a 503, never an allow.
    pub async fn is_member(&self, subject: &str) -> Result<bool, ApiError> {
        // The question: may this user be a `member` of this console?
        let req = authzen::Request::new(("user", subject), ("console", &self.cfg.resource), "member");
        if let Some(d) = self.decisions.lock().await.get(&req, now()) {
            return Ok(d);
        }
        // One Access Evaluation request (AuthZEN) to the policy decision point.
        let mut call = self
            .http
            .post(format!("{}/access/v1/evaluation", self.cfg.authz_base.trim_end_matches('/')))
            .header("content-type", "application/json")
            .body(req.body());
        // The pre-shared key goes as a Bearer token when one is configured.
        if let Some(key) = &self.cfg.authz_key {
            call = call.bearer_auth(key);
        }
        let resp = call.send().await.map_err(|_| ApiError::Unavailable("authorisation service unavailable"))?;
        // Only HTTP 200 with a boolean `decision` counts; everything else is "no answer" (503).
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|_| ApiError::Unavailable("authorisation service unavailable"))?;
        let decision = authzen::parse_decision(status, &bytes)
            .map_err(|_| ApiError::Unavailable("authorisation service unavailable"))?;
        // Remember it briefly (allow 30 s, deny 5 s).
        self.decisions.lock().await.put(&req, decision, now());
        Ok(decision)
    }

    /// The person behind a session cookie, if the session exists and is inside both lifetimes (this marks it used).
    pub async fn identify(&self, db: &PgPool, cookie_id: &str) -> Result<Option<Identity>, ApiError> {
        // Look the session up by the hash of the cookie's value; both lifetimes are applied by the query.
        let mut conn = db.acquire().await?;
        let found = sessions::use_session(
            &mut conn,
            &session::hash_id(cookie_id),
            self.cfg.idle.as_secs() as i64,
            self.cfg.absolute.as_secs() as i64,
        )
        .await?;
        Ok(found.map(|s| Identity { subject: s.subject, display_name: s.display_name, via: crate::auth::Via::Oidc }))
    }

    /// End the session of a cookie, and return the URL that ends the provider's session too (or the console's own page when there was no session).
    pub async fn sign_out(&self, db: &PgPool, cookie_id: Option<&str>) -> Result<(String, String), ApiError> {
        // The cookie is cleared whether or not a session was found.
        let clear = session::clear_cookie(&self.cfg.cookie_name).ok_or(ApiError::Internal)?;
        let ended = match cookie_id {
            Some(id) => {
                let mut conn = db.acquire().await?;
                sessions::end_session(&mut conn, &session::hash_id(id)).await?
            }
            None => None,
        };
        // No session: nothing to end at the provider either; send the person to the console's own page.
        let Some(s) = ended else { return Ok((self.cfg.post_logout_uri.clone(), clear)) };
        // The provider's own session ends through its logout endpoint; the ID token proves who is leaving.
        let d = self.discovery().await?;
        let mut url = url::Url::parse(&d.end_session_endpoint).map_err(|_| unavailable())?;
        url.query_pairs_mut()
            .append_pair("id_token_hint", &s.id_token)
            .append_pair("post_logout_redirect_uri", &self.cfg.post_logout_uri)
            .append_pair("client_id", &self.cfg.client_id);
        Ok((url.into(), clear))
    }

    /// Act on a back-channel logout token from the provider: verify it, then end the named session or all of the person's sessions. Returns how many ended.
    pub async fn back_channel_logout(&self, db: &PgPool, token: &str) -> Result<u64, ApiError> {
        let keys = self.keys_for(token).await?;
        // What this client expects of a logout token.
        let o = LogoutOptions { issuer: &self.cfg.issuer, client_id: &self.cfg.client_id, now: now() };
        // The replay cache is held only while the token is verified, so a token is acted on once.
        let claims = {
            let mut replay = self.replay.lock().await;
            logout::verify_logout_token(token, &keys, &o, &mut replay).map_err(|e| {
                tracing::info!(reason = %e, "a logout token was refused");
                match e {
                    IdentityError::Replay => ApiError::BadRequest("logout token already used".into()),
                    _ => ApiError::BadRequest("logout token refused".into()),
                }
            })?
        };
        // A token naming a session ends that session; one naming only the person ends all of theirs.
        let mut conn = db.acquire().await?;
        if claims.session_id.is_empty() {
            Ok(sessions::end_by_subject(&mut conn, &claims.subject).await?)
        } else {
            Ok(sessions::end_by_provider_session(&mut conn, &claims.session_id).await?)
        }
    }

    /// Delete expired sign-ins and sessions once.
    pub async fn purge(&self, db: &PgPool) -> Result<u64, ApiError> {
        let mut conn = db.acquire().await?;
        Ok(sessions::purge(
            &mut conn,
            LOGIN_MAX_AGE_SECS,
            self.cfg.idle.as_secs() as i64,
            self.cfg.absolute.as_secs() as i64,
        )
        .await?)
    }
}
