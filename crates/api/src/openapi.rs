//! The OpenAPI documents, built from the route tables (routes.rs) so the contract, the routers and the tests share
//! one list. Schemas come from the response and request types (`utoipa::ToSchema`); a test checks that every route
//! of each table is described.

use utoipa::OpenApi;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItem, PathsBuilder};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Type};
use utoipa::openapi::{ContentBuilder, InfoBuilder, OpenApiBuilder, Ref, Required, ResponseBuilder};

use crate::routes::{ADMIN_ROUTES, Method, PROBE_ROUTES, PUBLIC_ROUTES, Route};

#[derive(OpenApi)]
#[openapi(components(schemas(
    crate::error::ErrorBody,
    crate::admin::Me,
    crate::admin::Ceilings,
    content_hub_store::repo::phases::Phase,
    content_hub_store::repo::items::Item,
    content_hub_store::repo::tasks::Task,
    content_hub_store::repo::tasks::TaskPlatform,
    content_hub_store::repo::tasks::TaskLink,
    content_hub_store::repo::posts::Post,
    content_hub_store::repo::posts::PostRevision,
    content_hub_store::repo::instances::Instance,
    content_hub_store::repo::instances::Attempt,
    content_hub_store::repo::platforms::PlatformDefinition,
    content_hub_store::repo::targets::PlatformTarget,
    content_hub_store::repo::budgets::SpendCap,
    content_hub_store::repo::budgets::Usage,
    content_hub_store::repo::stats::Stats,
    content_hub_store::repo::stats::CountBy,
    content_hub_store::repo::audit::AuditEntry,
    crate::admin::roadmap::NewPhase,
    crate::admin::roadmap::PhasePatch,
    crate::admin::roadmap::NewItem,
    crate::admin::roadmap::ItemPatch,
    crate::admin::roadmap::ItemDetail,
    crate::admin::tasks::NewTask,
    crate::admin::tasks::TaskPatch,
    crate::admin::tasks::TaskTransition,
    crate::admin::tasks::NewLink,
    crate::admin::tasks::TaskDetail,
    crate::admin::tasks::PlatformRef,
    crate::admin::posts::NewPost,
    crate::admin::posts::PostPatch,
    crate::admin::posts::PostDetail,
    crate::admin::posts::Enqueue,
    crate::admin::posts::EnqueueTarget,
    crate::admin::posts::TargetProblem,
    crate::admin::publishing::InstanceDetail,
    crate::admin::publishing::InstanceMove,
    crate::admin::publishing::PlatformPatch,
    crate::admin::publishing::NewTarget,
    crate::admin::publishing::TargetPatch,
    crate::admin::ops::Budgets,
    crate::admin::ops::BudgetsInput,
    crate::admin::ops::CapInput,
    crate::admin::ops::Schedule,
    crate::admin::ops::Publishing,
    crate::public::Roadmap,
    content_hub_store::repo::public::PublicPhase,
    content_hub_store::repo::public::PublicItem,
)))]
struct AdminSchemas;

#[derive(OpenApi)]
#[openapi(components(schemas(
    crate::error::ErrorBody,
    crate::public::Roadmap,
    content_hub_store::repo::public::PublicPhase,
    content_hub_store::repo::public::PublicItem,
    content_hub_store::repo::public::PublicSnapshot,
    crate::public::SnapshotResponse,
)))]
struct PublicSchemas;

