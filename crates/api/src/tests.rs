//! Route exposure: each router answers exactly its table, and nothing of the other.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

use crate::admin::{self, AdminState, Ceilings};
use crate::auth::{AuthConfig, Authenticator, JwksSource, RoleSource};
use crate::public::{self, PublicConfig, PublicState};
use crate::routes::{ADMIN_ROUTES, PROBE_ROUTES, PUBLIC_ROUTES, Route};

/// A pool that never reaches a database (port 1 refuses), for routes that must not need one.
fn dead_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .expect("lazy pool")
}

fn public_router() -> Router {
    public::router(PublicState { db: dead_pool(), config: std::sync::Arc::new(PublicConfig::default()) })
}

pub(crate) fn test_auth() -> std::sync::Arc<Authenticator> {
    Authenticator::new(AuthConfig {
        issuer: "https://team.cloudflareaccess.com".into(),
        audience: "aud-tag".into(),
        jwks: JwksSource::Static(crate::auth::jwt::testing::jwks("k1")),
        roles: RoleSource::Static(["admin-subject".to_owned()].into()),
        admin_origin: "https://hub.example.org".into(),
    })
    .expect("authenticator")
}

fn admin_router() -> Router {
    admin::router(AdminState { db: dead_pool(), auth: test_auth(), ceilings: Ceilings::default() })
}

/// "Served" means the route exists: anything but 404 (no route) and 405 (wrong method).
fn served(s: StatusCode) -> bool {
    s != StatusCode::NOT_FOUND && s != StatusCode::METHOD_NOT_ALLOWED
}

/// A concrete path for a route pattern (`{id}` -> `1`).
fn concrete(path: &str) -> String {
    path.split('/').map(|seg| if seg.starts_with('{') { "1" } else { seg }).collect::<Vec<_>>().join("/")
}

async fn status(router: &Router, route: &Route) -> StatusCode {
    let req = Request::builder()
        .method(route.method.as_str())
        .uri(concrete(route.path))
        .body(Body::empty())
        .expect("request");
    router.clone().oneshot(req).await.expect("infallible").status()
}

#[tokio::test]
async fn the_public_router_serves_its_table_and_no_admin_route() {
    let r = public_router();
    for route in PUBLIC_ROUTES {
        assert!(served(status(&r, route).await), "{route:?}");
    }
    for route in ADMIN_ROUTES {
        assert_eq!(status(&r, route).await, StatusCode::NOT_FOUND, "admin route reachable on public: {route:?}");
    }
}

#[tokio::test]
async fn the_admin_router_serves_its_table_and_no_public_route() {
    let r = admin_router();
    for route in ADMIN_ROUTES {
        // Without the Access token every admin route answers 401 (403 for writes: the CSRF check comes first).
        let s = status(&r, route).await;
        let want = if route.method.is_state_changing() { StatusCode::FORBIDDEN } else { StatusCode::UNAUTHORIZED };
        assert_eq!(s, want, "{route:?}");
    }
    for route in PUBLIC_ROUTES {
        assert_eq!(status(&r, route).await, StatusCode::NOT_FOUND, "public route on admin: {route:?}");
    }
}

#[tokio::test]
async fn the_public_router_refuses_every_write_method() {
    let r = public_router();
    for route in PUBLIC_ROUTES {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let req = Request::builder().method(method).uri(route.path).body(Body::empty()).expect("request");
            let s = r.clone().oneshot(req).await.expect("infallible").status();
            assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED, "{method} {}", route.path);
        }
    }
}

#[tokio::test]
async fn probes_answer_on_both_without_and_with_a_database() {
    for r in [public_router(), admin_router()] {
        for route in PROBE_ROUTES {
            let s = status(&r, route).await;
            match route.path {
                "/healthz" => assert_eq!(s, StatusCode::OK),
                // No database is reachable here: not ready, but answering.
                _ => assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE),
            }
        }
    }
}

#[tokio::test]
async fn unknown_paths_get_a_json_404() {
    let req = Request::builder().uri("/api/v2/anything").body(Body::empty()).expect("request");
    let res = public_router().oneshot(req).await.expect("infallible");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(res.into_body(), 4096).await.expect("body");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["error"], "not_found");
}
