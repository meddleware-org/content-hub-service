//! The `/auth/*` routes of the OpenID Connect sign-in. They sit outside the `/api/v1` admin check (a person who is not signed in must reach them) and
//! exist only when the process was configured with a provider (`ADMIN_AUTH` is `both` or `oidc`); otherwise every one answers 404.
//!
//! - `GET /auth/login`: start a sign-in (redirect to the provider, plus the cookie that binds the callback to this browser).
//! - `GET /auth/callback`: finish it (the code, the state and the issuer arrive here), make the session, redirect to the console.
//! - `POST /auth/logout`: end the session; the answer names the provider's logout URL for the console to open. CSRF-checked like every write.
//! - `POST /auth/backchannel-logout`: the provider tells the console a session ended (a signed logout token). Never routed through the public ingress.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, State};
use axum::http::header::{CACHE_CONTROL, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use crate::auth::oidc::Oidc;
use crate::auth::{AuthState, cookie_of, csrf_ok};
use crate::error::ApiError;

/// The four routes; merged into the admin router.
pub fn router<S: AuthState>() -> Router<S> {
    Router::new()
        .route("/auth/login", get(login::<S>))
        .route("/auth/callback", get(callback::<S>))
        .route("/auth/logout", post(logout::<S>))
        .route("/auth/backchannel-logout", post(backchannel_logout::<S>))
}

/// The sign-in machinery of this process, or a 404 when the process has none (so the routes do not exist in `access` mode).
fn oidc_of<S: AuthState>(state: &S) -> Result<&std::sync::Arc<Oidc>, ApiError> {
    let auth = state.authenticator();
    auth.oidc().filter(|_| auth.mode().accepts_session()).ok_or(ApiError::NotFound)
}

/// A response with a status, a `Cache-Control: no-store` header and these cookies (none of this is ever cached).
fn plain(status: StatusCode, location: Option<&str>, cookies: &[&str], body: Body) -> Response {
    let mut b = Response::builder().status(status).header(CACHE_CONTROL, "no-store");
    if let Some(l) = location {
        b = b.header(LOCATION, l);
    }
    for c in cookies {
        b = b.header(SET_COOKIE, *c);
    }
    b.body(body).unwrap_or_else(|_| ApiError::Internal.into_response())
}

/// `GET /auth/login`.
async fn login<S: AuthState>(State(state): State<S>) -> Result<Response, ApiError> {
    let o = oidc_of(&state)?;
    let (location, cookie) = o.begin(state.db()).await?;
    Ok(plain(StatusCode::FOUND, Some(&location), &[&cookie], Body::empty()))
}

/// `GET /auth/callback`.
async fn callback<S: AuthState>(
    State(state): State<S>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let o = oidc_of(&state)?;
    let pairs: Vec<(String, String)> =
        url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()).into_owned().collect();
    let binding = cookie_of(&headers, o.binding_cookie_name());
    let done = o.complete(state.db(), &pairs, binding).await?;
    // The console is at the root of this host; the session cookie is set and the sign-in cookie cleared in the same answer.
    Ok(plain(StatusCode::FOUND, Some("/"), &[&done.session_cookie, &done.clear_binding], Body::empty()))
}

/// `POST /auth/logout`: the console calls it with `fetch` (so the CSRF header and `Origin` are present) and then opens the URL in the answer.
async fn logout<S: AuthState>(State(state): State<S>, headers: HeaderMap) -> Result<Response, ApiError> {
    let o = oidc_of(&state)?;
    if !csrf_ok(&Method::POST, &headers, state.authenticator().admin_origin()) {
        return Err(ApiError::Forbidden("cross-site request refused"));
    }
    let (redirect, clear) = o.sign_out(state.db(), cookie_of(&headers, o.cookie_name())).await?;
    let body = json!({ "redirect": redirect }).to_string();
    let mut r = plain(StatusCode::OK, None, &[&clear], Body::from(body));
    r.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"));
    Ok(r)
}

/// `POST /auth/backchannel-logout`: the form field `logout_token` carries a signed token naming the session (or the person) that ended at the provider.
async fn backchannel_logout<S: AuthState>(State(state): State<S>, body: Bytes) -> Result<Response, ApiError> {
    let o = oidc_of(&state)?;
    let token = url::form_urlencoded::parse(&body)
        .find(|(k, _)| k == "logout_token")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| ApiError::BadRequest("logout_token is required".into()))?;
    let ended = o.back_channel_logout(state.db(), &token).await?;
    tracing::info!(ended, "back-channel logout");
    Ok(plain(StatusCode::OK, None, &[], Body::empty()))
}
