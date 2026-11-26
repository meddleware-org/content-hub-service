//! The OpenAPI documents, built from the route tables (routes.rs) so the contract, the routers and the tests share
//! one list. Schemas come from the response and request types (`utoipa::ToSchema`); a test checks that every route
//! of each table is described.

use utoipa::OpenApi;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItem, PathsBuilder};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Type};
use utoipa::openapi::{ContentBuilder, InfoBuilder, OpenApiBuilder, Ref, Required, ResponseBuilder};

// The route tables are the single list the documents are built from.
use crate::routes::{ADMIN_ROUTES, Method, PROBE_ROUTES, PUBLIC_ROUTES, Route};

// The shared component schemas (response and request bodies) are registered once here; the paths come from the route tables.
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
/// The schemas the admin document adds to the route table.
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
/// The schemas the public document adds to the route table.
struct PublicSchemas;

/// What a route returns.
enum Out {
    /// A single JSON object of this schema.
    One(&'static str),
    /// A JSON array of this schema.
    List(&'static str),
    /// A page of this schema.
    Page(&'static str),
    /// Plain text of this content type.
    Text(&'static str),
    /// No body.
    Empty,
}

/// Request and response of each route (by method and path).
fn shape(r: &Route) -> (Option<&'static str>, Out) {
    // The method names are used bare in the table below.
    use Method::*;
    // One row per route: the request body schema, if any, and the response shape.
    match (r.method, r.path) {
        // The probes answer small JSON documents.
        (Get, "/healthz") | (Get, "/readyz") => (None, Out::Text("application/json")),
        // Both audiences get the same document shape.
        (Get, "/public/v1/roadmap") | (Get, "/public/v1/roadmap-bot") => (None, Out::One("Roadmap")),
        (Get, "/public/v1/snapshot") => (None, Out::One("SnapshotResponse")),
        // The feeds are not described field by field: they follow JSON Feed and RSS.
        (Get, "/public/v1/feed.json") => (None, Out::Text("application/feed+json")),
        (Get, "/public/v1/feed.xml") => (None, Out::Text("application/rss+xml")),
        (Get, "/public/v1/openapi.json") | (Get, "/api/v1/openapi.json") => (None, Out::Text("application/json")),
        // The two machine files are plain text.
        (Get, "/llms.txt") | (Get, "/robots.txt") => (None, Out::Text("text/plain")),
        (Get, "/api/v1/me") => (None, Out::One("Me")),
        // Phases are few, so the list is not paged.
        (Get, "/api/v1/phases") => (None, Out::List("Phase")),
        // A create returns the new row (201).
        (Post, "/api/v1/phases") => (Some("NewPhase"), Out::One("Phase")),
        (Get, "/api/v1/phases/{id}") => (None, Out::One("Phase")),
        (Patch, "/api/v1/phases/{id}") => (Some("PhasePatch"), Out::One("Phase")),
        // Items can be many, so the list is paged.
        (Get, "/api/v1/items") => (None, Out::Page("Item")),
        (Post, "/api/v1/items") => (Some("NewItem"), Out::One("Item")),
        (Get, "/api/v1/items/{id}") => (None, Out::One("ItemDetail")),
        (Patch, "/api/v1/items/{id}") => (Some("ItemPatch"), Out::One("Item")),
        // Tasks are paged.
        (Get, "/api/v1/tasks") => (None, Out::Page("Task")),
        (Post, "/api/v1/tasks") => (Some("NewTask"), Out::One("TaskDetail")),
        (Get, "/api/v1/tasks/{id}") => (None, Out::One("TaskDetail")),
        (Patch, "/api/v1/tasks/{id}") => (Some("TaskPatch"), Out::One("TaskDetail")),
        // A move returns the task with its dependencies and links.
        (Post, "/api/v1/tasks/{id}/transition") => (Some("TaskTransition"), Out::One("TaskDetail")),
        (Post, "/api/v1/tasks/{id}/links") => (Some("NewLink"), Out::One("TaskLink")),
        // A delete has no body (204).
        (Delete, "/api/v1/tasks/{id}/links/{link_id}") => (None, Out::Empty),
        // Posts are paged.
        (Get, "/api/v1/posts") => (None, Out::Page("Post")),
        (Post, "/api/v1/posts") => (Some("NewPost"), Out::One("Post")),
        (Get, "/api/v1/posts/{id}") => (None, Out::One("PostDetail")),
        (Patch, "/api/v1/posts/{id}") => (Some("PostPatch"), Out::One("Post")),
        // Queueing returns the instances it made.
        (Post, "/api/v1/posts/{id}/enqueue") => (Some("Enqueue"), Out::List("Instance")),
        // Instances are paged.
        (Get, "/api/v1/instances") => (None, Out::Page("Instance")),
        (Get, "/api/v1/instances/{id}") => (None, Out::One("InstanceDetail")),
        (Post, "/api/v1/instances/{id}/transition") => (Some("InstanceMove"), Out::One("InstanceDetail")),
        // Platform definitions are a short list.
        (Get, "/api/v1/platforms") => (None, Out::List("PlatformDefinition")),
        (Patch, "/api/v1/platforms/{id}") => (Some("PlatformPatch"), Out::One("PlatformDefinition")),
        // Targets are a short list.
        (Get, "/api/v1/targets") => (None, Out::List("PlatformTarget")),
        (Post, "/api/v1/targets") => (Some("NewTarget"), Out::One("PlatformTarget")),
        (Patch, "/api/v1/targets/{id}") => (Some("TargetPatch"), Out::One("PlatformTarget")),
        // The caps, the ceilings and the month's use.
        (Get, "/api/v1/budgets") => (None, Out::One("Budgets")),
        // Budgets are replaced as a whole.
        (Put, "/api/v1/budgets") => (Some("BudgetsInput"), Out::One("Budgets")),
        // The stop switch.
        (Get, "/api/v1/publishing") => (None, Out::One("Publishing")),
        (Put, "/api/v1/publishing") => (Some("Publishing"), Out::One("Publishing")),
        (Get, "/api/v1/preview/roadmap") => (None, Out::One("Roadmap")),
        (Get, "/api/v1/schedule") => (None, Out::One("Schedule")),
        (Get, "/api/v1/stats") => (None, Out::One("Stats")),
        // The audit log is paged, newest first.
        (Get, "/api/v1/audit") => (None, Out::Page("AuditEntry")),
        // A route not listed is described as having no body; a test fails if any table route is missing from the document.
        _ => (None, Out::Empty),
    }
}

/// A JSON media type for a schema.
fn json_content(
    schema: impl Into<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>,
) -> utoipa::openapi::Content {
    ContentBuilder::new().schema(Some(schema)).build()
}

/// The OpenAPI operation for a route: its request, its response and its error answers.
fn operation(r: &Route) -> utoipa::openapi::path::Operation {
    // The request and response shape of the route.
    let (req, out) = shape(r);
    let ok = match out {
        // A single object of the named schema.
        Out::One(name) => ResponseBuilder::new()
            .description("OK")
            .content("application/json", json_content(Ref::from_schema_name(name)))
            .build(),
        // An array of the named schema.
        Out::List(name) => ResponseBuilder::new()
            .description("OK")
            .content("application/json", json_content(ArrayBuilder::new().items(Ref::from_schema_name(name)).build()))
            .build(),
        // A page: `items` and an optional `next_cursor`.
        Out::Page(name) => ResponseBuilder::new()
            .description("One page, newest first; pass next_cursor back as cursor")
            .content(
                "application/json",
                json_content(
                    ObjectBuilder::new()
                        // The rows on the page.
                        .property("items", ArrayBuilder::new().items(Ref::from_schema_name(name)))
                        .required("items")
                        // Absent on the last page.
                        .property("next_cursor", ObjectBuilder::new().schema_type(Type::String))
                        .build(),
                ),
            )
            .build(),
        // A non-JSON answer: only its media type is described.
        Out::Text(ct) => ResponseBuilder::new().description("OK").content(ct, ContentBuilder::new().build()).build(),
        Out::Empty => ResponseBuilder::new().description("Done").build(),
    };
    // A create answers 201, a delete 204, everything else 200.
    let status = match (r.method, &out) {
        (Method::Post, Out::One(_) | Out::List(_)) => "201",
        (_, Out::Empty) => "204",
        _ => "200",
    };
    // Every route can answer with the error body.
    let error = ResponseBuilder::new()
        .description("Error")
        .content("application/json", json_content(Ref::from_schema_name("ErrorBody")))
        .build();
    let mut op = OperationBuilder::new().summary(Some(r.purpose)).response(status, ok).response("default", error);
    // Each `{name}` segment of the path is an integer path parameter.
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
    // A request body, when the route takes one, is required.
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

/// The OpenAPI method of a route method.
fn http(m: Method) -> HttpMethod {
    match m {
        Method::Get => HttpMethod::Get,
        Method::Post => HttpMethod::Post,
        Method::Put => HttpMethod::Put,
        Method::Patch => HttpMethod::Patch,
        Method::Delete => HttpMethod::Delete,
    }
}

/// Build an OpenAPI document from route tables.
fn build(
    title: &str,
    description: &str,
    routes: &[&[Route]],
    components: Option<utoipa::openapi::Components>,
) -> utoipa::openapi::OpenApi {
    // Group the routes by path: one path item holds every method on it.
    let mut by_path: Vec<(&str, Vec<&Route>)> = Vec::new();
    for r in routes.iter().flat_map(|t| t.iter()) {
        match by_path.iter_mut().find(|(p, _)| *p == r.path) {
            // Another method on a path already seen.
            Some((_, v)) => v.push(r),
            // The first method on a new path.
            None => by_path.push((r.path, vec![r])),
        }
    }
    let mut paths = PathsBuilder::new();
    for (path, rs) in by_path {
        // The first route makes the path item; the others are merged into it.
        let mut item = PathItem::new(http(rs[0].method), operation(rs[0]));
        for r in &rs[1..] {
            item.merge_operations(PathItem::new(http(r.method), operation(r)));
        }
        paths = paths.path(path, item);
    }
    OpenApiBuilder::new()
        // The document's version is the crate's, so it changes with each release.
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
        // The admin document describes the admin routes and the probes.
        &[PROBE_ROUTES, ADMIN_ROUTES],
        AdminSchemas::openapi().components,
    )
}

/// The public API (served at `/public/v1/openapi.json`).
pub fn public() -> utoipa::openapi::OpenApi {
    build(
        "Content hub public API",
        "Read-only, cacheable (ETag, Cache-Control). Roadmap for people and for automated clients, and the owned feeds.",
        // The public document describes the public routes and the probes.
        &[PROBE_ROUTES, PUBLIC_ROUTES],
        PublicSchemas::openapi().components,
    )
}

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use super::*;

    /// Every route is described with its method.
    #[test]
    fn every_route_is_described_with_its_method() {
        // Both documents against their own tables.
        for (doc, tables) in [(admin(), [PROBE_ROUTES, ADMIN_ROUTES]), (public(), [PROBE_ROUTES, PUBLIC_ROUTES])] {
            let v = serde_json::to_value(&doc).expect("json");
            for r in tables.iter().flat_map(|t| t.iter()) {
                let m = r.method.as_str().to_lowercase();
                // A route that exists in the router but not in the document fails here.
                assert!(v["paths"][r.path][&m].is_object(), "{} {} missing", r.method.as_str(), r.path);
            }
        }
    }

    /// Every referenced schema exists.
    #[test]
    fn every_referenced_schema_exists() {
        for doc in [admin(), public()] {
            let text = serde_json::to_string(&doc).expect("json");
            let v: serde_json::Value = serde_json::from_str(&text).expect("json");
            // Find every schema reference by splitting on the reference prefix.
            for part in text.split("\"$ref\":\"#/components/schemas/").skip(1) {
                let name = part.split('"').next().unwrap_or_default();
                // A reference to a schema that was not registered would break every client generator.
                assert!(v["components"]["schemas"][name].is_object(), "schema {name} is referenced but not defined");
            }
        }
    }

    /// The public document names no admin route.
    #[test]
    fn the_public_document_names_no_admin_route() {
        let text = serde_json::to_string(&public()).expect("json");
        assert!(!text.contains("/api/v1"));
    }
}