/// What a route returns.
enum Out {
    One(&'static str),
    List(&'static str),
    Page(&'static str),
    Text(&'static str),
    Empty,
}

/// Request and response of each route (by method and path).
fn shape(r: &Route) -> (Option<&'static str>, Out) {
    use Method::*;
    match (r.method, r.path) {
        (Get, "/healthz") | (Get, "/readyz") => (None, Out::Text("application/json")),
        (Get, "/public/v1/roadmap") | (Get, "/public/v1/roadmap-bot") => (None, Out::One("Roadmap")),
        (Get, "/public/v1/snapshot") => (None, Out::One("SnapshotResponse")),
        (Get, "/public/v1/feed.json") => (None, Out::Text("application/feed+json")),
        (Get, "/public/v1/feed.xml") => (None, Out::Text("application/rss+xml")),
        (Get, "/public/v1/openapi.json") | (Get, "/api/v1/openapi.json") => (None, Out::Text("application/json")),
        (Get, "/llms.txt") | (Get, "/robots.txt") => (None, Out::Text("text/plain")),
        (Get, "/api/v1/me") => (None, Out::One("Me")),
        (Get, "/api/v1/phases") => (None, Out::List("Phase")),
        (Post, "/api/v1/phases") => (Some("NewPhase"), Out::One("Phase")),
        (Get, "/api/v1/phases/{id}") => (None, Out::One("Phase")),
        (Patch, "/api/v1/phases/{id}") => (Some("PhasePatch"), Out::One("Phase")),
        (Get, "/api/v1/items") => (None, Out::Page("Item")),
        (Post, "/api/v1/items") => (Some("NewItem"), Out::One("Item")),
        (Get, "/api/v1/items/{id}") => (None, Out::One("ItemDetail")),
        (Patch, "/api/v1/items/{id}") => (Some("ItemPatch"), Out::One("Item")),
        (Get, "/api/v1/tasks") => (None, Out::Page("Task")),
        (Post, "/api/v1/tasks") => (Some("NewTask"), Out::One("TaskDetail")),
        (Get, "/api/v1/tasks/{id}") => (None, Out::One("TaskDetail")),
        (Patch, "/api/v1/tasks/{id}") => (Some("TaskPatch"), Out::One("TaskDetail")),
        (Post, "/api/v1/tasks/{id}/transition") => (Some("TaskTransition"), Out::One("TaskDetail")),
        (Post, "/api/v1/tasks/{id}/links") => (Some("NewLink"), Out::One("TaskLink")),
        (Delete, "/api/v1/tasks/{id}/links/{link_id}") => (None, Out::Empty),
        (Get, "/api/v1/posts") => (None, Out::Page("Post")),
        (Post, "/api/v1/posts") => (Some("NewPost"), Out::One("Post")),
        (Get, "/api/v1/posts/{id}") => (None, Out::One("PostDetail")),
        (Patch, "/api/v1/posts/{id}") => (Some("PostPatch"), Out::One("Post")),
        (Post, "/api/v1/posts/{id}/enqueue") => (Some("Enqueue"), Out::List("Instance")),
        (Get, "/api/v1/instances") => (None, Out::Page("Instance")),
        (Get, "/api/v1/instances/{id}") => (None, Out::One("InstanceDetail")),
        (Post, "/api/v1/instances/{id}/transition") => (Some("InstanceMove"), Out::One("InstanceDetail")),
        (Get, "/api/v1/platforms") => (None, Out::List("PlatformDefinition")),
        (Patch, "/api/v1/platforms/{id}") => (Some("PlatformPatch"), Out::One("PlatformDefinition")),
        (Get, "/api/v1/targets") => (None, Out::List("PlatformTarget")),
        (Post, "/api/v1/targets") => (Some("NewTarget"), Out::One("PlatformTarget")),
        (Patch, "/api/v1/targets/{id}") => (Some("TargetPatch"), Out::One("PlatformTarget")),
        (Get, "/api/v1/budgets") => (None, Out::One("Budgets")),
        (Put, "/api/v1/budgets") => (Some("BudgetsInput"), Out::One("Budgets")),
        (Get, "/api/v1/publishing") => (None, Out::One("Publishing")),
        (Put, "/api/v1/publishing") => (Some("Publishing"), Out::One("Publishing")),
        (Get, "/api/v1/preview/roadmap") => (None, Out::One("Roadmap")),
        (Get, "/api/v1/schedule") => (None, Out::One("Schedule")),
        (Get, "/api/v1/stats") => (None, Out::One("Stats")),
        (Get, "/api/v1/audit") => (None, Out::Page("AuditEntry")),
        _ => (None, Out::Empty),
    }
}

fn json_content(
    schema: impl Into<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>,
) -> utoipa::openapi::Content {
    ContentBuilder::new().schema(Some(schema)).build()
}

fn operation(r: &Route) -> utoipa::openapi::path::Operation {
    let (req, out) = shape(r);
    let ok = match out {
        Out::One(name) => ResponseBuilder::new()
            .description("OK")
            .content("application/json", json_content(Ref::from_schema_name(name)))
            .build(),
        Out::List(name) => ResponseBuilder::new()
            .description("OK")
            .content("application/json", json_content(ArrayBuilder::new().items(Ref::from_schema_name(name)).build()))
            .build(),
        Out::Page(name) => ResponseBuilder::new()
            .description("One page, newest first; pass next_cursor back as cursor")
            .content(
                "application/json",
                json_content(
                    ObjectBuilder::new()
                        .property("items", ArrayBuilder::new().items(Ref::from_schema_name(name)))
                        .required("items")
                        .property("next_cursor", ObjectBuilder::new().schema_type(Type::String))
                        .build(),
                ),
            )
            .build(),
        Out::Text(ct) => ResponseBuilder::new().description("OK").content(ct, ContentBuilder::new().build()).build(),
        Out::Empty => ResponseBuilder::new().description("Done").build(),
    };
    let status = match (r.method, &out) {
        (Method::Post, Out::One(_) | Out::List(_)) => "201",
        (_, Out::Empty) => "204",
        _ => "200",
    };
    let error = ResponseBuilder::new()
        .description("Error")
        .content("application/json", json_content(Ref::from_schema_name("ErrorBody")))
        .build();
    let mut op = OperationBuilder::new().summary(Some(r.purpose)).response(status, ok).response("default", error);
    for seg in r.path.split('/') {
        if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            op =
                op.parameter(
                    ParameterBuilder::new().name(name).parameter_in(ParameterIn::Path).required(Required::True).schema(
                        Some(ObjectBuilder::new().schema_type(Type::Integer).format(Some(
                            utoipa::openapi::SchemaFormat::KnownFormat(utoipa::openapi::KnownFormat::Int64),
                        ))),
                    ),
                );
        }
    }
    if let Some(name) = req {
        op = op.request_body(Some(
            RequestBodyBuilder::new()
                .content("application/json", json_content(Ref::from_schema_name(name)))
                .required(Some(Required::True))
                .build(),
        ));
    }
    op.build()
}

fn http(m: Method) -> HttpMethod {
    match m {
        Method::Get => HttpMethod::Get,
        Method::Post => HttpMethod::Post,
        Method::Put => HttpMethod::Put,
        Method::Patch => HttpMethod::Patch,
        Method::Delete => HttpMethod::Delete,
    }
}

fn build(
    title: &str,
    description: &str,
    routes: &[&[Route]],
    components: Option<utoipa::openapi::Components>,
) -> utoipa::openapi::OpenApi {
    let mut by_path: Vec<(&str, Vec<&Route>)> = Vec::new();
    for r in routes.iter().flat_map(|t| t.iter()) {
        match by_path.iter_mut().find(|(p, _)| *p == r.path) {
            Some((_, v)) => v.push(r),
            None => by_path.push((r.path, vec![r])),
        }
    }
    let mut paths = PathsBuilder::new();
    for (path, rs) in by_path {
        let mut item = PathItem::new(http(rs[0].method), operation(rs[0]));
        for r in &rs[1..] {
            item.merge_operations(PathItem::new(http(r.method), operation(r)));
        }
        paths = paths.path(path, item);
    }
    OpenApiBuilder::new()
        .info(InfoBuilder::new().title(title).version(env!("CARGO_PKG_VERSION")).description(Some(description)).build())
        .paths(paths.build())
        .components(components)
        .build()
}

/// The admin API (served at `/api/v1/openapi.json`, authenticated).
pub fn admin() -> utoipa::openapi::OpenApi {
    build(
        "Content hub admin API",
        "Behind Cloudflare Access: every request carries the Access JWT; state-changing requests also carry X-Content-Hub-Request and a same-origin Origin.",
        &[PROBE_ROUTES, ADMIN_ROUTES],
        AdminSchemas::openapi().components,
    )
}

/// The public API (served at `/public/v1/openapi.json`).
pub fn public() -> utoipa::openapi::OpenApi {
    build(
        "Content hub public API",
        "Read-only, cacheable (ETag, Cache-Control). Roadmap for people and for automated clients, and the owned feeds.",
        &[PROBE_ROUTES, PUBLIC_ROUTES],
        PublicSchemas::openapi().components,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_described_with_its_method() {
        for (doc, tables) in [(admin(), [PROBE_ROUTES, ADMIN_ROUTES]), (public(), [PROBE_ROUTES, PUBLIC_ROUTES])] {
            let v = serde_json::to_value(&doc).expect("json");
            for r in tables.iter().flat_map(|t| t.iter()) {
                let m = r.method.as_str().to_lowercase();
                assert!(v["paths"][r.path][&m].is_object(), "{} {} missing", r.method.as_str(), r.path);
            }
        }
    }

    #[test]
    fn every_referenced_schema_exists() {
        for doc in [admin(), public()] {
            let text = serde_json::to_string(&doc).expect("json");
            let v: serde_json::Value = serde_json::from_str(&text).expect("json");
            for part in text.split("\"$ref\":\"#/components/schemas/").skip(1) {
                let name = part.split('"').next().unwrap_or_default();
                assert!(v["components"]["schemas"][name].is_object(), "schema {name} is referenced but not defined");
            }
        }
    }

    #[test]
    fn the_public_document_names_no_admin_route() {
        let text = serde_json::to_string(&public()).expect("json");
        assert!(!text.contains("/api/v1"));
    }
}
